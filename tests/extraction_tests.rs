//! Public acceptance tests for provenance-aware extraction (#31).

use lopdf::content::{Content, Operation};
use lopdf::dictionary;
use lopdf::{Dictionary, Document, Object, Stream};
use paperengine::extract::{
    read_record, run_extraction, ExtractionConfig, ExtractionOutcome, ExtractionStatus,
    EXTRACTOR_ID, EXTRACTOR_VERSION,
};
use paperengine::library::LibraryLayout;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use tempfile::tempdir;

/// Build a small PDF whose pages carry known text, with an `/Info` dictionary.
fn write_text_pdf(path: &Path, pages: &[&str], title: &str) -> String {
    let mut document = Document::with_version("1.5");
    let pages_id = document.new_object_id();

    let font_id = document.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
    });
    let resources_id = document.add_object(dictionary! {
        "Font" => dictionary! {
            "F1" => font_id,
        },
    });

    let mut page_ids = Vec::new();
    for text in pages {
        let mut operations = vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 12.into()]),
            Operation::new("Td", vec![72.into(), 720.into()]),
        ];
        for line in text.lines() {
            operations.push(Operation::new("Tj", vec![Object::string_literal(line)]));
            operations.push(Operation::new("Td", vec![0.into(), (-14).into()]));
        }
        operations.push(Operation::new("ET", vec![]));
        let content = Content { operations };
        let encoded = content.encode().unwrap();
        let content_id = document.add_object(Stream::new(Dictionary::new(), encoded));
        let page_id = document.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            "Resources" => resources_id,
        });
        page_ids.push(page_id);
    }

    document.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => page_ids.into_iter().map(Object::Reference).collect::<Vec<_>>(),
            "Count" => pages.len() as u32,
        }),
    );
    let catalog_id = document.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    document.trailer.set("Root", catalog_id);
    let info_id = document.add_object(dictionary! {
        "Title" => Object::string_literal(title),
        "Author" => Object::string_literal("PaperEngine Acceptance Test"),
        "Producer" => Object::string_literal("lopdf fixture builder"),
    });
    document.trailer.set("Info", info_id);
    document.save(path).unwrap();

    let bytes = fs::read(path).unwrap();
    format!("{:x}", Sha256::digest(&bytes))
}

/// Place a PDF into content-addressed object storage the way `acquire` finalizes it.
fn store_object(library: &LibraryLayout, source: &Path, sha256_hex: &str) {
    let object_path = library.object_path(sha256_hex).unwrap();
    fs::create_dir_all(object_path.parent().unwrap()).unwrap();
    fs::copy(source, &object_path).unwrap();
}

fn config() -> ExtractionConfig {
    ExtractionConfig {
        max_text_bytes: 16 * 1024 * 1024,
        max_objects: 256,
    }
}

#[test]
fn extracts_text_with_provenance_and_deterministic_content() {
    let directory = tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    library.create().unwrap();

    let pdf_path = directory.path().join("two-page.pdf");
    let sha = write_text_pdf(
        &pdf_path,
        &[
            "Hello PaperEngine extraction",
            "second page text about attention",
        ],
        "Deterministic Extraction Fixture",
    );
    store_object(&library, &pdf_path, &sha);

    let mut entries = Vec::new();
    let summary = run_extraction(&library, &config(), false, 1_700_000_000, |entry| {
        entries.push(entry.clone());
        Ok(())
    })
    .unwrap();

    assert_eq!(summary.objects_examined, 1);
    assert_eq!(summary.extracted, 1);
    assert_eq!(summary.failed, 0);
    assert_eq!(entries.len(), 1);

    let ExtractionOutcome::Extracted {
        char_count,
        pages_with_text,
        pages_total,
        truncated,
        text_path,
        text_sha256_hex,
        ..
    } = &entries[0].outcome
    else {
        panic!("expected extraction, got {:?}", entries[0].outcome);
    };
    assert_eq!(*pages_total, 2);
    assert_eq!(*pages_with_text, 2);
    assert!(*char_count > 30);
    assert!(!*truncated);

    // The text artifact exists, is non-empty, and matches its recorded hash.
    let text_absolute = library.root().join(text_path);
    let text_bytes = fs::read(&text_absolute).unwrap();
    assert!(!text_bytes.is_empty());
    assert_eq!(
        &format!("{:x}", Sha256::digest(&text_bytes)),
        text_sha256_hex
    );
    let text = String::from_utf8_lossy(&text_bytes);
    assert!(text.contains("Hello PaperEngine extraction"));
    assert!(text.contains("second page text about attention"));

    // The provenance record identifies the extractor and the source object.
    let record = read_record(&library, &sha).unwrap().unwrap();
    assert_eq!(record.schema_version, 1);
    assert_eq!(record.extractor.id, EXTRACTOR_ID);
    assert_eq!(record.extractor.version, EXTRACTOR_VERSION);
    assert_eq!(record.object.sha256_hex, sha);
    assert_eq!(record.object.page_count, 2);
    assert_eq!(
        record.document_info.title.as_deref(),
        Some("Deterministic Extraction Fixture")
    );
    assert_eq!(
        record.document_info.author.as_deref(),
        Some("PaperEngine Acceptance Test")
    );
    assert!(matches!(
        record.status,
        ExtractionStatus::Extracted { pages_total: 2, .. }
    ));
    assert_eq!(record.extraction_rfc3339, "2023-11-14T22:13:20Z");
}

#[test]
fn extraction_is_deterministic_and_force_rerun_is_byte_identical() {
    let directory = tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    library.create().unwrap();

    let pdf_path = directory.path().join("page.pdf");
    let sha = write_text_pdf(&pdf_path, &["repeatable text"], "Reproducible Fixture");
    store_object(&library, &pdf_path, &sha);

    let first = run_extraction(&library, &config(), false, 1, |_| Ok(())).unwrap();
    assert_eq!(first.extracted, 1);
    let first_record = read_record(&library, &sha).unwrap().unwrap();
    let first_text = fs::read(library.root().join(&first_record.text.path)).unwrap();

    // Without --force the completed record is skipped.
    let second = run_extraction(&library, &config(), false, 2, |_| Ok(())).unwrap();
    assert_eq!(second.extracted, 0);
    assert_eq!(second.skipped, 1);

    // With --force the extraction re-runs and produces byte-identical text.
    let third = run_extraction(&library, &config(), true, 3, |_| Ok(())).unwrap();
    assert_eq!(third.extracted, 1);
    let third_record = read_record(&library, &sha).unwrap().unwrap();
    let third_text = fs::read(library.root().join(&third_record.text.path)).unwrap();
    assert_eq!(first_text, third_text);
    assert_eq!(first_record.text.sha256_hex, third_record.text.sha256_hex);
    // The timestamp updates; the extractor identity does not.
    assert_eq!(third_record.extraction_unix_seconds, 3);
    assert_eq!(
        third_record.extractor.version,
        first_record.extractor.version
    );
}

#[test]
fn text_budget_truncates_deterministically_and_is_recorded() {
    let directory = tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    library.create().unwrap();

    let pdf_path = directory.path().join("page.pdf");
    let sha = write_text_pdf(&pdf_path, &["0123456789abcdefghijklmnopqrstuvwxyz"], "Tiny");
    store_object(&library, &pdf_path, &sha);

    let tight = ExtractionConfig {
        max_text_bytes: 10,
        max_objects: 256,
    };
    let mut entries = Vec::new();
    let summary = run_extraction(&library, &tight, false, 1, |entry| {
        entries.push(entry.clone());
        Ok(())
    })
    .unwrap();
    assert_eq!(summary.extracted, 1);

    let ExtractionOutcome::Extracted { truncated, .. } = &entries[0].outcome else {
        panic!("expected extraction, got {:?}", entries[0].outcome);
    };
    assert!(*truncated, "tight budget must record truncation");

    let record = read_record(&library, &sha).unwrap().unwrap();
    assert!(record.text.byte_length <= 10);
    let ExtractionStatus::Extracted { truncated, .. } = record.status else {
        panic!("expected Extracted status");
    };
    assert!(truncated);
    let text = fs::read(library.root().join(&record.text.path)).unwrap();
    assert_eq!(text.len() as u64, record.text.byte_length);
}

#[test]
fn corrupt_object_fails_without_leaving_a_record() {
    let directory = tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    library.create().unwrap();

    // A garbage payload planted under a valid content-addressed name.
    let garbage = b"%PDF-1.5 not really a pdf at all".repeat(16);
    let sha = format!("{:x}", Sha256::digest(&garbage));
    let object_path = library.object_path(&sha).unwrap();
    fs::create_dir_all(object_path.parent().unwrap()).unwrap();
    fs::write(&object_path, &garbage).unwrap();

    let mut entries = Vec::new();
    let summary = run_extraction(&library, &config(), false, 1, |entry| {
        entries.push(entry.clone());
        Ok(())
    })
    .unwrap();
    assert_eq!(summary.objects_examined, 1);
    assert_eq!(summary.failed, 1);
    assert!(entries.len() == 1);
    match &entries[0].outcome {
        ExtractionOutcome::Failed { stage, .. } => assert_eq!(stage, "parse"),
        other => panic!("expected parse failure, got {other:?}"),
    }

    // No record and no text artifact were finalized for the failure.
    assert!(read_record(&library, &sha).unwrap().is_none());
    assert!(
        library
            .extracted_dir()
            .join(&sha[..2])
            .join(format!("{sha}.txt"))
            .read_dir()
            .is_err()
            || !library
                .extracted_dir()
                .join(&sha[..2])
                .join(format!("{sha}.txt"))
                .exists()
    );

    // A re-run retries the failed object instead of skipping it.
    let retried = run_extraction(&library, &config(), false, 2, |_| Ok(())).unwrap();
    assert_eq!(retried.objects_examined, 1);
    assert_eq!(retried.failed, 1);
    assert_eq!(retried.skipped, 0);
}

#[test]
fn misplaced_object_content_is_rejected_by_address_check() {
    let directory = tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    library.create().unwrap();

    let pdf_path = directory.path().join("page.pdf");
    let real_sha = write_text_pdf(&pdf_path, &["addressed text"], "Addressed");
    // Store the object under a DIFFERENT digest than its content: the name no longer
    // addresses the bytes.
    let wrong_sha = format!("{:x}", Sha256::digest(b"something else entirely"));
    store_object(&library, &pdf_path, &wrong_sha);
    let _ = real_sha;

    let mut entries = Vec::new();
    let summary = run_extraction(&library, &config(), false, 1, |entry| {
        entries.push(entry.clone());
        Ok(())
    })
    .unwrap();
    assert_eq!(summary.failed, 1);
    match &entries[0].outcome {
        ExtractionOutcome::Failed { stage, detail } => {
            assert_eq!(stage, "parse");
            assert!(detail.contains("content-addressed"), "detail: {detail}");
        }
        other => panic!("expected address mismatch, got {other:?}"),
    }
    assert!(read_record(&library, &wrong_sha).unwrap().is_none());
}

#[test]
fn object_budget_skips_remaining_objects() {
    let directory = tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    library.create().unwrap();

    for index in 0..3 {
        let pdf_path = directory.path().join("page{index}.pdf");
        let sha = write_text_pdf(&pdf_path, &[&format!("object {index} text")], "Budget");
        store_object(&library, &pdf_path, &sha);
    }

    let limited = ExtractionConfig {
        max_text_bytes: 16 * 1024 * 1024,
        max_objects: 1,
    };
    let mut entries = Vec::new();
    let summary = run_extraction(&library, &limited, false, 1, |entry| {
        entries.push(entry.clone());
        Ok(())
    })
    .unwrap();
    assert_eq!(summary.objects_examined, 1);
    assert_eq!(summary.extracted, 1);
    assert_eq!(summary.skipped, 2);
    assert_eq!(entries.len(), 3);
    assert!(entries
        .iter()
        .any(|entry| matches!(&entry.outcome, ExtractionOutcome::Skipped { reason } if reason.contains("budget exhausted"))));
}

#[test]
fn empty_text_object_reports_honest_empty_status() {
    let directory = tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    library.create().unwrap();

    // A structurally valid PDF with no text operators at all.
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

    let pdf_path = directory.path().join("notext.pdf");
    document.save(&pdf_path).unwrap();
    let bytes = fs::read(&pdf_path).unwrap();
    let sha = format!("{:x}", Sha256::digest(&bytes));
    store_object(&library, &pdf_path, &sha);

    let mut entries = Vec::new();
    let summary = run_extraction(&library, &config(), false, 1, |entry| {
        entries.push(entry.clone());
        Ok(())
    })
    .unwrap();
    assert_eq!(summary.extracted, 0);
    assert_eq!(summary.empty, 1);
    assert_eq!(summary.failed, 0);

    let ExtractionOutcome::Empty { pages_total, .. } = &entries[0].outcome else {
        panic!("expected empty outcome, got {:?}", entries[0].outcome);
    };
    assert_eq!(*pages_total, 1);

    let record = read_record(&library, &sha).unwrap().unwrap();
    assert!(matches!(
        record.status,
        ExtractionStatus::Empty { pages_total: 1 }
    ));
    // The empty text artifact still exists with its hash for integrity parity.
    let text_path = library.root().join(&record.text.path);
    assert!(text_path.exists());
    assert_eq!(fs::read(&text_path).unwrap().len(), 0);
}
