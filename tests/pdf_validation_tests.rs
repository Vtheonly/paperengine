//! Public acceptance tests for PDF validation and quarantine.

use lopdf::dictionary;
use lopdf::{Dictionary, Document, Object, Stream};
use paperengine::fetcher::pdf_validation::{
    validate_or_quarantine_pdf, PdfRejectionReason, PdfValidationConfig, PdfValidationContext,
    PdfValidationOutcome,
};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::path::Path;
use tempfile::tempdir;

fn write_valid_pdf(path: &Path) {
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

fn config() -> PdfValidationConfig {
    PdfValidationConfig {
        min_bytes: 128,
        max_bytes: 4 * 1024 * 1024,
    }
}

fn context() -> PdfValidationContext {
    PdfValidationContext {
        declared_content_type: Some("application/pdf".to_owned()),
        ..PdfValidationContext::default()
    }
}

#[test]
fn valid_fixture_is_accepted_with_page_count_and_hashes() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("valid.pdf");
    write_valid_pdf(&source);
    let bytes = fs::read(&source).unwrap();

    let result = validate_or_quarantine_pdf(
        &source,
        directory.path().join("quarantine"),
        PdfValidationContext {
            expected_blake3_hex: Some(blake3::hash(&bytes).to_hex().to_string()),
            expected_sha256_hex: Some(format!("{:x}", Sha256::digest(&bytes))),
            ..context()
        },
        config(),
    )
    .unwrap();

    match result {
        PdfValidationOutcome::Valid(report) => {
            assert_eq!(report.page_count, 1);
            assert_eq!(report.blake3_hex, blake3::hash(&bytes).to_hex().to_string());
            assert_eq!(report.sha256_hex, format!("{:x}", Sha256::digest(&bytes)));
        }
        PdfValidationOutcome::Quarantined(item) => {
            panic!("valid PDF was quarantined: {:?}", item.manifest.reason)
        }
    }
    assert!(source.exists());
}

#[test]
fn html_masquerading_as_pdf_is_quarantined_with_reason() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("html.pdf");
    let mut bytes = b"<!DOCTYPE html><html><body>forbidden</body></html>".to_vec();
    bytes.resize(1024, b' ');
    fs::write(&source, bytes).unwrap();

    let result = validate_or_quarantine_pdf(
        &source,
        directory.path().join("quarantine"),
        PdfValidationContext {
            source_url: Some("https://publisher.example/paper?signature=secret".to_owned()),
            declared_content_type: Some("text/html; charset=utf-8".to_owned()),
            ..PdfValidationContext::default()
        },
        config(),
    )
    .unwrap();

    let quarantined = match result {
        PdfValidationOutcome::Quarantined(item) => item,
        PdfValidationOutcome::Valid(_) => panic!("HTML response must be quarantined"),
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
    assert!(!fs::read_to_string(quarantined.manifest_path)
        .unwrap()
        .contains("secret"));
}

#[test]
fn corrupted_pdf_is_quarantined_after_parser_rejection() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("corrupt.pdf");
    let mut bytes = b"%PDF-1.7\nnot a PDF object graph\n".to_vec();
    bytes.resize(2048, b'X');
    bytes.extend_from_slice(b"\n%%EOF\n");
    fs::write(&source, bytes).unwrap();

    let result = validate_or_quarantine_pdf(
        &source,
        directory.path().join("quarantine"),
        context(),
        config(),
    )
    .unwrap();
    let quarantined = match result {
        PdfValidationOutcome::Quarantined(item) => item,
        PdfValidationOutcome::Valid(_) => panic!("corrupt PDF must be quarantined"),
    };
    assert!(matches!(
        quarantined.manifest.reason,
        PdfRejectionReason::ParserRejected { .. }
            | PdfRejectionReason::ParserPanicked
            | PdfRejectionReason::MissingCatalog
            | PdfRejectionReason::EmptyPageTree
    ));
    assert!(quarantined.manifest.observed_blake3_hex.is_some());
    assert!(quarantined.manifest.observed_sha256_hex.is_some());
}

#[test]
fn oversized_fixture_is_quarantined_before_structural_parse() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("large.pdf");
    write_valid_pdf(&source);

    let result = validate_or_quarantine_pdf(
        &source,
        directory.path().join("quarantine"),
        context(),
        PdfValidationConfig {
            min_bytes: 1,
            max_bytes: 128,
        },
    )
    .unwrap();
    let quarantined = match result {
        PdfValidationOutcome::Quarantined(item) => item,
        PdfValidationOutcome::Valid(_) => panic!("oversized PDF must be quarantined"),
    };
    assert!(matches!(
        quarantined.manifest.reason,
        PdfRejectionReason::TooLarge { .. }
    ));
    assert!(quarantined.manifest.observed_blake3_hex.is_none());
}

#[test]
fn partial_fixture_without_eof_is_quarantined() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("partial.pdf");
    write_valid_pdf(&source);
    let size = fs::metadata(&source).unwrap().len();
    OpenOptions::new()
        .write(true)
        .open(&source)
        .unwrap()
        .set_len(size - 16)
        .unwrap();

    let result = validate_or_quarantine_pdf(
        &source,
        directory.path().join("quarantine"),
        context(),
        config(),
    )
    .unwrap();
    let quarantined = match result {
        PdfValidationOutcome::Quarantined(item) => item,
        PdfValidationOutcome::Valid(_) => panic!("truncated PDF must be quarantined"),
    };
    assert_eq!(
        quarantined.manifest.reason,
        PdfRejectionReason::MissingEofMarker
    );
}
