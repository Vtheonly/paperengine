//! Durable acquisition job ledger and state machine (#6).
//!
//! Every work record that enters the acquisition pipeline gets a durable job record under
//! `library/jobs/`, persisted atomically at each state boundary. The ledger survives
//! restarts: an in-flight job whose lease has expired is recovered, retryable failures
//! back off with bounded attempts, and terminal states are only re-entered explicitly
//! (`--refetch`) or when a new discovery observation changes the target URL.
//!
//! Lifecycle (discovery through the reserved indexing states):
//!
//! ```text
//! Discovered → Fetching → Validating → Storing → Acquired → Extracted → Indexed
//!                 ↘ FailedRetryable ↗ (due retry)      ↘ Quarantined
//!                        ↘ FailedPermanent (budget exhausted)
//! Discovered → Skipped (no permitted target / policy rejection)
//! ```
//!
//! In-flight states (`Fetching`, `Validating`, `Storing`) carry a lease; a restart may
//! re-enter `Fetching` only once that lease has expired. Terminal states never transition
//! forward except through the explicit re-entry paths above. The state machine itself is
//! pure; only [`JobLedger`] touches the filesystem, and nothing in this module is part of
//! the computational kernel.

use crate::library::{publish_bytes_atomically, LibraryLayout};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;
use thiserror::Error;

pub const JOBS_SCHEMA_VERSION: u16 = 1;

/// Upper bound for the exponential retry delay.
const RETRY_DELAY_CAP_SECONDS: u64 = 3_600;

#[derive(Debug, Error)]
pub enum JobError {
    #[error("filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("job record serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("job ledger storage failed: {0}")]
    Storage(String),
    #[error("invalid job record {path}: {detail}")]
    InvalidRecord { path: String, detail: String },
}

/// Lifecycle state of one acquisition job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum JobState {
    /// Record known from discovery; no fetch attempt started.
    Discovered,
    /// Transfer in flight; a lease names the owner and its expiry.
    Fetching,
    /// Payload downloaded; structural validation in flight.
    Validating,
    /// Payload validated; durable finalization in flight.
    Storing,
    /// Object finalized into content-addressed storage with provenance. Terminal.
    Acquired,
    /// Reserved: text/metadata extracted from the object (#31 pipeline). Terminal.
    Extracted,
    /// Reserved: object indexed for search (#32). Terminal.
    Indexed,
    /// Payload rejected by validation and quarantined. Terminal.
    Quarantined,
    /// Attempt failed with a retryable error class; `next_retry_unix_seconds` is set.
    FailedRetryable,
    /// Retry budget exhausted or the error class is permanent. Terminal.
    FailedPermanent,
    /// No permitted target or policy rejected the URL. Terminal for that target.
    Skipped,
}

impl JobState {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            JobState::Acquired
                | JobState::Extracted
                | JobState::Indexed
                | JobState::Quarantined
                | JobState::FailedPermanent
                | JobState::Skipped
        )
    }

    pub fn is_in_flight(&self) -> bool {
        matches!(
            self,
            JobState::Fetching | JobState::Validating | JobState::Storing
        )
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            JobState::Discovered => "discovered",
            JobState::Fetching => "fetching",
            JobState::Validating => "validating",
            JobState::Storing => "storing",
            JobState::Acquired => "acquired",
            JobState::Extracted => "extracted",
            JobState::Indexed => "indexed",
            JobState::Quarantined => "quarantined",
            JobState::FailedRetryable => "failed_retryable",
            JobState::FailedPermanent => "failed_permanent",
            JobState::Skipped => "skipped",
        }
    }
}

/// Context that gates otherwise-invalid transitions.
#[derive(Debug, Clone, Copy, Default)]
pub struct TransitionContext {
    /// The in-flight job's lease has expired (or none was recorded): restart recovery.
    pub lease_expired: bool,
    /// The operator explicitly requested re-entry (`--refetch`).
    pub explicit_reentry: bool,
    /// New discovery observation carries a different target URL.
    pub target_url_changed: bool,
}

/// Pure transition validation. Returns `Ok(())` when `from → to` is legal in `context`.
pub fn validate_transition(
    from: JobState,
    to: JobState,
    context: &TransitionContext,
) -> Result<(), InvalidTransition> {
    // Self-transitions are idempotent no-ops only for in-flight lease refreshes.
    if from == to {
        if from.is_in_flight() {
            return Ok(());
        }
        return Err(InvalidTransition {
            from,
            to,
            reason: "only in-flight states may self-transition (lease refresh)".into(),
        });
    }

    // Reserved forward chain: provenance-bearing outcomes may advance into the
    // extraction/indexing lifecycle stages regardless of their terminality for the
    // acquisition pipeline.
    if matches!(
        (from, to),
        (JobState::Acquired, JobState::Extracted) | (JobState::Extracted, JobState::Indexed)
    ) {
        return Ok(());
    }

    // Terminal states re-enter only via explicit reset or a changed target.
    if from.is_terminal() {
        if (context.explicit_reentry || context.target_url_changed) && to == JobState::Discovered {
            return Ok(());
        }
        return Err(InvalidTransition {
            from,
            to,
            reason: "terminal states re-enter only through explicit re-entry (--refetch) \
                     or a changed target URL, and only back to `discovered`"
                .into(),
        });
    }

    match (from, to) {
        (JobState::Discovered, JobState::Fetching)
        | (JobState::Discovered, JobState::Skipped)
        | (JobState::Discovered, JobState::FailedPermanent) => Ok(()),

        (JobState::Fetching, JobState::Validating) => Ok(()),
        // The pre-request policy gate can refuse a target the planner accepted.
        (JobState::Fetching, JobState::Skipped) => Ok(()),
        (JobState::Validating, JobState::Storing) => Ok(()),
        (JobState::Storing, JobState::Acquired) => Ok(()),

        // Retryable and permanent failures from every in-flight state.
        (
            JobState::Fetching | JobState::Validating | JobState::Storing,
            JobState::FailedRetryable,
        )
        | (
            JobState::Fetching | JobState::Validating | JobState::Storing,
            JobState::FailedPermanent,
        ) => Ok(()),

        // Quarantine is a validation outcome.
        (JobState::Validating, JobState::Quarantined) => Ok(()),

        // A due retry re-enters the fetch stage.
        (JobState::FailedRetryable, JobState::Fetching) => Ok(()),
        (JobState::FailedRetryable, JobState::FailedPermanent) => Ok(()),

        // Reserved forward path is handled before the terminal guard, so it stays legal
        // even though `Acquired`/`Extracted` are terminal for the acquisition pipeline.

        // Restart recovery: an abandoned in-flight job may re-enter the fetch stage
        // only once its lease has expired.
        (JobState::Fetching | JobState::Validating | JobState::Storing, JobState::Fetching) => {
            if context.lease_expired {
                Ok(())
            } else {
                Err(InvalidTransition {
                    from,
                    to,
                    reason: "in-flight job still holds a live lease; it cannot be re-entered \
                             until the lease expires"
                        .into(),
                })
            }
        }

        _ => Err(InvalidTransition {
            from,
            to,
            reason: "transition is not part of the job lifecycle".into(),
        }),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid job transition {from:?} → {to:?}: {reason}")]
pub struct InvalidTransition {
    pub from: JobState,
    pub to: JobState,
    pub reason: String,
}

/// Error classes required by the issue: how failures drive retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobErrorClass {
    /// Transport/timeout/server errors: retry with backoff.
    Transient,
    /// HTTP 429: retry after the source's hint or the configured floor.
    RateLimited,
    /// HTTP 404/410: the target is gone; permanent for that URL.
    NotFound,
    /// HTTP 401/403: access-restricted; a stop condition that is never bypassed.
    AuthRestricted,
    /// Payload failed structural validation; deterministic for the same bytes.
    Corrupt,
    /// Everything deterministic-fatal: bad URL, size bounds, configuration.
    Permanent,
}

impl JobErrorClass {
    pub fn is_retryable(&self) -> bool {
        matches!(self, JobErrorClass::Transient | JobErrorClass::RateLimited)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            JobErrorClass::Transient => "transient",
            JobErrorClass::RateLimited => "rate_limited",
            JobErrorClass::NotFound => "not_found",
            JobErrorClass::AuthRestricted => "auth_restricted",
            JobErrorClass::Corrupt => "corrupt",
            JobErrorClass::Permanent => "permanent",
        }
    }
}

/// Classify a downloader failure for retry decisions.
pub fn classify_download_error(error: &crate::fetcher::stream::DownloadError) -> JobErrorClass {
    use crate::fetcher::stream::DownloadError;
    match error {
        DownloadError::Http(_) | DownloadError::Io(_) | DownloadError::Timeout => {
            JobErrorClass::Transient
        }
        DownloadError::HttpStatus(status) => match status.as_u16() {
            429 => JobErrorClass::RateLimited,
            404 | 410 => JobErrorClass::NotFound,
            401 | 403 => JobErrorClass::AuthRestricted,
            500..=599 => JobErrorClass::Transient,
            _ => JobErrorClass::Permanent,
        },
        DownloadError::TooLarge { .. } | DownloadError::TooSmall { .. } => JobErrorClass::Permanent,
        DownloadError::InvalidSignature { .. } => JobErrorClass::Corrupt,
        DownloadError::Protocol(_) => JobErrorClass::Transient,
        DownloadError::Metadata(_) => JobErrorClass::Permanent,
        DownloadError::DestinationExists(_) => JobErrorClass::Permanent,
        DownloadError::InvalidConfig(_) => JobErrorClass::Permanent,
    }
}

/// Exponential backoff with a cap: `floor * 2^(attempt-1)`, at most the cap.
pub fn retry_delay_seconds(floor_seconds: u64, attempts_so_far: u32) -> u64 {
    let shift = attempts_so_far.saturating_sub(1).min(16);
    floor_seconds
        .saturating_mul(1_u64 << shift)
        .min(RETRY_DELAY_CAP_SECONDS)
}

/// A lease names the run that owns an in-flight job and when the lease lapses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobLease {
    pub owner: String,
    pub expires_unix_seconds: u64,
}

impl JobLease {
    pub fn is_expired(&self, now_unix_seconds: u64) -> bool {
        now_unix_seconds >= self.expires_unix_seconds
    }
}

/// Durable record of one acquisition job. Stored atomically per
/// `(source_id, source_record_id)` under the library's `jobs/` directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRecord {
    pub schema_version: u16,
    pub source_id: String,
    pub source_record_id: String,
    /// Target PDF URL with credentials stripped. A changed value re-opens terminal jobs.
    pub target_url: String,
    pub state: JobState,
    /// Fetch attempts made against the current target URL.
    pub attempts: u32,
    pub last_error_class: Option<JobErrorClass>,
    pub last_error_detail: Option<String>,
    pub next_retry_unix_seconds: Option<u64>,
    pub lease: Option<JobLease>,
    pub created_unix_seconds: u64,
    pub updated_unix_seconds: u64,
    /// Digest of the finalized object when the state is `Acquired` (or beyond).
    pub acquired_sha256_hex: Option<String>,
}

impl JobRecord {
    pub fn new(
        source_id: &str,
        source_record_id: &str,
        target_url: &str,
        now_unix_seconds: u64,
    ) -> Self {
        Self {
            schema_version: JOBS_SCHEMA_VERSION,
            source_id: source_id.to_owned(),
            source_record_id: source_record_id.to_owned(),
            target_url: target_url.to_owned(),
            state: JobState::Discovered,
            attempts: 0,
            last_error_class: None,
            last_error_detail: None,
            next_retry_unix_seconds: None,
            lease: None,
            created_unix_seconds: now_unix_seconds,
            updated_unix_seconds: now_unix_seconds,
            acquired_sha256_hex: None,
        }
    }

    /// Validate and apply a state transition, maintaining bookkeeping fields.
    ///
    /// `attempts` counts the scheduled fetch series against the current target URL:
    /// it increments when a fetch is scheduled from `Discovered` or a due retry from
    /// `FailedRetryable`, and resets when a terminal job is explicitly re-entered or
    /// the target changes. Restart recovery of an abandoned in-flight job does not
    /// count as a new scheduled attempt.
    pub fn advance(
        &mut self,
        to: JobState,
        context: &TransitionContext,
        now_unix_seconds: u64,
    ) -> Result<(), InvalidTransition> {
        validate_transition(self.state, to, context)?;
        // Explicit re-entry and changed-target reopenings reset the attempt series.
        if self.state.is_terminal() && to == JobState::Discovered {
            self.attempts = 0;
            self.next_retry_unix_seconds = None;
        }
        if to == JobState::Fetching {
            if matches!(self.state, JobState::Discovered | JobState::FailedRetryable) {
                self.attempts += 1;
            }
            self.next_retry_unix_seconds = None;
        }
        if to == JobState::FailedRetryable || to == JobState::FailedPermanent || to.is_terminal() {
            self.lease = None;
        }
        self.state = to;
        self.updated_unix_seconds = now_unix_seconds;
        Ok(())
    }

    /// Record a retryable failure with backoff.
    pub fn fail_retryable(
        &mut self,
        class: JobErrorClass,
        detail: &str,
        retry_floor_seconds: u64,
        now_unix_seconds: u64,
    ) -> Result<(), InvalidTransition> {
        let to = JobState::FailedRetryable;
        validate_transition(self.state, to, &TransitionContext::default())?;
        self.last_error_class = Some(class);
        self.last_error_detail = Some(detail.to_owned());
        self.next_retry_unix_seconds =
            Some(now_unix_seconds + retry_delay_seconds(retry_floor_seconds, self.attempts));
        self.lease = None;
        self.state = to;
        self.updated_unix_seconds = now_unix_seconds;
        Ok(())
    }

    /// Record a permanent failure.
    pub fn fail_permanent(
        &mut self,
        class: JobErrorClass,
        detail: &str,
        now_unix_seconds: u64,
    ) -> Result<(), InvalidTransition> {
        let to = JobState::FailedPermanent;
        validate_transition(self.state, to, &TransitionContext::default())?;
        self.last_error_class = Some(class);
        self.last_error_detail = Some(detail.to_owned());
        self.next_retry_unix_seconds = None;
        self.lease = None;
        self.state = to;
        self.updated_unix_seconds = now_unix_seconds;
        Ok(())
    }
}

/// Retry policy for one acquisition run.
#[derive(Debug, Clone, Copy)]
pub struct JobPolicy {
    /// Maximum fetch attempts against one target URL before terminal failure.
    pub max_attempts: u32,
    /// Floor for the exponential retry delay.
    pub retry_min_delay_seconds: u64,
    /// Operator explicitly requested re-entry (`--refetch`): terminal states re-open
    /// and a pending retry backoff is overridden.
    pub refetch: bool,
}

impl Default for JobPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            retry_min_delay_seconds: 60,
            refetch: false,
        }
    }
}

/// The gate's decision for one record entering the pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobAction {
    /// Proceed with the acquisition; the job (new or existing) is supplied.
    Proceed(JobRecord),
    /// Do not touch the network; report the reason as a skip.
    Skip(JobRecord, String),
}

/// Human-oriented reason for a terminal job, shared by the gate's skip paths.
fn terminal_reason(job: &JobRecord) -> String {
    match job.state {
        JobState::Acquired | JobState::Extracted | JobState::Indexed => format!(
            "job already {} (sha256 {}…); re-run with --refetch to acquire again",
            job.state.as_str(),
            job.acquired_sha256_hex.as_deref().unwrap_or("?")
        ),
        JobState::Quarantined => format!(
            "job already quarantined ({}); re-run with --refetch to retry",
            job.last_error_class
                .map(|class| class.as_str())
                .unwrap_or("unknown")
        ),
        JobState::FailedPermanent => format!(
            "job failed permanently ({}: {}); re-run with --refetch to re-enter",
            job.last_error_class
                .map(|class| class.as_str())
                .unwrap_or("unknown"),
            job.last_error_detail.as_deref().unwrap_or("-")
        ),
        JobState::Skipped => "job previously skipped for this target; re-run with \
                              --refetch to re-enter"
            .to_owned(),
        other => format!("job is terminal ({})", other.as_str()),
    }
}

/// Decide what to do with a record given its durable job state. Pure.
///
/// `target_url` is `None` when the source reports no permitted PDF URL for the work.
pub fn plan_job_action(
    mut job: JobRecord,
    target_url: Option<&str>,
    policy: &JobPolicy,
    now_unix_seconds: u64,
) -> JobAction {
    let Some(target_url) = target_url else {
        // A first observation without a permitted target is persisted as a skip; jobs
        // already terminal keep their (more informative) terminal reason.
        if job.state == JobState::Discovered {
            let _ = job.advance(
                JobState::Skipped,
                &TransitionContext::default(),
                now_unix_seconds,
            );
            return JobAction::Skip(
                job,
                "no direct OA PDF URL reported by the source".to_owned(),
            );
        }
        if job.state.is_terminal() {
            let reason = terminal_reason(&job);
            return JobAction::Skip(job, reason);
        }
        return JobAction::Skip(
            job,
            "no direct OA PDF URL reported by the source".to_owned(),
        );
    };

    let url_changed = job.target_url != target_url;
    let lease_expired = job
        .lease
        .as_ref()
        .map(|lease| lease.is_expired(now_unix_seconds))
        .unwrap_or(true);
    let mut context = TransitionContext {
        lease_expired,
        explicit_reentry: policy.refetch,
        target_url_changed: url_changed,
    };

    // Terminal states: re-enter only explicitly or because the target changed.
    if job.state.is_terminal() && !(policy.refetch || url_changed) {
        let reason = terminal_reason(&job);
        return JobAction::Skip(job, reason);
    }

    // Terminal but re-entering: reopen to Discovered (resets attempts).
    if job.state.is_terminal() {
        if let Err(error) = job.advance(JobState::Discovered, &context, now_unix_seconds) {
            return JobAction::Skip(job, format!("job re-entry rejected: {error}"));
        }
        job.target_url = target_url.to_owned();
        // Re-entry clears any stale lease knowledge for the fetch transition below.
        context.lease_expired = true;
    } else if url_changed {
        // A changed target URL re-targets a non-terminal job too: the attempt series
        // and backoff schedule belonged to the old target.
        job.attempts = 0;
        job.next_retry_unix_seconds = None;
        job.target_url = target_url.to_owned();
    }

    // A live lease held by another run must not be disturbed.
    if job.state.is_in_flight() {
        let live_lease = job
            .lease
            .as_ref()
            .filter(|lease| !lease.is_expired(now_unix_seconds))
            .map(|lease| (lease.owner.clone(), lease.expires_unix_seconds));
        if let Some((owner, expires)) = live_lease {
            return JobAction::Skip(
                job,
                format!("in-flight job holds a live lease owned by `{owner}` until {expires}"),
            );
        }
    }

    // Retry not yet due; an explicit --refetch overrides the backoff schedule.
    if job.state == JobState::FailedRetryable && !policy.refetch {
        if let Some(due) = job.next_retry_unix_seconds {
            if now_unix_seconds < due {
                let reason = format!(
                    "retry not due until {due} (attempt {}/{}; backoff)",
                    job.attempts, policy.max_attempts
                );
                return JobAction::Skip(job, reason);
            }
        }
    }

    // Proceed to the fetch stage (restart recovery or due retry or first attempt).
    if let Err(error) = job.advance(JobState::Fetching, &context, now_unix_seconds) {
        return JobAction::Skip(job, format!("job transition rejected: {error}"));
    }
    JobAction::Proceed(job)
}

/// Durable per-library job ledger. One atomically-published JSON file per job under
/// `library/jobs/`; readers ignore `.pending` siblings exactly like manifests.
pub struct JobLedger {
    directory: PathBuf,
}

impl JobLedger {
    pub fn new(library: &LibraryLayout) -> Self {
        Self {
            directory: library.jobs_dir(),
        }
    }

    fn record_path(&self, record: &JobRecord) -> PathBuf {
        self.directory.join(format!(
            "{}.json",
            crate::library::sanitize_stem(&format!(
                "{}-{}",
                record.source_id, record.source_record_id
            ))
        ))
    }

    /// Load a job; `Ok(None)` when the ledger has never seen this work.
    pub fn load(
        &self,
        source_id: &str,
        source_record_id: &str,
    ) -> Result<Option<JobRecord>, JobError> {
        let path = self.directory.join(format!(
            "{}.json",
            crate::library::sanitize_stem(&format!("{source_id}-{source_record_id}"))
        ));
        self.load_path(&path)
    }

    fn load_path(&self, path: &std::path::Path) -> Result<Option<JobRecord>, JobError> {
        if !path.exists() {
            return Ok(None);
        }
        let content = fs::read_to_string(path)?;
        let record: JobRecord =
            serde_json::from_str(&content).map_err(|error| JobError::InvalidRecord {
                path: path.display().to_string(),
                detail: error.to_string(),
            })?;
        if record.schema_version != JOBS_SCHEMA_VERSION {
            return Err(JobError::InvalidRecord {
                path: path.display().to_string(),
                detail: format!(
                    "unsupported job schema version {} (expected {JOBS_SCHEMA_VERSION})",
                    record.schema_version
                ),
            });
        }
        Ok(Some(record))
    }

    /// Persist a job atomically (pending → fsync → rename → dir sync).
    pub fn store(&self, record: &JobRecord) -> Result<(), JobError> {
        fs::create_dir_all(&self.directory)?;
        let bytes = serde_json::to_vec_pretty(record)?;
        publish_bytes_atomically(&self.record_path(record), &bytes)
            .map_err(|error| JobError::Storage(error.to_string()))?;
        Ok(())
    }

    /// All persisted jobs, sorted by (source, record id).
    pub fn list(&self) -> Result<Vec<JobRecord>, JobError> {
        let mut records = Vec::new();
        if !self.directory.exists() {
            return Ok(records);
        }
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Some(record) = self.load_path(&path)? {
                records.push(record);
            }
        }
        records.sort_by(|a, b| {
            (&a.source_id, &a.source_record_id).cmp(&(&b.source_id, &b.source_record_id))
        });
        Ok(records)
    }

    /// Counts per state name, for summaries and `inspect`.
    pub fn counts_by_state(&self) -> Result<BTreeMap<String, usize>, JobError> {
        let mut counts = BTreeMap::new();
        for record in self.list()? {
            *counts.entry(record.state.as_str().to_owned()).or_insert(0) += 1;
        }
        Ok(counts)
    }
}

/// Lease owner identity for one acquisition run: command, process, start time.
pub fn lease_owner(now_unix_seconds: u64) -> String {
    format!("acquire:pid:{}:{}", std::process::id(), now_unix_seconds)
}

/// Lease lifetime: the total transfer timeout plus a fixed slack for validation and
/// storage, so a live run is never considered abandoned by its own clock.
pub fn lease_expiry(now_unix_seconds: u64, total_timeout: Duration) -> u64 {
    now_unix_seconds + total_timeout.as_secs() + 120
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(lease_expired: bool) -> TransitionContext {
        TransitionContext {
            lease_expired,
            ..TransitionContext::default()
        }
    }

    #[test]
    fn happy_path_transitions_are_legal() {
        for (from, to) in [
            (JobState::Discovered, JobState::Fetching),
            (JobState::Fetching, JobState::Validating),
            (JobState::Validating, JobState::Storing),
            (JobState::Storing, JobState::Acquired),
            (JobState::Acquired, JobState::Extracted),
            (JobState::Extracted, JobState::Indexed),
        ] {
            assert!(
                validate_transition(from, to, &context(false)).is_ok(),
                "{from:?} → {to:?} must be legal"
            );
        }
    }

    #[test]
    fn failure_and_retry_transitions_are_legal() {
        for from in [JobState::Fetching, JobState::Validating, JobState::Storing] {
            assert!(validate_transition(from, JobState::FailedRetryable, &context(false)).is_ok());
            assert!(validate_transition(from, JobState::FailedPermanent, &context(false)).is_ok());
        }
        assert!(
            validate_transition(JobState::Validating, JobState::Quarantined, &context(false))
                .is_ok()
        );
        assert!(validate_transition(
            JobState::FailedRetryable,
            JobState::Fetching,
            &context(false)
        )
        .is_ok());
        assert!(validate_transition(
            JobState::FailedRetryable,
            JobState::FailedPermanent,
            &context(false)
        )
        .is_ok());
        assert!(
            validate_transition(JobState::Discovered, JobState::Skipped, &context(false)).is_ok()
        );
        assert!(validate_transition(
            JobState::Discovered,
            JobState::FailedPermanent,
            &context(false)
        )
        .is_ok());
        // The pre-request policy gate may refuse a target after scheduling.
        assert!(
            validate_transition(JobState::Fetching, JobState::Skipped, &context(false)).is_ok()
        );
    }

    #[test]
    fn invalid_transitions_are_rejected() {
        // Skipping stages.
        assert!(
            validate_transition(JobState::Discovered, JobState::Acquired, &context(false)).is_err()
        );
        assert!(
            validate_transition(JobState::Discovered, JobState::Validating, &context(false))
                .is_err()
        );
        assert!(
            validate_transition(JobState::Fetching, JobState::Storing, &context(false)).is_err()
        );
        assert!(
            validate_transition(JobState::Fetching, JobState::Quarantined, &context(false))
                .is_err()
        );
        // Backwards out of order.
        assert!(
            validate_transition(JobState::Validating, JobState::Discovered, &context(false))
                .is_err()
        );
        assert!(
            validate_transition(JobState::Acquired, JobState::Fetching, &context(false)).is_err()
        );
        assert!(
            validate_transition(JobState::Quarantined, JobState::Validating, &context(false))
                .is_err()
        );
        assert!(
            validate_transition(JobState::Skipped, JobState::Fetching, &context(false)).is_err()
        );
        assert!(validate_transition(
            JobState::FailedPermanent,
            JobState::Fetching,
            &context(false)
        )
        .is_err());
        // Quarantine only from validation.
        assert!(
            validate_transition(JobState::Storing, JobState::Quarantined, &context(false)).is_err()
        );
        // Terminal self-transition is not a lease refresh.
        assert!(
            validate_transition(JobState::Acquired, JobState::Acquired, &context(false)).is_err()
        );
    }

    #[test]
    fn terminal_reentry_requires_explicit_consent_or_new_target() {
        assert!(validate_transition(
            JobState::Acquired,
            JobState::Discovered,
            &TransitionContext {
                explicit_reentry: true,
                ..TransitionContext::default()
            }
        )
        .is_ok());
        assert!(validate_transition(
            JobState::FailedPermanent,
            JobState::Discovered,
            &TransitionContext {
                target_url_changed: true,
                ..TransitionContext::default()
            }
        )
        .is_ok());
        // Without consent, and to anywhere else, re-entry is refused.
        assert!(validate_transition(
            JobState::Acquired,
            JobState::Discovered,
            &TransitionContext::default()
        )
        .is_err());
        assert!(validate_transition(
            JobState::Acquired,
            JobState::Fetching,
            &TransitionContext {
                explicit_reentry: true,
                ..TransitionContext::default()
            }
        )
        .is_err());
    }

    #[test]
    fn in_flight_recovery_requires_expired_lease() {
        // Re-entering the fetch stage from a later in-flight state requires expiry.
        assert!(
            validate_transition(JobState::Validating, JobState::Fetching, &context(false)).is_err()
        );
        assert!(validate_transition(JobState::Storing, JobState::Fetching, &context(true)).is_ok());
        // A self-transition on an in-flight state is a lease refresh by its owner and
        // is always legal; on any other state it is rejected.
        assert!(
            validate_transition(JobState::Fetching, JobState::Fetching, &context(false)).is_ok()
        );
        assert!(
            validate_transition(JobState::Discovered, JobState::Discovered, &context(true))
                .is_err()
        );
        assert!(
            validate_transition(JobState::Acquired, JobState::Acquired, &context(true)).is_err()
        );
    }

    #[test]
    fn attempts_increment_on_fetch_entry_and_reset_on_reentry() {
        let mut job = JobRecord::new("arxiv", "2401.00001", "https://example.org/a.pdf", 100);
        assert!(job
            .advance(JobState::Fetching, &TransitionContext::default(), 101)
            .is_ok());
        assert_eq!(job.attempts, 1);
        assert!(job
            .fail_retryable(JobErrorClass::Transient, "boom", 60, 102)
            .is_ok());
        assert_eq!(job.next_retry_unix_seconds, Some(102 + 60));
        assert!(job
            .advance(JobState::Fetching, &TransitionContext::default(), 300)
            .is_ok());
        assert_eq!(job.attempts, 2);
        assert!(job
            .fail_permanent(JobErrorClass::Transient, "budget", 301)
            .is_ok());
        // Explicit re-entry resets the attempt series.
        assert!(job
            .advance(
                JobState::Discovered,
                &TransitionContext {
                    explicit_reentry: true,
                    ..TransitionContext::default()
                },
                400
            )
            .is_ok());
        assert_eq!(job.attempts, 0);
    }

    #[test]
    fn backoff_doubles_with_cap() {
        assert_eq!(retry_delay_seconds(60, 1), 60);
        assert_eq!(retry_delay_seconds(60, 2), 120);
        assert_eq!(retry_delay_seconds(60, 3), 240);
        assert_eq!(retry_delay_seconds(60, 10), 3_600);
        assert_eq!(retry_delay_seconds(0, 3), 0);
    }

    #[test]
    fn plan_skips_terminal_jobs_without_consent() {
        let mut job = JobRecord::new("arxiv", "id", "https://example.org/a.pdf", 100);
        job.state = JobState::Acquired;
        job.acquired_sha256_hex = Some("ab".repeat(32));
        match plan_job_action(
            job.clone(),
            Some("https://example.org/a.pdf"),
            &JobPolicy::default(),
            200,
        ) {
            JobAction::Skip(_, reason) => assert!(reason.contains("--refetch"), "{reason}"),
            other => panic!("expected skip, got {other:?}"),
        }
        // Explicit re-entry proceeds.
        let policy = JobPolicy {
            refetch: true,
            ..JobPolicy::default()
        };
        assert!(matches!(
            plan_job_action(job, Some("https://example.org/a.pdf"), &policy, 200),
            JobAction::Proceed(_)
        ));
    }

    #[test]
    fn plan_honors_backoff_and_live_leases() {
        let mut job = JobRecord::new("arxiv", "id", "https://example.org/a.pdf", 100);
        job.state = JobState::FailedRetryable;
        job.attempts = 1;
        job.next_retry_unix_seconds = Some(1_000);
        match plan_job_action(
            job.clone(),
            Some("https://example.org/a.pdf"),
            &JobPolicy::default(),
            500,
        ) {
            JobAction::Skip(_, reason) => assert!(reason.contains("retry not due"), "{reason}"),
            other => panic!("expected skip, got {other:?}"),
        }
        assert!(matches!(
            plan_job_action(
                job.clone(),
                Some("https://example.org/a.pdf"),
                &JobPolicy::default(),
                1_001
            ),
            JobAction::Proceed(_)
        ));
        // An explicit --refetch overrides the pending backoff.
        assert!(matches!(
            plan_job_action(
                job,
                Some("https://example.org/a.pdf"),
                &JobPolicy {
                    refetch: true,
                    ..JobPolicy::default()
                },
                500
            ),
            JobAction::Proceed(_)
        ));

        let mut job = JobRecord::new("arxiv", "id", "https://example.org/a.pdf", 100);
        job.state = JobState::Fetching;
        job.lease = Some(JobLease {
            owner: "acquire:pid:1:100".into(),
            expires_unix_seconds: 5_000,
        });
        match plan_job_action(
            job,
            Some("https://example.org/a.pdf"),
            &JobPolicy::default(),
            200,
        ) {
            JobAction::Skip(_, reason) => assert!(reason.contains("live lease"), "{reason}"),
            other => panic!("expected skip, got {other:?}"),
        }
    }

    #[test]
    fn changed_target_url_reopens_terminal_job() {
        let mut job = JobRecord::new("arxiv", "id", "https://example.org/a.pdf", 100);
        job.state = JobState::Acquired;
        job.acquired_sha256_hex = Some("ab".repeat(32));
        match plan_job_action(
            job,
            Some("https://example.org/b.pdf"),
            &JobPolicy::default(),
            200,
        ) {
            JobAction::Proceed(record) => {
                assert_eq!(record.target_url, "https://example.org/b.pdf");
                assert_eq!(record.state, JobState::Fetching);
                assert_eq!(record.attempts, 1);
            }
            other => panic!("expected proceed, got {other:?}"),
        }
    }

    #[test]
    fn no_target_url_yields_terminal_skip() {
        let job = JobRecord::new("crossref", "10.1/x", "", 100);
        match plan_job_action(job, None, &JobPolicy::default(), 200) {
            JobAction::Skip(record, reason) => {
                assert_eq!(record.state, JobState::Skipped);
                assert!(reason.contains("no direct OA PDF URL"));
            }
            other => panic!("expected skip, got {other:?}"),
        }
    }

    #[test]
    fn download_errors_classify_as_required() {
        use crate::fetcher::stream::DownloadError;
        let status = |code: u16| {
            DownloadError::HttpStatus(
                reqwest::StatusCode::from_u16(code).expect("valid status code"),
            )
        };
        assert_eq!(
            classify_download_error(&status(429)),
            JobErrorClass::RateLimited
        );
        assert_eq!(
            classify_download_error(&status(404)),
            JobErrorClass::NotFound
        );
        assert_eq!(
            classify_download_error(&status(410)),
            JobErrorClass::NotFound
        );
        assert_eq!(
            classify_download_error(&status(403)),
            JobErrorClass::AuthRestricted
        );
        assert_eq!(
            classify_download_error(&status(401)),
            JobErrorClass::AuthRestricted
        );
        assert_eq!(
            classify_download_error(&status(503)),
            JobErrorClass::Transient
        );
        assert_eq!(
            classify_download_error(&status(302)),
            JobErrorClass::Permanent
        );
        assert_eq!(
            classify_download_error(&DownloadError::Timeout),
            JobErrorClass::Transient
        );
        assert_eq!(
            classify_download_error(&DownloadError::TooLarge { limit: 10 }),
            JobErrorClass::Permanent
        );
        assert_eq!(
            classify_download_error(&DownloadError::InvalidSignature {
                url: "https://x/y".into(),
                prefix: b"<html>".to_vec(),
            }),
            JobErrorClass::Corrupt
        );
    }

    #[test]
    fn retryable_classes_are_transparent() {
        assert!(JobErrorClass::Transient.is_retryable());
        assert!(JobErrorClass::RateLimited.is_retryable());
        assert!(!JobErrorClass::NotFound.is_retryable());
        assert!(!JobErrorClass::AuthRestricted.is_retryable());
        assert!(!JobErrorClass::Corrupt.is_retryable());
        assert!(!JobErrorClass::Permanent.is_retryable());
    }
}
