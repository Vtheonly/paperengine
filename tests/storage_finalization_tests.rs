//! Crash-boundary and dedup tests for content-addressed storage finalization (#29).
//!
//! Finalization crosses three boundaries: payload scratch (fsynced by the downloader),
//! the object rename into `objects/`, and provenance-manifest publication. These tests
//! simulate a crash at each boundary by constructing the exact on-disk state a crash
//! would leave, then verify the two acceptance invariants:
//!
//! 1. No crash state is ever reported as a falsely complete acquisition.
//! 2. Duplicates reuse content safely, and recovery converges without data loss.
//!
//! All tests run offline against the local fixture server.

mod support;

use paperengine::library::{
    run_acquisition, AcquisitionOutcome, AcquisitionReportEntry, LibraryError, LibraryLayout,
    ProvenanceManifest, LIBRARY_SCHEMA_VERSION,
};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use support::{
    acquisition_config, downloader_for, record_with_pdf, small_pdf_bytes, FixtureServer, Route,
};

fn pdf_route(path: &'static str, body: Vec<u8>) -> Route {
    Route {
        path,
        status: "200 OK",
        content_type: "application/pdf",
        headers: vec![("ETag", "\"finalization-v1\"".to_owned())],
        body,
    }
}

fn push_entry(
    entries: &mut Vec<String>,
) -> impl FnMut(&AcquisitionReportEntry) -> std::io::Result<()> + '_ {
    move |entry: &AcquisitionReportEntry| {
        entries.push(serde_json::to_string(entry).unwrap());
        Ok(())
    }
}

fn parse_entry(line: &str) -> AcquisitionReportEntry {
    serde_json::from_str(line).unwrap()
}

/// Boundary 1 — crash during the transfer itself: only a scratch file exists under
/// `tmp/`, no object, no manifest. The next run must clear the stale scratch, transfer
/// fresh bytes, and finalize exactly one complete object.
#[tokio::test]
async fn crash_during_transfer_leaves_only_scratch_and_next_run_recovers() {
    let pdf = small_pdf_bytes();
    let server = FixtureServer::start(vec![pdf_route("/paper.pdf", pdf.clone())]);
    let record = record_with_pdf(
        "arxiv",
        "2401.00001",
        format!("{}/paper.pdf", server.base_url),
    );

    let directory = tempfile::tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    library.create().unwrap();
    let config = acquisition_config();
    let downloader = downloader_for(&config);

    // Simulate the leftover of a crashed earlier transfer: a bogus scratch payload
    // that must never be finalized as-is.
    let scratch = library.tmp_dir().join("arxiv-2401.00001.pdf");
    fs::write(&scratch, b"%PDF-1.4 torn garbage from an interrupted run").unwrap();

    let mut entries = Vec::new();
    let summary = run_acquisition(
        std::iter::once(&record),
        &library,
        &config,
        &downloader,
        1_700_000_000,
        push_entry(&mut entries),
    )
    .await
    .unwrap();

    assert_eq!(summary.acquired, 1);
    assert_eq!(summary.failed, 0);
    let entry = parse_entry(&entries[0]);
    let AcquisitionOutcome::Acquired {
        byte_length,
        sha256_hex,
        object_path,
        ..
    } = entry.outcome
    else {
        panic!("expected acquisition, got {:?}", entry.outcome)
    };
    assert_eq!(byte_length, pdf.len() as u64);
    assert_eq!(sha256_hex, format!("{:x}", Sha256::digest(&pdf)));

    // Exactly one complete object; the stale scratch was replaced, not appended to.
    let objects = library.list_objects().unwrap();
    assert_eq!(objects.len(), 1);
    assert_eq!(fs::read(&object_path).unwrap(), pdf);
    // tmp/ is empty again: no scratch survived the run.
    assert!(library
        .tmp_dir()
        .read_dir()
        .map(|mut d| d.next().is_none())
        .unwrap());
    // The library attributes the object: no orphans.
    assert!(library.unattributed_objects().unwrap().is_empty());
}

/// Boundary 2 — crash between the object rename and manifest publication: the object
/// exists but its provenance does not. The state must be *visible as incomplete*
/// (`unattributed_objects`), and re-acquiring the record must converge: dedup against
/// the existing object, write the missing manifest, leave bytes untouched.
#[tokio::test]
async fn crash_between_object_rename_and_manifest_is_visible_and_recoverable() {
    let pdf = small_pdf_bytes();
    // Initial run + recovery run each transfer the payload once.
    let server = FixtureServer::start(vec![
        pdf_route("/paper.pdf", pdf.clone()),
        pdf_route("/paper.pdf", pdf.clone()),
    ]);
    let record = record_with_pdf(
        "arxiv",
        "2401.00002",
        format!("{}/paper.pdf", server.base_url),
    );

    let directory = tempfile::tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    let config = acquisition_config();
    let downloader = downloader_for(&config);

    // Complete run: object + manifest.
    let mut entries = Vec::new();
    let summary = run_acquisition(
        std::iter::once(&record),
        &library,
        &config,
        &downloader,
        1_700_000_000,
        push_entry(&mut entries),
    )
    .await
    .unwrap();
    assert_eq!(summary.acquired, 1);
    let entry = parse_entry(&entries[0]);
    let AcquisitionOutcome::Acquired {
        object_path,
        sha256_hex,
        ..
    } = entry.outcome
    else {
        panic!("expected acquisition, got {:?}", entry.outcome)
    };
    let object_path = Path::new(&object_path).to_path_buf();
    assert!(library.unattributed_objects().unwrap().is_empty());

    // Simulate the crash boundary: the manifest never made it to disk.
    let manifests_before = library.list_manifests().unwrap();
    assert_eq!(manifests_before.len(), 1);
    let manifest_file = library
        .manifests_dir()
        .read_dir()
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::remove_file(&manifest_file).unwrap();

    // The object is now explicitly reported as unattributed — an interrupted run is
    // never presented as a complete acquisition.
    let orphans = library.unattributed_objects().unwrap();
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].0, object_path);

    // Recovery: re-acquire the same record.
    let mut recovery = Vec::new();
    let summary = run_acquisition(
        std::iter::once(&record),
        &library,
        &config,
        &downloader,
        1_700_000_500,
        push_entry(&mut recovery),
    )
    .await
    .unwrap();
    assert_eq!(summary.acquired, 1);
    assert_eq!(summary.deduplicated, 1);

    // The manifest was written, the object bytes are unchanged, and no orphans remain.
    let manifests = library.list_manifests().unwrap();
    assert_eq!(manifests.len(), 1);
    assert_eq!(manifests[0].content.sha256_hex, sha256_hex);
    assert_eq!(fs::read(&object_path).unwrap(), pdf);
    let objects = library.list_objects().unwrap();
    assert_eq!(objects.len(), 1);
    assert!(library.unattributed_objects().unwrap().is_empty());
}

/// Boundary 3 — manifest publication itself: the provenance JSON is written to a
/// `.pending` sibling and renamed into place, so a torn manifest cannot exist at the
/// final path. A leftover `.pending` file is ignored by readers and cleared by the next
/// publication of the same manifest.
#[tokio::test]
async fn manifest_publication_is_atomic_and_pending_leftovers_are_ignored() {
    let directory = tempfile::tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    library.create().unwrap();

    let manifest = ProvenanceManifest {
        schema_version: LIBRARY_SCHEMA_VERSION,
        work: paperengine::library::ProvenanceWork {
            source_id: "arxiv".into(),
            source_record_id: "2401.00003".into(),
            identifiers: Vec::new(),
            title: "Atomic publication".into(),
            authors: Vec::new(),
            publication_year: Some(2024),
            venue: None,
            license: None,
        },
        acquisition: paperengine::library::ProvenanceAcquisition {
            pdf_url: "https://example.org/p.pdf".into(),
            retrieval_unix_seconds: 1_700_000_000,
            retrieval_rfc3339: "2023-11-14T22:13:20Z".into(),
        },
        content: paperengine::library::ProvenanceContent {
            byte_length: 42,
            sha256_hex: format!("{:x}", Sha256::digest(b"atomic")),
            blake3_hex: "0".repeat(64),
            page_count: 1,
            pdf_version: "1.5".into(),
        },
    };

    // Simulate an interrupted earlier publication: a stale pending file.
    let manifest_path = library
        .manifest_path("arxiv", "2401.00003", &manifest.content.sha256_hex)
        .unwrap();
    let pending = manifest_path.with_file_name(format!(
        "{}.pending",
        manifest_path.file_name().unwrap().to_string_lossy()
    ));
    fs::write(&pending, b"{ \"schema_version\": 1, \"torn\": ").unwrap();
    // Readers ignore the pending sibling entirely.
    assert!(library.list_manifests().unwrap().is_empty());

    // Publishing succeeds, replaces the stale pending file, and lands atomically.
    let bytes = serde_json::to_vec_pretty(&manifest).unwrap();
    paperengine::library::publish_bytes_atomically(&manifest_path, &bytes).unwrap();
    assert!(manifest_path.exists());
    assert!(
        !pending.exists(),
        "pending file must not survive publication"
    );
    let parsed = library.list_manifests().unwrap();
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0], manifest);

    // The manifests directory contains exactly one .json file — no remnants.
    let entries: Vec<_> = library
        .manifests_dir()
        .read_dir()
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries.len(), 1);
    assert!(entries[0].ends_with(".json"));
}

/// Duplicates across distinct works: two different records carrying byte-identical
/// payloads share one content-addressed object, each keeps its own provenance, and the
/// second acquisition is reported as a deduplication, not a second object.
#[tokio::test]
async fn duplicate_content_across_distinct_works_shares_one_object() {
    let pdf = small_pdf_bytes();
    let server = FixtureServer::start(vec![
        pdf_route("/a.pdf", pdf.clone()),
        pdf_route("/b.pdf", pdf.clone()),
    ]);
    let first = record_with_pdf("arxiv", "2401.00004", format!("{}/a.pdf", server.base_url));
    let second = record_with_pdf("openalex", "W-dup", format!("{}/b.pdf", server.base_url));

    let directory = tempfile::tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    let config = acquisition_config();
    let downloader = downloader_for(&config);

    let records = [first, second];
    let mut entries = Vec::new();
    let summary = run_acquisition(
        records.iter(),
        &library,
        &config,
        &downloader,
        1_700_000_000,
        push_entry(&mut entries),
    )
    .await
    .unwrap();

    assert_eq!(summary.acquired, 2);
    assert_eq!(summary.deduplicated, 1);
    assert_eq!(summary.failed, 0);

    let first_entry = parse_entry(&entries[0]);
    let AcquisitionOutcome::Acquired {
        deduplicated: first_dedup,
        object_path: first_object,
        ..
    } = first_entry.outcome
    else {
        panic!("expected acquisition, got {:?}", first_entry.outcome)
    };
    let second_entry = parse_entry(&entries[1]);
    let AcquisitionOutcome::Acquired {
        deduplicated: second_dedup,
        object_path: second_object,
        ..
    } = second_entry.outcome
    else {
        panic!("expected acquisition, got {:?}", second_entry.outcome)
    };
    assert!(!first_dedup);
    assert!(second_dedup);
    assert_eq!(first_object, second_object, "one shared object");

    // One object, two provenance manifests, no orphans.
    assert_eq!(library.list_objects().unwrap().len(), 1);
    let manifests = library.list_manifests().unwrap();
    assert_eq!(manifests.len(), 2);
    assert!(library.unattributed_objects().unwrap().is_empty());
    let source_ids: Vec<_> = manifests
        .iter()
        .map(|manifest| manifest.work.source_id.as_str())
        .collect();
    assert!(source_ids.contains(&"arxiv"));
    assert!(source_ids.contains(&"openalex"));
}

/// The finalized object is a real, reopenable PDF: the structural parser loads it from
/// its final storage path and its bytes hash to its content address.
#[tokio::test]
async fn finalized_object_reopens_with_real_parser_and_matches_its_address() {
    let pdf = small_pdf_bytes();
    let server = FixtureServer::start(vec![pdf_route("/paper.pdf", pdf.clone())]);
    let record = record_with_pdf(
        "arxiv",
        "2401.00005",
        format!("{}/paper.pdf", server.base_url),
    );

    let directory = tempfile::tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    let config = acquisition_config();
    let downloader = downloader_for(&config);

    let mut entries = Vec::new();
    run_acquisition(
        std::iter::once(&record),
        &library,
        &config,
        &downloader,
        1_700_000_000,
        push_entry(&mut entries),
    )
    .await
    .unwrap();

    let entry = parse_entry(&entries[0]);
    let AcquisitionOutcome::Acquired {
        object_path,
        sha256_hex,
        byte_length,
        ..
    } = entry.outcome
    else {
        panic!("expected acquisition, got {:?}", entry.outcome)
    };

    let stored = fs::read(&object_path).unwrap();
    assert_eq!(stored.len() as u64, byte_length);
    assert_eq!(format!("{:x}", Sha256::digest(&stored)), sha256_hex);

    // The production validator accepts the finalized object from its final path.
    let outcome = paperengine::fetcher::pdf_validation::validate_or_quarantine_pdf(
        Path::new(&object_path),
        library.quarantine_dir(),
        paperengine::fetcher::pdf_validation::PdfValidationContext {
            source_url: None,
            declared_content_type: None,
            expected_blake3_hex: None,
            expected_sha256_hex: Some(sha256_hex.clone()),
        },
        paperengine::fetcher::pdf_validation::PdfValidationConfig::default(),
    )
    .unwrap();
    assert!(matches!(
        outcome,
        paperengine::fetcher::pdf_validation::PdfValidationOutcome::Valid(_)
    ));
    // And the raw parser reopens it independently.
    lopdf::Document::load(&object_path).unwrap();
}

/// A torn manifest at the final path can now only come from external corruption (the
/// production path publishes atomically). The library refuses to silently skip it —
/// the error names the file so the operator can inspect and repair.
#[test]
fn externally_corrupted_manifest_is_surfaced_with_its_path() {
    let directory = tempfile::tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    library.create().unwrap();
    fs::write(
        library.manifests_dir().join("torn.json"),
        b"{ \"schema_version\": 1, \"trunc",
    )
    .unwrap();

    let error = library.list_manifests().unwrap_err();
    match &error {
        LibraryError::InvalidLayout(detail) => {
            assert!(
                detail.contains("torn.json"),
                "error must name the offending file: {detail}"
            );
        }
        other => panic!("expected InvalidLayout, got {other:?}"),
    }
}
