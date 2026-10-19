//! Local library storage and the end-to-end acquisition pipeline (#29 partial, #26 partial).
//!
//! Layout under a library root:
//!
//! ```text
//! library/
//!   objects/<sha256[0..2]>/<sha256>.pdf   content-addressed, no-clobber, atomic rename
//!   manifests/<source>-<id>-<sha12>.json  provenance for each acquired object
//!   quarantine/                           rejected payloads + typed reason manifests
//!   reports/<unix>-acquire.jsonl          per-run acquisition reports
//!   tmp/                                  scratch for in-flight transfers (never durable)
//!   extracted/<sha[0..2]>/<sha>.txt       extracted text artifacts (#31, src/extract.rs)
//!   extraction/<sha>.json                 extraction provenance records (#31)
//! ```
//!
//! A download is reported as acquired only after the payload passed structural PDF
//! validation and the object plus its provenance manifest were finalized. Interrupted or
//! invalid transfers leave at most a scratch file; they never appear under `objects/`.
//!
//! # Crash safety of finalization
//!
//! The payload is fsynced by the downloader before `finalize_object` runs. Publication
//! then crosses two boundaries, each crash-safe:
//!
//! 1. **Object rename**: scratch → `objects/<sha>.pdf` by atomic same-filesystem rename,
//!    followed by a parent-directory sync. A crash before the rename leaves only scratch
//!    under `tmp/`, which the next run clears; the object never exists half-written.
//! 2. **Manifest publication**: provenance is written to a `.pending` sibling, fsynced,
//!    and renamed into place. A crash can only leave a complete manifest or a `.pending`
//!    file that readers ignore — never a torn JSON document.
//!
//! A crash *between* the two boundaries leaves an unattributed object: validated,
//! content-addressed bytes with no provenance yet. [`LibraryLayout::unattributed_objects`]
//! surfaces exactly those so an interrupted run is visible as incomplete rather than
//! silently counted as an acquisition; re-acquiring the record deduplicates against the
//! existing object and writes the missing manifest.

use crate::fetcher::pdf_validation::{
    validate_or_quarantine_pdf, PdfValidationConfig, PdfValidationContext, PdfValidationOutcome,
};
use crate::fetcher::stream::{DownloadConfig, DownloadError, DownloadResult, ResumableDownloader};
use crate::jobs::{
    classify_download_error, lease_expiry, lease_owner, plan_job_action, JobAction, JobErrorClass,
    JobLedger, JobPolicy, JobRecord, JobState,
};
use crate::sources::work_record::WorkRecord;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;
use url::Url;

pub const LIBRARY_SCHEMA_VERSION: u16 = 1;
const MAX_MANIFEST_STEM_BYTES: usize = 96;

#[derive(Debug, Error)]
pub enum LibraryError {
    #[error("filesystem operation failed: {0}")]
    Io(#[from] io::Error),
    #[error("manifest serialization failed: {0}")]
    Manifest(#[from] serde_json::Error),
    #[error("invalid library layout: {0}")]
    InvalidLayout(String),
    #[error("invalid acquisition configuration: {0}")]
    InvalidConfig(String),
}

/// Typed rejection from the pre-request policy gate. The gate is re-evaluated immediately
/// before every network request, as the architecture requires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum PolicyRejection {
    UnsupportedScheme { scheme: String },
    CredentialsInUrl,
    MissingHost,
    MalformedUrl { detail: String },
}

impl std::fmt::Display for PolicyRejection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PolicyRejection::UnsupportedScheme { scheme } => {
                write!(formatter, "unsupported URL scheme `{scheme}`")
            }
            PolicyRejection::CredentialsInUrl => {
                write!(formatter, "URL embeds credentials")
            }
            PolicyRejection::MissingHost => write!(formatter, "URL has no host"),
            PolicyRejection::MalformedUrl { detail } => {
                write!(formatter, "malformed URL: {detail}")
            }
        }
    }
}

/// Resource budgets and policy switches for one acquisition run.
#[derive(Debug, Clone)]
pub struct AcquisitionConfig {
    /// Maximum number of payload downloads in one run.
    pub max_downloads: usize,
    /// Per-file byte bounds forwarded to the downloader.
    pub min_bytes_per_file: u64,
    pub max_bytes_per_file: u64,
    /// Run-wide byte budget across all downloads.
    pub max_total_bytes: u64,
    pub request_timeout: Duration,
    pub total_timeout: Duration,
    /// Polite pause between two consecutive payload downloads.
    pub inter_download_delay: Duration,
    /// Permit plain http:// PDF sources. Defaults to false.
    pub allow_http: bool,
}

impl Default for AcquisitionConfig {
    fn default() -> Self {
        Self {
            max_downloads: 5,
            min_bytes_per_file: 1024,
            max_bytes_per_file: 64 * 1024 * 1024,
            max_total_bytes: 256 * 1024 * 1024,
            request_timeout: Duration::from_secs(60),
            total_timeout: Duration::from_secs(10 * 60),
            inter_download_delay: Duration::from_secs(1),
            allow_http: false,
        }
    }
}

impl AcquisitionConfig {
    pub fn validate(&self) -> Result<(), LibraryError> {
        if self.max_downloads == 0 {
            return Err(LibraryError::InvalidConfig(
                "max_downloads must be positive".into(),
            ));
        }
        if self.min_bytes_per_file == 0 || self.max_bytes_per_file == 0 {
            return Err(LibraryError::InvalidConfig(
                "byte bounds must be positive".into(),
            ));
        }
        if self.min_bytes_per_file > self.max_bytes_per_file {
            return Err(LibraryError::InvalidConfig(
                "min_bytes_per_file exceeds max_bytes_per_file".into(),
            ));
        }
        if self.max_total_bytes < self.max_bytes_per_file {
            return Err(LibraryError::InvalidConfig(
                "max_total_bytes is smaller than one permitted file".into(),
            ));
        }
        if self.request_timeout.is_zero() || self.total_timeout.is_zero() {
            return Err(LibraryError::InvalidConfig(
                "timeouts must be positive".into(),
            ));
        }
        Ok(())
    }
}

/// Directory layout of a local library.
#[derive(Debug, Clone)]
pub struct LibraryLayout {
    root: PathBuf,
}

impl LibraryLayout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn objects_dir(&self) -> PathBuf {
        self.root.join("objects")
    }

    pub fn manifests_dir(&self) -> PathBuf {
        self.root.join("manifests")
    }

    pub fn quarantine_dir(&self) -> PathBuf {
        self.root.join("quarantine")
    }

    pub fn reports_dir(&self) -> PathBuf {
        self.root.join("reports")
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    /// Extracted text artifacts (`extracted/<sha[0..2]>/<sha>.txt`), see `src/extract.rs`.
    pub fn extracted_dir(&self) -> PathBuf {
        self.root.join("extracted")
    }

    /// Extraction provenance records (`extraction/<sha>.json`).
    pub fn extraction_dir(&self) -> PathBuf {
        self.root.join("extraction")
    }

    /// Durable acquisition job records (#6): one atomically-published JSON per job.
    pub fn jobs_dir(&self) -> PathBuf {
        self.root.join("jobs")
    }

    /// Create the directory skeleton. Idempotent.
    pub fn create(&self) -> Result<(), LibraryError> {
        for directory in [
            self.root.clone(),
            self.objects_dir(),
            self.manifests_dir(),
            self.quarantine_dir(),
            self.reports_dir(),
            self.tmp_dir(),
            self.extracted_dir(),
            self.extraction_dir(),
            self.jobs_dir(),
        ] {
            fs::create_dir_all(directory)?;
        }
        Ok(())
    }

    pub fn object_path(&self, sha256_hex: &str) -> Result<PathBuf, LibraryError> {
        if sha256_hex.len() < 2 || !sha256_hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(LibraryError::InvalidLayout(format!(
                "not a SHA-256 hex digest: {sha256_hex}"
            )));
        }
        let lower = sha256_hex.to_ascii_lowercase();
        Ok(self
            .objects_dir()
            .join(&lower[..2])
            .join(format!("{lower}.pdf")))
    }

    pub fn manifest_path(
        &self,
        source_id: &str,
        record_id: &str,
        sha256_hex: &str,
    ) -> Result<PathBuf, LibraryError> {
        if sha256_hex.len() < 12 {
            return Err(LibraryError::InvalidLayout("short digest".into()));
        }
        let stem = sanitize_stem(&format!(
            "{source_id}-{record_id}-{}",
            &sha256_hex.to_ascii_lowercase()[..12]
        ));
        Ok(self.manifests_dir().join(format!("{stem}.json")))
    }

    /// Scratch destination for an in-flight transfer.
    fn scratch_path(&self, source_id: &str, record_id: &str) -> PathBuf {
        self.tmp_dir().join(format!(
            "{}.pdf",
            sanitize_stem(&format!("{source_id}-{record_id}"))
        ))
    }

    /// All finalized objects with their byte lengths, sorted by path.
    pub fn list_objects(&self) -> Result<Vec<(PathBuf, u64)>, LibraryError> {
        let mut objects = Vec::new();
        let objects_dir = self.objects_dir();
        if !objects_dir.exists() {
            return Ok(objects);
        }
        for entry in fs::read_dir(&objects_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            for file in fs::read_dir(entry.path())? {
                let file = file?;
                if file.file_name().to_string_lossy().ends_with(".pdf") {
                    let length = file.metadata()?.len();
                    objects.push((file.path(), length));
                }
            }
        }
        objects.sort();
        Ok(objects)
    }

    /// All provenance manifests, parsed.
    pub fn list_manifests(&self) -> Result<Vec<ProvenanceManifest>, LibraryError> {
        let mut manifests = Vec::new();
        let dir = self.manifests_dir();
        if !dir.exists() {
            return Ok(manifests);
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let content = fs::read_to_string(&path)?;
            match serde_json::from_str(&content) {
                Ok(manifest) => manifests.push(manifest),
                Err(error) => {
                    return Err(LibraryError::InvalidLayout(format!(
                        "provenance manifest {} is not valid JSON: {error}",
                        path.display()
                    )));
                }
            }
        }
        manifests.sort_by(|a, b| a.work.title.cmp(&b.work.title));
        Ok(manifests)
    }

    /// Objects present under `objects/` whose content hash has no provenance manifest.
    ///
    /// These appear when a crash interrupts finalization between the object rename and
    /// manifest publication: the bytes are validated and content-addressed, but the
    /// library cannot attribute them to a work until a later run re-acquires the record
    /// and writes the missing manifest. Surfacing them keeps an interrupted run honest —
    /// an unattributed object is visible as such instead of silently counting as a
    /// complete acquisition.
    pub fn unattributed_objects(&self) -> Result<Vec<(PathBuf, u64)>, LibraryError> {
        let attributed: HashSet<String> = self
            .list_manifests()?
            .into_iter()
            .map(|manifest| manifest.content.sha256_hex.to_ascii_lowercase())
            .collect();
        let mut orphans = Vec::new();
        for (path, length) in self.list_objects()? {
            let stem = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if !attributed.contains(&stem) {
                orphans.push((path, length));
            }
        }
        orphans.sort();
        Ok(orphans)
    }
}

pub fn sanitize_stem(raw: &str) -> String {
    let mut sanitized: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    sanitized.truncate(MAX_MANIFEST_STEM_BYTES);
    if sanitized.is_empty() {
        sanitized = "unnamed".to_owned();
    }
    sanitized
}

/// Provenance recorded next to every acquired object.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProvenanceManifest {
    pub schema_version: u16,
    pub work: ProvenanceWork,
    pub acquisition: ProvenanceAcquisition,
    pub content: ProvenanceContent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProvenanceWork {
    pub source_id: String,
    pub source_record_id: String,
    pub identifiers: Vec<crate::sources::work_record::WorkIdentifier>,
    pub title: String,
    pub authors: Vec<String>,
    pub publication_year: Option<u16>,
    pub venue: Option<String>,
    pub license: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProvenanceAcquisition {
    /// PDF URL with any userinfo credentials removed.
    pub pdf_url: String,
    pub retrieval_unix_seconds: u64,
    pub retrieval_rfc3339: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProvenanceContent {
    pub byte_length: u64,
    pub sha256_hex: String,
    pub blake3_hex: String,
    pub page_count: usize,
    pub pdf_version: String,
}

/// Per-record result written into the run report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcquisitionReportEntry {
    pub schema_version: u16,
    pub source_id: String,
    pub source_record_id: String,
    pub doi: Option<String>,
    pub arxiv_id: Option<String>,
    pub title: String,
    /// Full PDF URL with credentials stripped; `None` when no permitted PDF URL exists.
    pub pdf_url: Option<String>,
    pub retrieved_unix_seconds: u64,
    pub outcome: AcquisitionOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AcquisitionOutcome {
    Acquired {
        deduplicated: bool,
        byte_length: u64,
        sha256_hex: String,
        blake3_hex: String,
        page_count: usize,
        object_path: String,
        manifest_path: String,
    },
    Quarantined {
        reason: serde_json::Value,
        quarantined_path: String,
        manifest_path: String,
    },
    Failed {
        stage: String,
        detail: String,
    },
    Skipped {
        reason: String,
    },
}

/// Run-wide accounting returned by [`run_acquisition`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcquisitionRunSummary {
    pub records_examined: usize,
    pub acquired: usize,
    pub deduplicated: usize,
    pub quarantined: usize,
    pub failed: usize,
    pub skipped: usize,
    pub total_bytes: u64,
    pub download_budget_exhausted: bool,
}

/// Re-check the URL policy immediately before a request.
pub fn authorize_url(raw: &str, allow_http: bool) -> Result<Url, PolicyRejection> {
    let url = Url::parse(raw).map_err(|error| PolicyRejection::MalformedUrl {
        detail: error.to_string(),
    })?;
    match url.scheme() {
        "https" => {}
        "http" if allow_http => {}
        scheme => {
            return Err(PolicyRejection::UnsupportedScheme {
                scheme: scheme.to_owned(),
            })
        }
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(PolicyRejection::CredentialsInUrl);
    }
    if url.host_str().is_none() {
        return Err(PolicyRejection::MissingHost);
    }
    Ok(url)
}

/// Remove userinfo from a URL string while keeping the rest intact.
fn strip_url_credentials(raw: &str) -> String {
    match Url::parse(raw) {
        Ok(mut url) => {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.to_string()
        }
        Err(_) => raw.to_owned(),
    }
}

/// Mutable run state threaded through the pipeline.
#[derive(Debug)]
struct RunBudget {
    downloads_left: usize,
    bytes_left: u64,
}

impl RunBudget {
    fn exhausted(&self) -> bool {
        self.downloads_left == 0 || self.bytes_left == 0
    }
}

/// Acquire one work record: durable job gate, policy re-check, bounded download,
/// structural validation, and content-addressed storage with provenance.
///
/// The job ledger (#6) decides whether the network is touched at all: terminal jobs
/// skip unless explicitly re-entered (`--refetch`) or re-targeted, due retries proceed,
/// and abandoned in-flight jobs recover only after their lease expires.
#[allow(clippy::too_many_arguments)]
async fn acquire_work(
    record: &WorkRecord,
    library: &LibraryLayout,
    config: &AcquisitionConfig,
    downloader: &ResumableDownloader,
    budget: &mut RunBudget,
    ledger: &JobLedger,
    policy: &JobPolicy,
    now_unix_seconds: u64,
) -> AcquisitionReportEntry {
    let entry = |outcome: AcquisitionOutcome| AcquisitionReportEntry {
        schema_version: LIBRARY_SCHEMA_VERSION,
        source_id: record.source_id.clone(),
        source_record_id: record.source_record_id.clone(),
        doi: record.doi().map(str::to_owned),
        arxiv_id: record.arxiv_id().map(str::to_owned),
        title: record.title.clone(),
        pdf_url: record.oa_pdf_url.as_deref().map(strip_url_credentials),
        retrieved_unix_seconds: now_unix_seconds,
        outcome,
    };

    // Durable job gate: load-or-create, then decide.
    let loaded = ledger.load(&record.source_id, &record.source_record_id);
    let loaded = match loaded {
        Ok(job) => job,
        Err(error) => {
            return entry(AcquisitionOutcome::Failed {
                stage: "job-ledger".to_owned(),
                detail: error.to_string(),
            })
        }
    };
    let job = loaded.unwrap_or_else(|| {
        JobRecord::new(
            &record.source_id,
            &record.source_record_id,
            "",
            now_unix_seconds,
        )
    });
    let target = record.oa_pdf_url.as_deref().map(strip_url_credentials);
    let mut job = match plan_job_action(job, target.as_deref(), policy, now_unix_seconds) {
        JobAction::Skip(job, reason) => {
            // Persist state changes made by the gate (e.g. Discovered → Skipped).
            if let Err(error) = ledger.store(&job) {
                return entry(AcquisitionOutcome::Failed {
                    stage: "job-ledger".to_owned(),
                    detail: error.to_string(),
                });
            }
            return entry(AcquisitionOutcome::Skipped { reason });
        }
        JobAction::Proceed(job) => job,
    };
    job.target_url = target.unwrap_or_default();

    // Policy re-check immediately before the request.
    if let Err(rejection) = authorize_url(
        record.oa_pdf_url.as_deref().unwrap_or(""),
        config.allow_http,
    ) {
        let _ = job.advance(
            JobState::Skipped,
            &crate::jobs::TransitionContext::default(),
            now_unix_seconds,
        );
        let _ = ledger.store(&job);
        return entry(AcquisitionOutcome::Skipped {
            reason: format!("policy rejected URL: {rejection}"),
        });
    }

    if budget.exhausted() {
        // A budget skip is a property of this run, not an outcome of the work: the
        // in-memory Fetching transition is deliberately not persisted.
        return entry(AcquisitionOutcome::Skipped {
            reason: "run budget exhausted".to_owned(),
        });
    }

    let scratch = library.scratch_path(&record.source_id, &record.source_record_id);
    // Scratch is never durable state: clear a leftover from an interrupted run before
    // starting a fresh transfer.
    if scratch.exists() {
        let _ = fs::remove_file(&scratch);
    }

    // Persist the in-flight state with a lease right before the transfer starts.
    job.lease = Some(crate::jobs::JobLease {
        owner: lease_owner(now_unix_seconds),
        expires_unix_seconds: lease_expiry(now_unix_seconds, config.total_timeout),
    });
    if let Err(error) = ledger.store(&job) {
        return entry(AcquisitionOutcome::Failed {
            stage: "job-ledger".to_owned(),
            detail: error.to_string(),
        });
    }

    let transfer: Result<DownloadResult, DownloadError> = downloader
        .download(record.oa_pdf_url.as_deref().unwrap_or(""), &scratch)
        .await;
    let transfer = match transfer {
        Ok(transfer) => transfer,
        Err(error) => {
            let detail = record_job_failure(
                &mut job,
                classify_download_error(&error),
                &error.to_string(),
                policy,
                now_unix_seconds,
            );
            let _ = ledger.store(&job);
            return entry(AcquisitionOutcome::Failed {
                stage: "download".to_owned(),
                detail,
            });
        }
    };

    budget.downloads_left -= 1;
    budget.bytes_left = budget.bytes_left.saturating_sub(transfer.bytes);

    let _ = job.advance(
        JobState::Validating,
        &crate::jobs::TransitionContext::default(),
        now_unix_seconds,
    );
    let _ = ledger.store(&job);

    let validation = validate_or_quarantine_pdf(
        &scratch,
        library.quarantine_dir(),
        PdfValidationContext {
            source_url: record.oa_pdf_url.clone(),
            declared_content_type: None,
            expected_blake3_hex: None,
            expected_sha256_hex: None,
        },
        PdfValidationConfig {
            min_bytes: config.min_bytes_per_file,
            max_bytes: config.max_bytes_per_file,
        },
    );
    match validation {
        Err(error) => {
            let detail = record_job_failure(
                &mut job,
                JobErrorClass::Transient,
                &error.to_string(),
                policy,
                now_unix_seconds,
            );
            let _ = ledger.store(&job);
            entry(AcquisitionOutcome::Failed {
                stage: "validation".to_owned(),
                detail,
            })
        }
        Ok(PdfValidationOutcome::Quarantined(quarantined)) => {
            let _ = job.advance(
                JobState::Quarantined,
                &crate::jobs::TransitionContext::default(),
                now_unix_seconds,
            );
            job.last_error_class = Some(JobErrorClass::Corrupt);
            job.last_error_detail =
                Some(serde_json::to_string(&quarantined.manifest.reason).unwrap_or_default());
            let _ = ledger.store(&job);
            entry(AcquisitionOutcome::Quarantined {
                reason: serde_json::to_value(&quarantined.manifest.reason).unwrap_or_default(),
                quarantined_path: quarantined.quarantined_path.to_string_lossy().into_owned(),
                manifest_path: quarantined.manifest_path.to_string_lossy().into_owned(),
            })
        }
        Ok(PdfValidationOutcome::Valid(report)) => {
            let _ = job.advance(
                JobState::Storing,
                &crate::jobs::TransitionContext::default(),
                now_unix_seconds,
            );
            let _ = ledger.store(&job);
            match finalize_object(
                library,
                record,
                record.oa_pdf_url.as_deref().unwrap_or(""),
                &report,
                now_unix_seconds,
            ) {
                Ok((object_path, manifest_path, deduplicated)) => {
                    let _ = job.advance(
                        JobState::Acquired,
                        &crate::jobs::TransitionContext::default(),
                        now_unix_seconds,
                    );
                    job.acquired_sha256_hex = Some(report.sha256_hex.clone());
                    let _ = ledger.store(&job);
                    entry(AcquisitionOutcome::Acquired {
                        deduplicated,
                        byte_length: report.byte_length,
                        sha256_hex: report.sha256_hex.clone(),
                        blake3_hex: report.blake3_hex.clone(),
                        page_count: report.page_count,
                        object_path: object_path.to_string_lossy().into_owned(),
                        manifest_path: manifest_path.to_string_lossy().into_owned(),
                    })
                }
                Err(error) => {
                    let detail = record_job_failure(
                        &mut job,
                        JobErrorClass::Transient,
                        &error.to_string(),
                        policy,
                        now_unix_seconds,
                    );
                    let _ = ledger.store(&job);
                    entry(AcquisitionOutcome::Failed {
                        stage: "storage".to_owned(),
                        detail,
                    })
                }
            }
        }
    }
}

/// Apply the retry policy to a failure and return the report detail.
fn record_job_failure(
    job: &mut JobRecord,
    class: JobErrorClass,
    detail: &str,
    policy: &JobPolicy,
    now_unix_seconds: u64,
) -> String {
    if class.is_retryable() && job.attempts < policy.max_attempts {
        let _ = job.fail_retryable(
            class,
            detail,
            policy.retry_min_delay_seconds,
            now_unix_seconds,
        );
        format!(
            "{detail}; retryable ({}), next attempt after {}",
            class.as_str(),
            job.next_retry_unix_seconds.unwrap_or(now_unix_seconds)
        )
    } else {
        let suffix = if class.is_retryable() {
            format!(
                "; retry budget exhausted after {} attempt{} (class {})",
                job.attempts,
                if job.attempts == 1 { "" } else { "s" },
                class.as_str()
            )
        } else {
            format!(
                "; permanent ({}), re-run with --refetch to re-enter",
                class.as_str()
            )
        };
        let combined = format!("{detail}{suffix}");
        let _ = job.fail_permanent(class, &combined, now_unix_seconds);
        combined
    }
}

/// Move a validated payload into content-addressed storage and write its provenance.
fn finalize_object(
    library: &LibraryLayout,
    record: &WorkRecord,
    pdf_url: &str,
    report: &crate::fetcher::pdf_validation::PdfValidationReport,
    now_unix_seconds: u64,
) -> Result<(PathBuf, PathBuf, bool), LibraryError> {
    let object_path = library.object_path(&report.sha256_hex)?;
    let manifest_path = library.manifest_path(
        &record.source_id,
        &record.source_record_id,
        &report.sha256_hex,
    )?;

    let deduplicated = object_path.exists();
    if deduplicated {
        // The bytes are already in the library; drop the scratch copy.
        let _ = fs::remove_file(library.scratch_path(&record.source_id, &record.source_record_id));
    } else {
        if let Some(parent) = object_path.parent() {
            fs::create_dir_all(parent)?;
        }
        // Atomic same-filesystem publication.
        fs::rename(
            library.scratch_path(&record.source_id, &record.source_record_id),
            &object_path,
        )?;
        if let Some(parent) = object_path.parent() {
            sync_directory(parent)?;
        }
    }

    let manifest = ProvenanceManifest {
        schema_version: LIBRARY_SCHEMA_VERSION,
        work: ProvenanceWork {
            source_id: record.source_id.clone(),
            source_record_id: record.source_record_id.clone(),
            identifiers: record.identifiers.clone(),
            title: record.title.clone(),
            authors: record.authors.clone(),
            publication_year: record.publication_year,
            venue: record.venue.clone(),
            license: record.license.clone(),
        },
        acquisition: ProvenanceAcquisition {
            pdf_url: strip_url_credentials(pdf_url),
            retrieval_unix_seconds: now_unix_seconds,
            retrieval_rfc3339: format_rfc3339_utc(now_unix_seconds),
        },
        content: ProvenanceContent {
            byte_length: report.byte_length,
            sha256_hex: report.sha256_hex.clone(),
            blake3_hex: report.blake3_hex.clone(),
            page_count: report.page_count,
            pdf_version: report.pdf_version.clone(),
        },
    };

    if let Some(parent) = manifest_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let serialized = serde_json::to_vec_pretty(&manifest)?;
    publish_bytes_atomically(&manifest_path, &serialized)?;

    Ok((object_path, manifest_path, deduplicated))
}

/// Publish `bytes` at `path` atomically: write to a sibling `.pending` file, sync it,
/// rename over the destination, then sync the parent directory.
///
/// A crash at any point leaves either the previous content or the complete new content
/// at `path`; a torn write can only ever exist under the `.pending` sibling name, which
/// readers ignore. This is the same publication pattern the quarantine path uses.
pub fn publish_bytes_atomically(path: &Path, bytes: &[u8]) -> Result<(), LibraryError> {
    let pending = append_suffix(path, ".pending");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        // Clear a stale pending file left by an interrupted earlier publication.
        let _ = fs::remove_file(&pending);
    }
    let mut file = fs::File::create(&pending)?;
    if let Err(error) = file.write_all(bytes) {
        let _ = fs::remove_file(&pending);
        return Err(error.into());
    }
    if let Err(error) = file.sync_all() {
        let _ = fs::remove_file(&pending);
        return Err(error.into());
    }
    drop(file);
    if let Err(error) = fs::rename(&pending, path) {
        let _ = fs::remove_file(&pending);
        return Err(error.into());
    }
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn sync_directory(path: &Path) -> Result<(), LibraryError> {
    fs::File::open(path)?.sync_all()?;
    Ok(())
}

/// Run the acquisition pipeline over a stream of records, invoking `on_entry` for each
/// report entry so callers can persist JSONL incrementally.
pub async fn run_acquisition<'a, I>(
    records: I,
    library: &LibraryLayout,
    config: &AcquisitionConfig,
    downloader: &ResumableDownloader,
    job_policy: &JobPolicy,
    now_unix_seconds: u64,
    mut on_entry: impl FnMut(&AcquisitionReportEntry) -> Result<(), io::Error>,
) -> Result<AcquisitionRunSummary, LibraryError>
where
    I: Iterator<Item = &'a WorkRecord>,
{
    config.validate()?;
    library.create()?;
    let ledger = JobLedger::new(library);

    let mut budget = RunBudget {
        downloads_left: config.max_downloads,
        bytes_left: config.max_total_bytes,
    };
    let mut summary = AcquisitionRunSummary {
        records_examined: 0,
        acquired: 0,
        deduplicated: 0,
        quarantined: 0,
        failed: 0,
        skipped: 0,
        total_bytes: 0,
        download_budget_exhausted: false,
    };

    for record in records {
        if budget.exhausted() {
            summary.download_budget_exhausted = true;
            // Still report the unexamined remainder as skipped so the report is complete.
            summary.skipped += 1;
            on_entry(&skipped_entry(
                record,
                now_unix_seconds,
                "run budget exhausted",
            ))?;
            continue;
        }
        summary.records_examined += 1;
        let entry = acquire_work(
            record,
            library,
            config,
            downloader,
            &mut budget,
            &ledger,
            job_policy,
            now_unix_seconds,
        )
        .await;
        match &entry.outcome {
            AcquisitionOutcome::Acquired {
                deduplicated,
                byte_length,
                ..
            } => {
                summary.acquired += 1;
                if *deduplicated {
                    summary.deduplicated += 1;
                }
                summary.total_bytes += byte_length;
            }
            AcquisitionOutcome::Quarantined { .. } => summary.quarantined += 1,
            AcquisitionOutcome::Failed { .. } => summary.failed += 1,
            AcquisitionOutcome::Skipped { .. } => summary.skipped += 1,
        }
        on_entry(&entry)?;
    }

    Ok(summary)
}

fn skipped_entry(
    record: &WorkRecord,
    now_unix_seconds: u64,
    reason: &str,
) -> AcquisitionReportEntry {
    AcquisitionReportEntry {
        schema_version: LIBRARY_SCHEMA_VERSION,
        source_id: record.source_id.clone(),
        source_record_id: record.source_record_id.clone(),
        doi: record.doi().map(str::to_owned),
        arxiv_id: record.arxiv_id().map(str::to_owned),
        title: record.title.clone(),
        pdf_url: record.oa_pdf_url.as_deref().map(strip_url_credentials),
        retrieved_unix_seconds: now_unix_seconds,
        outcome: AcquisitionOutcome::Skipped {
            reason: reason.to_owned(),
        },
    }
}

/// Build the downloader used by the acquisition pipeline.
pub fn acquisition_downloader(
    config: &AcquisitionConfig,
) -> Result<ResumableDownloader, DownloadError> {
    ResumableDownloader::new(DownloadConfig {
        max_bytes: config.max_bytes_per_file,
        min_bytes: config.min_bytes_per_file,
        request_timeout: config.request_timeout,
        total_timeout: config.total_timeout,
    })
}

/// Format a UNIX timestamp as an RFC 3339 UTC instant without pulling in a datetime crate.
pub fn format_rfc3339_utc(unix_seconds: u64) -> String {
    let days = unix_seconds / 86_400;
    let seconds_of_day = unix_seconds % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Howard Hinnant's `civil_from_days` algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_matches_known_instants() {
        assert_eq!(format_rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339_utc(86_400), "1970-01-02T00:00:00Z");
        assert_eq!(format_rfc3339_utc(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(format_rfc3339_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_rfc3339_utc(1_000_000_000), "2001-09-09T01:46:40Z");
    }

    #[test]
    fn url_policy_gate_rejects_unsafe_urls() {
        assert!(authorize_url("https://example.org/a.pdf", false).is_ok());
        assert!(authorize_url("http://example.org/a.pdf", true).is_ok());

        assert!(matches!(
            authorize_url("http://example.org/a.pdf", false),
            Err(PolicyRejection::UnsupportedScheme { .. })
        ));
        assert!(matches!(
            authorize_url("ftp://example.org/a.pdf", true),
            Err(PolicyRejection::UnsupportedScheme { .. })
        ));
        assert!(matches!(
            authorize_url("https://user:secret@example.org/a.pdf", false),
            Err(PolicyRejection::CredentialsInUrl)
        ));
        assert!(matches!(
            authorize_url("mailto:someone@example.org", false),
            Err(PolicyRejection::UnsupportedScheme { .. }) | Err(PolicyRejection::MissingHost)
        ));
        assert!(matches!(
            authorize_url("not a url at all", false),
            Err(PolicyRejection::MalformedUrl { .. })
        ));
    }

    #[test]
    fn credentials_are_stripped_from_reported_urls() {
        assert_eq!(
            strip_url_credentials("https://user:secret@example.org/a.pdf?x=1"),
            "https://example.org/a.pdf?x=1"
        );
    }

    #[test]
    fn object_paths_are_content_addressed() {
        let library = LibraryLayout::new("/tmp/paperengine-test-library");
        let sha = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let path = library.object_path(sha).unwrap();
        assert_eq!(
            path,
            PathBuf::from("/tmp/paperengine-test-library/objects/ab/abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789.pdf")
        );
        assert!(library.object_path("zz").is_err());
        assert!(library.object_path("not-hex!").is_err());
    }

    #[test]
    fn acquisition_config_validates_bounds() {
        assert!(AcquisitionConfig::default().validate().is_ok());
        assert!(AcquisitionConfig {
            max_downloads: 0,
            ..AcquisitionConfig::default()
        }
        .validate()
        .is_err());
        assert!(AcquisitionConfig {
            min_bytes_per_file: 10,
            max_bytes_per_file: 5,
            ..AcquisitionConfig::default()
        }
        .validate()
        .is_err());
        assert!(AcquisitionConfig {
            max_total_bytes: 1,
            ..AcquisitionConfig::default()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn report_entries_round_trip_through_json() {
        let entry = AcquisitionReportEntry {
            schema_version: LIBRARY_SCHEMA_VERSION,
            source_id: "arxiv".into(),
            source_record_id: "2401.00001".into(),
            doi: None,
            arxiv_id: Some("2401.00001".into()),
            title: "Test".into(),
            pdf_url: Some("https://arxiv.org/pdf/2401.00001".into()),
            retrieved_unix_seconds: 1_700_000_000,
            outcome: AcquisitionOutcome::Acquired {
                deduplicated: false,
                byte_length: 1234,
                sha256_hex: "ab".repeat(32),
                blake3_hex: "cd".repeat(32),
                page_count: 7,
                object_path: "/library/objects/ab/ab.pdf".into(),
                manifest_path: "/library/manifests/arxiv-2401.00001-ab.json".into(),
            },
        };
        let serialized = serde_json::to_string(&entry).unwrap();
        assert!(serialized.contains("\"kind\":\"acquired\""));
        let parsed: AcquisitionReportEntry = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed.outcome, entry.outcome);
    }
}
