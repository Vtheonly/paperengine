//! Structural PDF validation and quarantine at the outer I/O boundary.
//!
//! The downloader's PDF signature check is an early bandwidth guard, not proof of validity.
//! This module applies length, MIME, signature, EOF, checksum, parser, page-tree and encryption
//! checks. Only Valid outcomes may be passed to a later indexer; rejected objects are moved into
//! quarantine with a provenance manifest.

use lopdf::Document;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

const PDF_HEADER_SCAN_BYTES: usize = 1024;
const PDF_EOF_SCAN_BYTES: usize = 4096;
const HASH_BUFFER_BYTES: usize = 64 * 1024;
const PDF_SIGNATURE: &[u8] = b"%PDF-";
const PDF_EOF_MARKER: &[u8] = b"%%EOF";
const PARSER_NAME: &str = "lopdf";
const PARSER_VERSION: &str = "0.36.0";
const VALIDATOR_SCHEMA_VERSION: u16 = 1;
static QUARANTINE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct PdfValidationConfig {
    /// Smallest accepted representation, in bytes.
    pub min_bytes: u64,
    /// Largest representation passed to the structural parser, in bytes.
    ///
    /// lopdf builds an in-memory object graph, so this cap also limits parser memory demand.
    pub max_bytes: u64,
}

impl Default for PdfValidationConfig {
    fn default() -> Self {
        Self {
            min_bytes: 1024,
            max_bytes: 512 * 1024 * 1024,
        }
    }
}

impl PdfValidationConfig {
    fn validate(&self) -> Result<(), PdfValidationError> {
        if self.min_bytes == 0 {
            return Err(PdfValidationError::InvalidConfig(
                "min_bytes must be positive",
            ));
        }
        if self.max_bytes == 0 {
            return Err(PdfValidationError::InvalidConfig(
                "max_bytes must be positive",
            ));
        }
        if self.min_bytes > self.max_bytes {
            return Err(PdfValidationError::InvalidConfig(
                "min_bytes must not exceed max_bytes",
            ));
        }
        Ok(())
    }
}

/// Facts supplied by the acquisition layer. A URL is reduced to scheme/host/port in quarantine
/// manifests; username, password, path, query and fragment are not persisted.
#[derive(Debug, Clone, Default)]
pub struct PdfValidationContext {
    pub source_url: Option<String>,
    pub declared_content_type: Option<String>,
    pub expected_blake3_hex: Option<String>,
    pub expected_sha256_hex: Option<String>,
}

#[derive(Debug, Error)]
pub enum PdfValidationError {
    #[error("filesystem operation failed: {0}")]
    Io(#[from] io::Error),
    #[error("quarantine manifest serialization failed: {0}")]
    ManifestSerialization(#[from] serde_json::Error),
    #[error("invalid PDF validation configuration: {0}")]
    InvalidConfig(&'static str),
    #[error("validation input is not a regular file: {0}")]
    NotRegularFile(PathBuf),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PdfRejectionReason {
    TooSmall {
        size_bytes: u64,
        minimum_bytes: u64,
    },
    TooLarge {
        size_bytes: u64,
        maximum_bytes: u64,
    },
    DeclaredMimeNotPdf {
        content_type: String,
    },
    MissingPdfSignature {
        prefix_hex: String,
    },
    MissingEofMarker,
    ChecksumMismatch {
        algorithm: String,
    },
    FileChangedDuringHash {
        metadata_bytes: u64,
        hashed_bytes: u64,
    },
    ParserRejected {
        detail: String,
    },
    ParserPanicked,
    EncryptedDocument,
    MissingCatalog,
    EmptyPageTree,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PdfValidationReport {
    pub byte_length: u64,
    pub blake3_hex: String,
    pub sha256_hex: String,
    pub page_count: usize,
    pub pdf_version: String,
    pub declared_content_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuarantineManifest {
    pub schema_version: u16,
    pub source_file_name: String,
    pub quarantined_file_name: String,
    /// Scheme + host + port only; no user info, path, query, or fragment.
    pub source_origin: Option<String>,
    pub declared_content_type: Option<String>,
    pub expected_blake3_hex: Option<String>,
    pub expected_sha256_hex: Option<String>,
    pub observed_blake3_hex: Option<String>,
    pub observed_sha256_hex: Option<String>,
    pub size_bytes: u64,
    pub reason: PdfRejectionReason,
    pub detected_at_unix_seconds: u64,
    pub parser: String,
    pub parser_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantinedPdf {
    pub quarantined_path: PathBuf,
    pub manifest_path: PathBuf,
    pub manifest: QuarantineManifest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PdfValidationOutcome {
    Valid(PdfValidationReport),
    /// Boxed: `QuarantinedPdf` is much larger than `PdfValidationReport`, and boxing keeps the
    /// outcome enum small on the hot validation path.
    Quarantined(Box<QuarantinedPdf>),
}

#[derive(Debug, Clone)]
struct FileChecksums {
    blake3_hex: String,
    sha256_hex: String,
    bytes_hashed: u64,
}

#[derive(Debug, Clone)]
struct ParsedPdf {
    page_count: usize,
    version: String,
}

/// Validate a downloaded representation. Invalid inputs are moved to quarantine and accompanied
/// by a durable JSON manifest with reason and available checksums. A caller must only submit
/// PdfValidationOutcome::Valid reports to an indexing pipeline.
pub fn validate_or_quarantine_pdf(
    source_path: impl AsRef<Path>,
    quarantine_dir: impl AsRef<Path>,
    context: PdfValidationContext,
    config: PdfValidationConfig,
) -> Result<PdfValidationOutcome, PdfValidationError> {
    config.validate()?;
    let source_path = source_path.as_ref();
    let quarantine_dir = quarantine_dir.as_ref();

    let metadata = fs::symlink_metadata(source_path)?;
    if !metadata.file_type().is_file() {
        return Err(PdfValidationError::NotRegularFile(
            source_path.to_path_buf(),
        ));
    }
    let initial_size = metadata.len();

    // Do not hash or parse an oversized payload. The stat size is enough to quarantine it without
    // spending CPU on a file which already violates the configured safety budget.
    let checksums = if initial_size <= config.max_bytes {
        Some(hash_file(source_path)?)
    } else {
        None
    };

    let mut rejection = None;
    let mut parsed_pdf = None;

    if initial_size > config.max_bytes {
        rejection = Some(PdfRejectionReason::TooLarge {
            size_bytes: initial_size,
            maximum_bytes: config.max_bytes,
        });
    } else if initial_size < config.min_bytes {
        rejection = Some(PdfRejectionReason::TooSmall {
            size_bytes: initial_size,
            minimum_bytes: config.min_bytes,
        });
    } else if let Some(content_type) = unsupported_mime(context.declared_content_type.as_deref()) {
        rejection = Some(PdfRejectionReason::DeclaredMimeNotPdf { content_type });
    } else {
        let prefix = read_prefix(source_path, PDF_HEADER_SCAN_BYTES)?;
        if !contains_window(&prefix, PDF_SIGNATURE) {
            rejection = Some(PdfRejectionReason::MissingPdfSignature {
                prefix_hex: hex_preview(&prefix, 32),
            });
        } else {
            let tail = read_tail(source_path, initial_size, PDF_EOF_SCAN_BYTES)?;
            if !contains_window(&tail, PDF_EOF_MARKER) {
                rejection = Some(PdfRejectionReason::MissingEofMarker);
            } else if let Some(hash) = checksums.as_ref() {
                if hash.bytes_hashed != initial_size {
                    rejection = Some(PdfRejectionReason::FileChangedDuringHash {
                        metadata_bytes: initial_size,
                        hashed_bytes: hash.bytes_hashed,
                    });
                } else if context
                    .expected_blake3_hex
                    .as_deref()
                    .is_some_and(|expected| !expected.eq_ignore_ascii_case(&hash.blake3_hex))
                {
                    rejection = Some(PdfRejectionReason::ChecksumMismatch {
                        algorithm: "BLAKE3".to_owned(),
                    });
                } else if context
                    .expected_sha256_hex
                    .as_deref()
                    .is_some_and(|expected| !expected.eq_ignore_ascii_case(&hash.sha256_hex))
                {
                    rejection = Some(PdfRejectionReason::ChecksumMismatch {
                        algorithm: "SHA-256".to_owned(),
                    });
                } else {
                    match inspect_pdf(source_path) {
                        Ok(parsed) => parsed_pdf = Some(parsed),
                        Err(reason) => rejection = Some(reason),
                    }
                }
            } else {
                rejection = Some(PdfRejectionReason::TooLarge {
                    size_bytes: initial_size,
                    maximum_bytes: config.max_bytes,
                });
            }
        }
    }

    if let Some(reason) = rejection {
        return quarantine_file(
            source_path,
            quarantine_dir,
            context,
            initial_size,
            checksums,
            reason,
        )
        .map(|quarantined| PdfValidationOutcome::Quarantined(Box::new(quarantined)));
    }

    let final_metadata = fs::metadata(source_path)?;
    if final_metadata.len() != initial_size {
        return quarantine_file(
            source_path,
            quarantine_dir,
            context,
            final_metadata.len(),
            None,
            PdfRejectionReason::FileChangedDuringHash {
                metadata_bytes: initial_size,
                hashed_bytes: final_metadata.len(),
            },
        )
        .map(|quarantined| PdfValidationOutcome::Quarantined(Box::new(quarantined)));
    }

    let checksums = checksums.expect("non-oversized input must have checksums");
    let parsed = parsed_pdf.expect("no rejection means structural parser accepted the PDF");
    Ok(PdfValidationOutcome::Valid(PdfValidationReport {
        byte_length: initial_size,
        blake3_hex: checksums.blake3_hex,
        sha256_hex: checksums.sha256_hex,
        page_count: parsed.page_count,
        pdf_version: parsed.version,
        declared_content_type: normalized_mime(context.declared_content_type.as_deref()),
    }))
}

fn inspect_pdf(path: &Path) -> Result<ParsedPdf, PdfRejectionReason> {
    let loaded = catch_unwind(AssertUnwindSafe(|| Document::load(path)));
    let document = match loaded {
        Ok(Ok(document)) => document,
        Ok(Err(error)) => {
            return Err(PdfRejectionReason::ParserRejected {
                detail: truncate_detail(&error.to_string(), 512),
            })
        }
        Err(_) => return Err(PdfRejectionReason::ParserPanicked),
    };

    // Check the helper and the raw trailer entry. A malformed Encrypt entry must not be accepted
    // simply because it cannot be resolved as a normal indirect reference.
    if document.is_encrypted() || document.trailer.get(b"Encrypt").is_ok() {
        return Err(PdfRejectionReason::EncryptedDocument);
    }
    if document.catalog().is_err() {
        return Err(PdfRejectionReason::MissingCatalog);
    }

    let page_count = document.get_pages().len();
    if page_count == 0 {
        return Err(PdfRejectionReason::EmptyPageTree);
    }

    Ok(ParsedPdf {
        page_count,
        version: document.version,
    })
}

fn hash_file(path: &Path) -> Result<FileChecksums, PdfValidationError> {
    let mut file = File::open(path)?;
    let mut blake3_hasher = blake3::Hasher::new();
    let mut sha256_hasher = Sha256::new();
    let mut buffer = [0_u8; HASH_BUFFER_BYTES];
    let mut bytes_hashed = 0_u64;

    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        blake3_hasher.update(&buffer[..count]);
        sha256_hasher.update(&buffer[..count]);
        bytes_hashed = bytes_hashed.saturating_add(count as u64);
    }

    Ok(FileChecksums {
        blake3_hex: blake3_hasher.finalize().to_hex().to_string(),
        sha256_hex: format!("{:x}", sha256_hasher.finalize()),
        bytes_hashed,
    })
}

fn read_prefix(path: &Path, limit: usize) -> Result<Vec<u8>, PdfValidationError> {
    let mut buffer = Vec::with_capacity(limit);
    File::open(path)?
        .take(limit as u64)
        .read_to_end(&mut buffer)?;
    Ok(buffer)
}

fn read_tail(path: &Path, size: u64, limit: usize) -> Result<Vec<u8>, PdfValidationError> {
    let mut file = File::open(path)?;
    let count = size.min(limit as u64) as usize;
    file.seek(SeekFrom::End(-(count as i64)))?;
    let mut buffer = vec![0_u8; count];
    file.read_exact(&mut buffer)?;
    Ok(buffer)
}

fn contains_window(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn unsupported_mime(content_type: Option<&str>) -> Option<String> {
    let normalized = normalized_mime(content_type)?;
    match normalized.as_str() {
        "application/pdf"
        | "application/x-pdf"
        | "application/acrobat"
        | "application/octet-stream"
        | "binary/octet-stream" => None,
        other => Some(other.to_owned()),
    }
}

fn normalized_mime(content_type: Option<&str>) -> Option<String> {
    let value = content_type?.split(';').next()?.trim().to_ascii_lowercase();
    if value.is_empty() {
        return None;
    }
    Some(value.chars().take(128).collect())
}

fn hex_preview(bytes: &[u8], max_bytes: usize) -> String {
    bytes
        .iter()
        .take(max_bytes)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn truncate_detail(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn safe_source_origin(raw_url: Option<&str>) -> Option<String> {
    let parsed = reqwest::Url::parse(raw_url?).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    Some(parsed.origin().ascii_serialization())
}

fn unix_seconds_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn quarantine_file(
    source_path: &Path,
    quarantine_dir: &Path,
    context: PdfValidationContext,
    size_bytes: u64,
    checksums: Option<FileChecksums>,
    reason: PdfRejectionReason,
) -> Result<QuarantinedPdf, PdfValidationError> {
    fs::create_dir_all(quarantine_dir)?;

    let timestamp = unix_seconds_now();
    let hash_prefix = checksums
        .as_ref()
        .map(|value| value.blake3_hex.chars().take(16).collect::<String>())
        .unwrap_or_else(|| "unhashed".to_owned());
    let source_file_name = source_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unnamed-payload")
        .chars()
        .take(255)
        .collect::<String>();

    let (quarantined_path, manifest_path, pending_manifest_path) = loop {
        let sequence = QUARANTINE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let file_name = format!(
            "q-{hash_prefix}-{timestamp}-{}-{sequence}.pdf",
            std::process::id()
        );
        let payload_path = quarantine_dir.join(&file_name);
        let metadata_path = payload_path.with_extension("json");
        let pending_path = append_suffix(&metadata_path, ".pending");
        if !payload_path.exists() && !metadata_path.exists() && !pending_path.exists() {
            break (payload_path, metadata_path, pending_path);
        }
    };

    let manifest = QuarantineManifest {
        schema_version: VALIDATOR_SCHEMA_VERSION,
        source_file_name,
        quarantined_file_name: quarantined_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("quarantined.pdf")
            .to_owned(),
        source_origin: safe_source_origin(context.source_url.as_deref()),
        declared_content_type: normalized_mime(context.declared_content_type.as_deref()),
        expected_blake3_hex: context.expected_blake3_hex,
        expected_sha256_hex: context.expected_sha256_hex,
        observed_blake3_hex: checksums.as_ref().map(|value| value.blake3_hex.clone()),
        observed_sha256_hex: checksums.as_ref().map(|value| value.sha256_hex.clone()),
        size_bytes,
        reason,
        detected_at_unix_seconds: timestamp,
        parser: PARSER_NAME.to_owned(),
        parser_version: PARSER_VERSION.to_owned(),
    };

    // Create a durable pending manifest before moving the payload. If a crash happens between
    // payload quarantine and the final manifest rename, the pending record remains inspectable.
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    let mut pending_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&pending_manifest_path)?;
    if let Err(error) = pending_file.write_all(&manifest_bytes) {
        let _ = fs::remove_file(&pending_manifest_path);
        return Err(error.into());
    }
    if let Err(error) = pending_file.sync_all() {
        let _ = fs::remove_file(&pending_manifest_path);
        return Err(error.into());
    }
    drop(pending_file);

    if let Err(error) = move_file_without_clobber(source_path, &quarantined_path) {
        let _ = fs::remove_file(&pending_manifest_path);
        return Err(error.into());
    }

    fs::rename(&pending_manifest_path, &manifest_path)?;
    sync_directory(quarantine_dir)?;

    Ok(QuarantinedPdf {
        quarantined_path,
        manifest_path,
        manifest,
    })
}

fn move_file_without_clobber(source: &Path, destination: &Path) -> io::Result<()> {
    match fs::hard_link(source, destination) {
        Ok(()) => {
            if let Err(error) = fs::remove_file(source) {
                let _ = fs::remove_file(destination);
                return Err(error);
            }
            Ok(())
        }
        Err(link_error) => {
            // Cross-device or hard-link-restricted layouts use a streamed copy to a create_new
            // destination. The source is removed only after the complete copy is durable.
            if destination.exists() {
                return Err(link_error);
            }
            let mut input = File::open(source)?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(destination)?;
            if let Err(error) = io::copy(&mut input, &mut output) {
                let _ = fs::remove_file(destination);
                return Err(error);
            }
            if let Err(error) = output.sync_all() {
                let _ = fs::remove_file(destination);
                return Err(error);
            }
            drop(output);
            if let Err(error) = fs::remove_file(source) {
                let _ = fs::remove_file(destination);
                return Err(error);
            }
            Ok(())
        }
    }
}

fn sync_directory(path: &Path) -> Result<(), PdfValidationError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::dictionary;
    use lopdf::{
        Dictionary, EncryptionState, EncryptionVersion, Object, Permissions, Stream, StringFormat,
    };
    use tempfile::tempdir;

    fn write_pdf(path: &Path) {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let content_id = document.add_object(Stream::new(Dictionary::new(), b"q Q\n".repeat(512)));
        let page_id = document.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            "Resources" => dictionary! {},
        });
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
            }),
        );
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        document.trailer.set("Root", catalog_id);
        document.save(path).unwrap();
    }

    fn test_config() -> PdfValidationConfig {
        PdfValidationConfig {
            min_bytes: 1,
            max_bytes: 16 * 1024 * 1024,
        }
    }

    fn validate(
        path: &Path,
        quarantine: &Path,
        context: PdfValidationContext,
    ) -> PdfValidationOutcome {
        validate_or_quarantine_pdf(path, quarantine, context, test_config()).unwrap()
    }

    #[test]
    fn accepts_a_structurally_valid_pdf_and_computes_checksums() {
        let directory = tempdir().unwrap();
        let source = directory.path().join("paper.pdf");
        write_pdf(&source);
        let bytes = fs::read(&source).unwrap();

        let outcome = validate(
            &source,
            &directory.path().join("quarantine"),
            PdfValidationContext {
                declared_content_type: Some("application/pdf; charset=binary".to_owned()),
                expected_blake3_hex: Some(blake3::hash(&bytes).to_hex().to_string()),
                expected_sha256_hex: Some(format!("{:x}", Sha256::digest(&bytes))),
                ..PdfValidationContext::default()
            },
        );

        match outcome {
            PdfValidationOutcome::Valid(report) => {
                assert_eq!(report.page_count, 1);
                assert_eq!(report.byte_length, bytes.len() as u64);
                assert_eq!(report.blake3_hex, blake3::hash(&bytes).to_hex().to_string());
                assert_eq!(report.sha256_hex, format!("{:x}", Sha256::digest(&bytes)));
                assert_eq!(
                    report.declared_content_type.as_deref(),
                    Some("application/pdf")
                );
            }
            PdfValidationOutcome::Quarantined(other) => {
                panic!("valid PDF quarantined: {:?}", other.manifest.reason)
            }
        }
        assert!(source.exists());
    }

    #[test]
    fn quarantines_html_masquerading_as_pdf_and_redacts_url_secrets() {
        let directory = tempdir().unwrap();
        let source = directory.path().join("paper.pdf");
        let mut html = b"<!DOCTYPE html><html><body>restricted</body></html>".to_vec();
        html.resize(2048, b' ');
        fs::write(&source, html).unwrap();

        let outcome = validate(
            &source,
            &directory.path().join("quarantine"),
            PdfValidationContext {
                source_url: Some(
                    "https://user:password@publisher.example/paper.pdf?token=secret#download"
                        .to_owned(),
                ),
                declared_content_type: Some("text/html; charset=utf-8".to_owned()),
                ..PdfValidationContext::default()
            },
        );
        let quarantined = match outcome {
            PdfValidationOutcome::Quarantined(item) => item,
            PdfValidationOutcome::Valid(_) => panic!("HTML must never be accepted as a PDF"),
        };

        assert!(matches!(
            quarantined.manifest.reason,
            PdfRejectionReason::DeclaredMimeNotPdf { .. }
        ));
        assert!(!source.exists());
        assert!(quarantined.quarantined_path.exists());
        assert!(quarantined.manifest_path.exists());
        assert_eq!(
            quarantined.manifest.source_origin.as_deref(),
            Some("https://publisher.example")
        );
        let manifest_text = fs::read_to_string(quarantined.manifest_path).unwrap();
        assert!(!manifest_text.contains("password"));
        assert!(!manifest_text.contains("secret"));
    }

    #[test]
    fn quarantines_corrupt_pdf_after_parser_rejection() {
        let directory = tempdir().unwrap();
        let source = directory.path().join("corrupt.pdf");
        let mut corrupt = b"%PDF-1.7\nthis is not a PDF object graph\n".to_vec();
        corrupt.resize(2048, b'X');
        corrupt.extend_from_slice(b"\n%%EOF\n");
        fs::write(&source, corrupt).unwrap();

        let outcome = validate(
            &source,
            &directory.path().join("quarantine"),
            PdfValidationContext {
                declared_content_type: Some("application/pdf".to_owned()),
                ..PdfValidationContext::default()
            },
        );
        let quarantined = match outcome {
            PdfValidationOutcome::Quarantined(item) => item,
            PdfValidationOutcome::Valid(_) => panic!("corrupt PDF unexpectedly accepted"),
        };

        assert!(matches!(
            quarantined.manifest.reason,
            PdfRejectionReason::ParserRejected { .. }
                | PdfRejectionReason::MissingCatalog
                | PdfRejectionReason::EmptyPageTree
        ));
        assert!(!source.exists());
        assert!(quarantined.manifest.observed_blake3_hex.is_some());
        assert!(quarantined.manifest.observed_sha256_hex.is_some());
    }

    #[test]
    fn quarantines_oversized_pdf_before_parser_invocation() {
        let directory = tempdir().unwrap();
        let source = directory.path().join("large.pdf");
        write_pdf(&source);

        let outcome = validate_or_quarantine_pdf(
            &source,
            directory.path().join("quarantine"),
            PdfValidationContext::default(),
            PdfValidationConfig {
                min_bytes: 1,
                max_bytes: 128,
            },
        )
        .unwrap();
        let quarantined = match outcome {
            PdfValidationOutcome::Quarantined(item) => item,
            PdfValidationOutcome::Valid(_) => panic!("oversized PDF unexpectedly accepted"),
        };

        assert!(matches!(
            quarantined.manifest.reason,
            PdfRejectionReason::TooLarge { .. }
        ));
        assert!(quarantined.manifest.observed_blake3_hex.is_none());
        assert!(!source.exists());
    }

    #[test]
    fn quarantines_truncated_pdf_without_eof_marker() {
        let directory = tempdir().unwrap();
        let source = directory.path().join("truncated.pdf");
        write_pdf(&source);
        let size = fs::metadata(&source).unwrap().len();
        OpenOptions::new()
            .write(true)
            .open(&source)
            .unwrap()
            .set_len(size - 16)
            .unwrap();

        let outcome = validate(
            &source,
            &directory.path().join("quarantine"),
            PdfValidationContext::default(),
        );
        let quarantined = match outcome {
            PdfValidationOutcome::Quarantined(item) => item,
            PdfValidationOutcome::Valid(_) => panic!("truncated PDF unexpectedly accepted"),
        };
        assert_eq!(
            quarantined.manifest.reason,
            PdfRejectionReason::MissingEofMarker
        );
    }

    #[test]
    fn quarantines_encrypted_pdf() {
        let directory = tempdir().unwrap();
        let source = directory.path().join("encrypted.pdf");
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let content_id = document.add_object(Stream::new(Dictionary::new(), b"q Q\n".repeat(512)));
        let page_id = document.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        });
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
            }),
        );
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        document.trailer.set("Root", catalog_id);
        // PDF encryption keys are derived from the file identifier; lopdf refuses to encrypt a
        // document whose trailer has no /ID entry. Provide a deterministic 16-byte hex ID pair.
        let file_id = Object::String(vec![0x42; 16], StringFormat::Hexadecimal);
        document.trailer.set("ID", vec![file_id.clone(), file_id]);
        let encryption = EncryptionState::try_from(EncryptionVersion::V1 {
            document: &document,
            owner_password: "owner-password",
            user_password: "user-password",
            permissions: Permissions::default(),
        })
        .unwrap();
        document.encrypt(&encryption).unwrap();
        document.save(&source).unwrap();

        let outcome = validate(
            &source,
            &directory.path().join("quarantine"),
            PdfValidationContext {
                declared_content_type: Some("application/pdf".to_owned()),
                ..PdfValidationContext::default()
            },
        );
        let quarantined = match outcome {
            PdfValidationOutcome::Quarantined(item) => item,
            PdfValidationOutcome::Valid(_) => panic!("encrypted PDF unexpectedly accepted"),
        };
        assert_eq!(
            quarantined.manifest.reason,
            PdfRejectionReason::EncryptedDocument
        );
    }

    #[test]
    fn quarantines_when_an_expected_checksum_does_not_match() {
        let directory = tempdir().unwrap();
        let source = directory.path().join("paper.pdf");
        write_pdf(&source);

        let outcome = validate(
            &source,
            &directory.path().join("quarantine"),
            PdfValidationContext {
                expected_sha256_hex: Some("0".repeat(64)),
                ..PdfValidationContext::default()
            },
        );
        let quarantined = match outcome {
            PdfValidationOutcome::Quarantined(item) => item,
            PdfValidationOutcome::Valid(_) => panic!("checksum mismatch unexpectedly accepted"),
        };

        assert_eq!(
            quarantined.manifest.reason,
            PdfRejectionReason::ChecksumMismatch {
                algorithm: "SHA-256".to_owned()
            }
        );
        assert!(quarantined.manifest.observed_sha256_hex.is_some());
    }
}
