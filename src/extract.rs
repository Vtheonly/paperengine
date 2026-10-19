//! Provenance-aware PDF metadata and text extraction over finalized library objects
//! (#31).
//!
//! Extraction is an outer-layer operation: it reads durable objects that already passed
//! structural validation and writes derived text artifacts plus provenance records. It
//! never touches the network, and it never modifies the source object.
//!
//! Layout under a library root:
//!
//! ```text
//! library/
//!   extracted/<sha256[0..2]>/<sha256>.txt   extracted UTF-8 text, atomic replace
//!   extraction/<sha256>.json                provenance record for one extraction
//!   reports/<unix>-extract.jsonl            per-run extraction reports
//! ```
//!
//! The extractor is **versioned**: every record names the extractor identity and version
//! that produced it, so downstream consumers (indexing, search) can distinguish outputs
//! when the extractor changes. Extraction is deterministic for one object and extractor
//! version: re-running with `--force` produces byte-identical text.
//!
//! Text extraction uses the real PDF parser (lopdf): it walks page content streams,
//! resolves each page's font encodings, and decodes text operators. It is not a
//! byte-marker scan. Its known limits are recorded honestly in each record: pages whose
//! fonts or encodings cannot be resolved surface as bounded per-page notes, image-only
//! pages contribute no text, and layout is not reconstructed.
//!
//! Objects are content-addressed: the object's file name is its SHA-256. Extraction
//! re-hashes the bytes it reads and refuses an object whose content does not match its
//! address, so a misplaced or silently corrupted object can never produce an extraction
//! record attributed to a different object.

use crate::library::{format_rfc3339_utc, LibraryError, LibraryLayout};
use lopdf::{Document, Object};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;

pub const EXTRACTION_SCHEMA_VERSION: u16 = 1;
/// Stable identity of the text extractor implementation. Change the version whenever
/// the extracted text for the same input could change.
pub const EXTRACTOR_ID: &str = "paperengine/lopdf-text";
pub const EXTRACTOR_VERSION: u16 = 1;

/// Hard bound on per-page error notes kept in one record.
const MAX_PAGE_NOTES: usize = 32;
/// Length cap of a per-page note's detail text.
const MAX_NOTE_CHARS: usize = 160;
/// Defensive in-memory cap while iterating pages, far above any sane text budget; the
/// configured budget is what actually applies before anything is persisted.
const PER_RUN_PAGE_LOOP_TEXT_CAP: usize = 512 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum ExtractionError {
    #[error("filesystem operation failed: {0}")]
    Io(#[from] io::Error),
    #[error("record serialization failed: {0}")]
    Manifest(#[from] serde_json::Error),
    #[error("invalid extraction configuration: {0}")]
    InvalidConfig(String),
    #[error("invalid library layout: {0}")]
    Layout(String),
    #[error("PDF parse failed for `{path}`: {detail}")]
    Parse { path: PathBuf, detail: String },
    #[error("object `{path}` is encrypted; extraction refuses to guess passwords")]
    Encrypted { path: PathBuf },
    #[error("object content does not match its content-addressed name `{expected}`")]
    AddressMismatch { path: PathBuf, expected: String },
}

impl From<LibraryError> for ExtractionError {
    fn from(error: LibraryError) -> Self {
        ExtractionError::Layout(error.to_string())
    }
}

/// Resource budgets for one extraction run.
#[derive(Debug, Clone, Copy)]
pub struct ExtractionConfig {
    /// Upper bound of extracted text bytes persisted for one object.
    pub max_text_bytes: u64,
    /// Upper bound of objects processed in one run.
    pub max_objects: usize,
}

impl Default for ExtractionConfig {
    fn default() -> Self {
        Self {
            max_text_bytes: 16 * 1024 * 1024,
            max_objects: 256,
        }
    }
}

impl ExtractionConfig {
    pub fn validate(&self) -> Result<(), ExtractionError> {
        if self.max_text_bytes == 0 {
            return Err(ExtractionError::InvalidConfig(
                "max_text_bytes must be positive".into(),
            ));
        }
        if self.max_objects == 0 {
            return Err(ExtractionError::InvalidConfig(
                "max_objects must be positive".into(),
            ));
        }
        Ok(())
    }
}

/// Document-level PDF metadata from the trailer's `/Info` dictionary. All fields are
/// optional: producers frequently omit them, and absent metadata is recorded as absent
/// rather than invented. Strings are decoded lossily (PDF `DocInfo` strings are not
/// guaranteed UTF-8); replacement characters may appear for legacy encodings.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PdfDocumentInfo {
    pub title: Option<String>,
    pub author: Option<String>,
    pub subject: Option<String>,
    pub keywords: Option<String>,
    pub creator: Option<String>,
    pub producer: Option<String>,
    pub creation_date: Option<String>,
    pub mod_date: Option<String>,
}

/// Status of one completed extraction. Only completed outcomes are persisted as
/// records; parse failures are reported per-run and retried on the next execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExtractionStatus {
    /// Text was extracted from at least one page.
    Extracted {
        char_count: usize,
        pages_with_text: usize,
        pages_total: usize,
        /// True when the persisted text was cut at `max_text_bytes`.
        truncated: bool,
    },
    /// The parser accepted the object but no page yielded any text (e.g. image-only
    /// scans). This is an honest outcome, not a failure.
    Empty { pages_total: usize },
}

impl ExtractionStatus {
    /// Coarse confidence statement for downstream consumers: did we get usable text?
    pub fn has_text(&self) -> bool {
        matches!(self, ExtractionStatus::Extracted { .. })
    }
}

/// Provenance record persisted for one extraction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtractionRecord {
    pub schema_version: u16,
    pub extractor: ExtractorIdentity,
    pub object: ExtractedObjectRef,
    pub document_info: PdfDocumentInfo,
    pub text: TextArtifact,
    pub status: ExtractionStatus,
    /// Bounded per-page extraction notes (failures the parser tolerated per page).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub page_notes: Vec<String>,
    pub extraction_unix_seconds: u64,
    pub extraction_rfc3339: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtractorIdentity {
    pub id: String,
    pub version: u16,
    /// PaperEngine build that produced the extraction.
    pub paperengine_version: String,
}

/// Reference to the immutable source object the text was derived from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtractedObjectRef {
    pub sha256_hex: String,
    pub byte_length: u64,
    /// Path relative to the library root.
    pub object_path: String,
    pub page_count: usize,
    pub pdf_version: String,
}

/// The derived text artifact and its integrity information.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TextArtifact {
    /// Path relative to the library root.
    pub path: String,
    pub byte_length: u64,
    pub sha256_hex: String,
}

/// Per-object outcome written into the run report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionReportEntry {
    pub schema_version: u16,
    pub sha256_hex: String,
    pub object_path: String,
    pub outcome: ExtractionOutcome,
    pub extraction_unix_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExtractionOutcome {
    Extracted {
        char_count: usize,
        pages_with_text: usize,
        pages_total: usize,
        truncated: bool,
        text_path: String,
        text_sha256_hex: String,
        record_path: String,
    },
    Empty {
        pages_total: usize,
        record_path: String,
    },
    Skipped {
        reason: String,
    },
    Failed {
        stage: String,
        detail: String,
    },
}

/// Run-wide accounting returned by [`run_extraction`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractionRunSummary {
    pub objects_examined: usize,
    pub extracted: usize,
    pub empty: usize,
    pub failed: usize,
    pub skipped: usize,
    pub total_text_bytes: u64,
}

/// In-memory result of extracting one object; persisted by [`finalize_extraction`].
#[derive(Debug, Clone)]
struct ExtractedPayload {
    sha256_hex: String,
    byte_length: u64,
    text: String,
    pages_total: usize,
    pages_with_text: usize,
    truncated: bool,
    page_notes: Vec<String>,
    document_info: PdfDocumentInfo,
    pdf_version: String,
}

/// Extract text and metadata from one finalized object and verify its content address.
/// Deterministic for one input file: no clock, randomness, or environment is consulted.
fn extract_object(object_path: &Path) -> Result<ExtractedPayload, ExtractionError> {
    let expected = sha_from_object_path(object_path);
    let bytes = fs::read(object_path)?;
    let byte_length = bytes.len() as u64;
    let sha256_hex = format!("{:x}", Sha256::digest(&bytes));

    let document = Document::load(object_path).map_err(|error| ExtractionError::Parse {
        path: object_path.to_owned(),
        detail: error.to_string(),
    })?;
    if document.is_encrypted() {
        return Err(ExtractionError::Encrypted {
            path: object_path.to_owned(),
        });
    }

    let pdf_version = document.version.clone();
    let document_info = read_document_info(&document);
    let pages: Vec<u32> = document.get_pages().keys().copied().collect();
    let pages_total = pages.len();

    let mut text = String::new();
    let mut pages_with_text = 0_usize;
    let mut truncated = false;
    let mut page_notes = Vec::new();

    for page_number in pages {
        if text.len() >= PER_RUN_PAGE_LOOP_TEXT_CAP {
            truncated = true;
            break;
        }
        match document.extract_text(&[page_number]) {
            Ok(page_text) => {
                if !page_text.is_empty() {
                    pages_with_text += 1;
                    text.push_str(&page_text);
                    if !text.ends_with('\n') {
                        text.push('\n');
                    }
                }
            }
            Err(error) => {
                if page_notes.len() < MAX_PAGE_NOTES {
                    page_notes.push(truncate_chars(
                        &format!("page {page_number}: {error}"),
                        MAX_NOTE_CHARS,
                    ));
                }
            }
        }
    }

    if let Some(expected) = expected {
        if !expected.eq_ignore_ascii_case(&sha256_hex) {
            return Err(ExtractionError::AddressMismatch {
                path: object_path.to_owned(),
                expected,
            });
        }
    }

    Ok(ExtractedPayload {
        sha256_hex,
        byte_length,
        text,
        pages_total,
        pages_with_text,
        truncated,
        page_notes,
        document_info,
        pdf_version,
    })
}

/// Read the trailer `/Info` dictionary into normalized metadata.
fn read_document_info(document: &Document) -> PdfDocumentInfo {
    let mut info = PdfDocumentInfo::default();
    let Ok(Object::Reference(info_id)) = document.trailer.get(b"Info") else {
        return info;
    };
    let Ok(Object::Dictionary(dictionary)) = document.get_object(*info_id) else {
        return info;
    };
    let decode = |key: &[u8]| -> Option<String> {
        match dictionary.get(key) {
            Ok(Object::String(bytes, _)) => {
                let value = String::from_utf8_lossy(bytes).trim().to_owned();
                (!value.is_empty()).then_some(value)
            }
            _ => None,
        }
    };
    info.title = decode(b"Title");
    info.author = decode(b"Author");
    info.subject = decode(b"Subject");
    info.keywords = decode(b"Keywords");
    info.creator = decode(b"Creator");
    info.producer = decode(b"Producer");
    info.creation_date = decode(b"CreationDate");
    info.mod_date = decode(b"ModDate");
    info
}

/// Persist one extraction: text artifact first (atomic replace), then the record.
/// A crash between the two leaves at most an orphan text artifact, which the next run
/// rewrites deterministically; a record never exists without its text artifact.
fn finalize_extraction(
    library: &LibraryLayout,
    payload: &ExtractedPayload,
    config: &ExtractionConfig,
    now_unix_seconds: u64,
) -> Result<(ExtractionRecord, PathBuf), ExtractionError> {
    let text_bytes = payload.text.as_bytes();
    if text_bytes.len() as u64 > config.max_text_bytes {
        // The caller truncates before persisting; this is a defensive invariant.
        return Err(ExtractionError::InvalidConfig(format!(
            "extracted text exceeds the configured budget ({} bytes)",
            text_bytes.len()
        )));
    }
    let text_sha256_hex = format!("{:x}", Sha256::digest(text_bytes));

    let text_rel = format!(
        "extracted/{}/{}.txt",
        &payload.sha256_hex[..2],
        payload.sha256_hex
    );
    let text_path = library.root().join(&text_rel);
    if let Some(parent) = text_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let scratch = library
        .tmp_dir()
        .join(format!("{}.txt", payload.sha256_hex));
    fs::write(&scratch, text_bytes)?;
    atomic_replace(&scratch, &text_path)?;

    let status = if payload.pages_with_text > 0 {
        ExtractionStatus::Extracted {
            char_count: payload.text.chars().count(),
            pages_with_text: payload.pages_with_text,
            pages_total: payload.pages_total,
            truncated: payload.truncated,
        }
    } else {
        ExtractionStatus::Empty {
            pages_total: payload.pages_total,
        }
    };

    let object_rel = format!(
        "objects/{}/{}.pdf",
        &payload.sha256_hex[..2],
        payload.sha256_hex
    );
    let record = ExtractionRecord {
        schema_version: EXTRACTION_SCHEMA_VERSION,
        extractor: ExtractorIdentity {
            id: EXTRACTOR_ID.to_owned(),
            version: EXTRACTOR_VERSION,
            paperengine_version: env!("CARGO_PKG_VERSION").to_owned(),
        },
        object: ExtractedObjectRef {
            sha256_hex: payload.sha256_hex.clone(),
            byte_length: payload.byte_length,
            object_path: object_rel,
            page_count: payload.pages_total,
            pdf_version: payload.pdf_version.clone(),
        },
        document_info: payload.document_info.clone(),
        text: TextArtifact {
            path: text_rel,
            byte_length: text_bytes.len() as u64,
            sha256_hex: text_sha256_hex,
        },
        status,
        page_notes: payload.page_notes.clone(),
        extraction_unix_seconds: now_unix_seconds,
        extraction_rfc3339: format_rfc3339_utc(now_unix_seconds),
    };

    let record_path = library
        .extraction_dir()
        .join(format!("{}.json", payload.sha256_hex));
    if let Some(parent) = record_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let serialized = serde_json::to_vec_pretty(&record)?;
    let record_scratch = library
        .tmp_dir()
        .join(format!("{}.json", payload.sha256_hex));
    fs::write(&record_scratch, serialized)?;
    atomic_replace(&record_scratch, &record_path)?;

    Ok((record, record_path))
}

/// `fs::rename` on the same filesystem atomically replaces the destination on Unix,
/// then the parent directory is synced so the rename itself is durable.
fn atomic_replace(source: &Path, destination: &Path) -> Result<(), ExtractionError> {
    fs::rename(source, destination)?;
    if let Some(parent) = destination.parent() {
        let handle = fs::File::open(parent)?;
        handle.sync_all()?;
    }
    Ok(())
}

/// Run the extraction pipeline over the library's finalized objects, invoking
/// `on_entry` for each report entry so callers can persist JSONL incrementally.
///
/// Objects with an existing completed record are skipped unless `force` is set;
/// parse failures are retried on the next run by design.
pub fn run_extraction(
    library: &LibraryLayout,
    config: &ExtractionConfig,
    force: bool,
    now_unix_seconds: u64,
    mut on_entry: impl FnMut(&ExtractionReportEntry) -> Result<(), io::Error>,
) -> Result<ExtractionRunSummary, ExtractionError> {
    config.validate()?;
    library.create()?;

    let objects = library.list_objects()?;

    let mut summary = ExtractionRunSummary {
        objects_examined: 0,
        extracted: 0,
        empty: 0,
        failed: 0,
        skipped: 0,
        total_text_bytes: 0,
    };

    for (object_path, _byte_length) in objects {
        let object_display = object_path.to_string_lossy().into_owned();
        let address = sha_from_object_path(&object_path).unwrap_or_default();

        if summary.objects_examined >= config.max_objects {
            summary.skipped += 1;
            on_entry(&ExtractionReportEntry {
                schema_version: EXTRACTION_SCHEMA_VERSION,
                sha256_hex: address,
                object_path: object_display,
                outcome: ExtractionOutcome::Skipped {
                    reason: "run object budget exhausted".to_owned(),
                },
                extraction_unix_seconds: now_unix_seconds,
            })?;
            continue;
        }

        summary.objects_examined += 1;
        let record_path = library.extraction_dir().join(format!("{address}.json"));
        if !force && !address.is_empty() && record_path.exists() {
            summary.skipped += 1;
            on_entry(&ExtractionReportEntry {
                schema_version: EXTRACTION_SCHEMA_VERSION,
                sha256_hex: address,
                object_path: object_display,
                outcome: ExtractionOutcome::Skipped {
                    reason: "extraction record already exists (use --force to re-extract)"
                        .to_owned(),
                },
                extraction_unix_seconds: now_unix_seconds,
            })?;
            continue;
        }

        let outcome = match extract_object(&object_path) {
            Err(error) => ExtractionOutcome::Failed {
                stage: "parse".to_owned(),
                detail: error.to_string(),
            },
            Ok(mut payload) => {
                // Apply the configured text budget deterministically before persisting.
                if payload.text.len() as u64 > config.max_text_bytes {
                    payload.truncated = true;
                    payload.text = truncate_char_boundary(&payload.text, config.max_text_bytes);
                }
                match finalize_extraction(library, &payload, config, now_unix_seconds) {
                    Ok((record, record_path)) => match &record.status {
                        ExtractionStatus::Extracted {
                            char_count,
                            pages_with_text,
                            pages_total,
                            truncated,
                        } => {
                            summary.extracted += 1;
                            summary.total_text_bytes += record.text.byte_length;
                            ExtractionOutcome::Extracted {
                                char_count: *char_count,
                                pages_with_text: *pages_with_text,
                                pages_total: *pages_total,
                                truncated: *truncated,
                                text_path: record.text.path.clone(),
                                text_sha256_hex: record.text.sha256_hex.clone(),
                                record_path: record_path.to_string_lossy().into_owned(),
                            }
                        }
                        ExtractionStatus::Empty { pages_total } => {
                            summary.empty += 1;
                            ExtractionOutcome::Empty {
                                pages_total: *pages_total,
                                record_path: record_path.to_string_lossy().into_owned(),
                            }
                        }
                    },
                    Err(error) => ExtractionOutcome::Failed {
                        stage: "persist".to_owned(),
                        detail: error.to_string(),
                    },
                }
            }
        };

        if matches!(outcome, ExtractionOutcome::Failed { .. }) {
            summary.failed += 1;
        }
        on_entry(&ExtractionReportEntry {
            schema_version: EXTRACTION_SCHEMA_VERSION,
            sha256_hex: address,
            object_path: object_display,
            outcome,
            extraction_unix_seconds: now_unix_seconds,
        })?;
    }

    Ok(summary)
}

/// Derive the object's content address from its content-addressed file name.
fn sha_from_object_path(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    if stem.len() == 64 && stem.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(stem.to_ascii_lowercase())
    } else {
        None
    }
}

/// Truncate to a byte bound without splitting a UTF-8 code point.
fn truncate_char_boundary(value: &str, max_bytes: u64) -> String {
    if (value.len() as u64) <= max_bytes {
        return value.to_owned();
    }
    let mut end = (max_bytes as usize).min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    value
        .char_indices()
        .nth(max_chars)
        .map(|(index, _)| value[..index].to_owned())
        .unwrap_or_else(|| value.to_owned())
}

/// Read one extraction record by object digest, for CLI inspection and indexing.
pub fn read_record(
    library: &LibraryLayout,
    sha256_hex: &str,
) -> Result<Option<ExtractionRecord>, ExtractionError> {
    if sha256_hex.len() != 64 || !sha256_hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(ExtractionError::InvalidConfig(format!(
            "not a SHA-256 hex digest: {sha256_hex}"
        )));
    }
    let path = library
        .extraction_dir()
        .join(format!("{}.json", sha256_hex.to_ascii_lowercase()));
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(path)?;
    Ok(Some(serde_json::from_str(&content)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_respects_char_boundaries() {
        let value = "aé漢字".repeat(64);
        let truncated = truncate_char_boundary(&value, 10);
        assert!(truncated.len() <= 10);
        assert!(value.starts_with(&truncated));
    }

    #[test]
    fn note_truncation_is_bounded() {
        let long = "x".repeat(500);
        let truncated = truncate_chars(&long, MAX_NOTE_CHARS);
        assert_eq!(truncated.chars().count(), MAX_NOTE_CHARS);
    }

    #[test]
    fn content_address_is_derived_from_the_file_name() {
        let address = "abcdef0123456789".repeat(4); // 64 hex chars
        let path = Path::new("/library/objects/ab").join(format!("{address}.pdf"));
        assert_eq!(
            sha_from_object_path(&path).as_deref(),
            Some(address.as_str())
        );
        assert_eq!(sha_from_object_path(Path::new("/tmp/not-a-hash.pdf")), None);
    }

    #[test]
    fn config_validation_rejects_zero_budgets() {
        assert!(ExtractionConfig {
            max_text_bytes: 0,
            max_objects: 1
        }
        .validate()
        .is_err());
        assert!(ExtractionConfig {
            max_text_bytes: 1,
            max_objects: 0
        }
        .validate()
        .is_err());
    }
}
