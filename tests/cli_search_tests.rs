//! Command integration tests for the index and search CLI (#33): exercise the real
//! `paperengine` binary end to end against a fixture library.

use lopdf::content::{Content, Operation};
use lopdf::dictionary;
use lopdf::{Dictionary, Document, Object, Stream};
use paperengine::library::{
    LibraryLayout, ProvenanceAcquisition, ProvenanceContent, ProvenanceManifest, ProvenanceWork,
};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::tempdir;

const BINARY: &str = env!("CARGO_BIN_EXE_paperengine");

fn run_cli(args: &[&str], working_dir: &Path) -> Output {
    Command::new(BINARY)
        .args(args)
        .current_dir(working_dir)
        .env_remove("PAPERENGINE_CONTACT_EMAIL")
        .output()
        .expect("failed to launch the paperengine binary")
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A minimal one-page PDF carrying the given two text lines.
fn write_text_pdf(path: &Path, lines: &[&str]) -> (String, u64) {
    let mut document = Document::with_version("1.5");
    let pages_id = document.new_object_id();
    let font_id = document.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
    });
    let resources_id = document.add_object(dictionary! {
        "Font" => dictionary! { "F1" => font_id },
    });
    let mut operations = vec![
        Operation::new("BT", vec![]),
        Operation::new("Tf", vec!["F1".into(), 12.into()]),
        Operation::new("Td", vec![72.into(), 720.into()]),
    ];
    for line in lines {
        operations.push(Operation::new("Tj", vec![Object::string_literal(*line)]));
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
    let bytes = fs::read(path).unwrap();
    (format!("{:x}", Sha256::digest(&bytes)), bytes.len() as u64)
}

struct WorkSpec {
    source_id: &'static str,
    record_id: &'static str,
    title: &'static str,
    authors: Vec<&'static str>,
    year: Option<u16>,
    doi: Option<&'static str>,
    body_lines: Vec<&'static str>,
}

/// Store one work as object + provenance manifest (mirrors `acquire` finalization).
fn store_work(library: &LibraryLayout, scratch: &Path, spec: &WorkSpec) -> String {
    let pdf_path = scratch.join(format!("{}.pdf", spec.record_id.replace('/', "_")));
    let (sha, bytes) = write_text_pdf(&pdf_path, &spec.body_lines);
    let object_path = library.object_path(&sha).unwrap();
    fs::create_dir_all(object_path.parent().unwrap()).unwrap();
    fs::copy(&pdf_path, &object_path).unwrap();

    let identifiers = spec
        .doi
        .map(|doi| {
            vec![paperengine::sources::work_record::WorkIdentifier {
                kind: paperengine::sources::work_record::IdentifierKind::Doi,
                value: doi.to_owned(),
            }]
        })
        .unwrap_or_default();

    let manifest = ProvenanceManifest {
        schema_version: 1,
        work: ProvenanceWork {
            source_id: spec.source_id.to_owned(),
            source_record_id: spec.record_id.to_owned(),
            identifiers,
            title: spec.title.to_owned(),
            authors: spec
                .authors
                .iter()
                .map(|author| (*author).to_owned())
                .collect(),
            publication_year: spec.year,
            venue: None,
            license: None,
        },
        acquisition: ProvenanceAcquisition {
            pdf_url: "https://example.org/fixture.pdf".to_owned(),
            retrieval_unix_seconds: 1,
            retrieval_rfc3339: "2026-01-01T00:00:00Z".to_owned(),
        },
        content: ProvenanceContent {
            byte_length: bytes,
            sha256_hex: sha.clone(),
            blake3_hex: "00".repeat(32),
            page_count: 1,
            pdf_version: "1.5".to_owned(),
        },
    };
    let manifest_path = library
        .manifest_path(spec.source_id, spec.record_id, &sha)
        .unwrap();
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    sha
}

fn fixture_library(root: &Path) -> (LibraryLayout, PathBuf) {
    let library = LibraryLayout::new(root.join("library"));
    library.create().unwrap();
    let scratch = root.join("fixtures");
    fs::create_dir_all(&scratch).unwrap();

    store_work(
        &library,
        &scratch,
        &WorkSpec {
            source_id: "arxiv",
            record_id: "2501.00001",
            title: "Gradient Methods for Sparse Recovery",
            authors: vec!["Ada Mathematician"],
            year: Some(2025),
            doi: None,
            body_lines: vec!["sparse recovery via gradients", "unique token zephyr"],
        },
    );
    store_work(
        &library,
        &scratch,
        &WorkSpec {
            source_id: "doaj",
            record_id: "10.1000/clin",
            title: "Clinical Trial Reporting Standards",
            authors: vec!["Ben Clinician", "Cy Statistician"],
            year: Some(2023),
            doi: Some("10.1000/clin"),
            body_lines: vec!["clinical trial reporting overview"],
        },
    );
    (library, scratch)
}

#[test]
fn extract_index_search_commands_work_end_to_end() {
    let directory = tempdir().unwrap();
    let (library, _scratch) = fixture_library(directory.path());
    let library_dir = library.root().to_string_lossy().into_owned();

    // extract: both objects processed, JSONL report written.
    let extract = run_cli(&["extract", "--library", &library_dir], directory.path());
    assert!(
        extract.status.success(),
        "extract failed: {}",
        stderr_of(&extract)
    );
    let extract_out = stdout_of(&extract);
    assert_eq!(extract_out.matches("\"kind\":\"extracted\"").count(), 2);
    assert!(
        library.extraction_dir().join("arxiv").exists() || {
            // records are keyed by object sha, not source; assert via the report instead
            let mut count = 0;
            for entry in fs::read_dir(library.extraction_dir()).unwrap() {
                if entry.unwrap().path().extension().and_then(|e| e.to_str()) == Some("json") {
                    count += 1;
                }
            }
            count == 2
        }
    );

    // index: both manifests added.
    let index = run_cli(&["index", "--library", &library_dir], directory.path());
    assert!(
        index.status.success(),
        "index failed: {}",
        stderr_of(&index)
    );
    assert!(stdout_of(&index).contains("\"kind\":\"added\""));

    // search: title query with provenance lines.
    let search = run_cli(
        &[
            "search",
            "--library",
            &library_dir,
            "--query",
            "sparse recovery",
        ],
        directory.path(),
    );
    assert!(
        search.status.success(),
        "search failed: {}",
        stderr_of(&search)
    );
    let search_out = stdout_of(&search);
    assert!(
        search_out.contains("Gradient Methods for Sparse Recovery"),
        "output: {search_out}"
    );
    assert!(search_out.contains("[arxiv]"), "output: {search_out}");
    assert!(search_out.contains("sha256="), "output: {search_out}");
    assert!(search_out.contains("text=yes"), "output: {search_out}");
    assert!(
        !search_out.contains("Clinical Trial"),
        "output: {search_out}"
    );

    // body-text-only query (word that exists only in extracted text).
    let body_search = run_cli(
        &["search", "--library", &library_dir, "--query", "zephyr"],
        directory.path(),
    );
    assert!(body_search.status.success());
    assert!(stdout_of(&body_search).contains("Gradient Methods"));
}

#[test]
fn index_command_is_incremental_across_invocations() {
    let directory = tempdir().unwrap();
    let (library, _scratch) = fixture_library(directory.path());
    let library_dir = library.root().to_string_lossy().into_owned();

    let first = run_cli(&["index", "--library", &library_dir], directory.path());
    assert!(first.status.success());
    let stderr = stderr_of(&first);
    assert!(stderr.contains("added=2"), "stderr: {stderr}");

    // A second invocation skips both unchanged documents.
    let second = run_cli(&["index", "--library", &library_dir], directory.path());
    assert!(second.status.success());
    let stderr = stderr_of(&second);
    assert!(stderr.contains("skipped=2"), "stderr: {stderr}");
    assert!(stderr.contains("added=0"), "stderr: {stderr}");

    // JSON summary reports the same accounting (stdout is one JSON document).
    let json_run = run_cli(
        &["index", "--library", &library_dir, "--json"],
        directory.path(),
    );
    assert!(json_run.status.success());
    let json_out = stdout_of(&json_run);
    let summary: serde_json::Value =
        serde_json::from_str(&json_out).unwrap_or(serde_json::Value::Null);
    assert_eq!(
        summary.get("skipped").and_then(|value| value.as_u64()),
        Some(2)
    );
}

#[test]
fn search_command_filters_and_json_output() {
    let directory = tempdir().unwrap();
    let (library, _scratch) = fixture_library(directory.path());
    let library_dir = library.root().to_string_lossy().into_owned();

    for command in [
        vec!["extract", "--library", &library_dir],
        vec!["index", "--library", &library_dir],
    ] {
        let output = run_cli(&command, directory.path());
        assert!(output.status.success(), "{}", stderr_of(&output));
    }

    // Source filter.
    let by_source = run_cli(
        &[
            "search",
            "--library",
            &library_dir,
            "--query",
            "reporting OR recovery",
            "--source",
            "doaj",
        ],
        directory.path(),
    );
    assert!(by_source.status.success());
    let out = stdout_of(&by_source);
    assert!(out.contains("Clinical Trial"), "out: {out}");
    assert!(!out.contains("Gradient Methods"), "out: {out}");

    // DOI filter with empty query.
    let by_doi = run_cli(
        &["search", "--library", &library_dir, "--doi", "10.1000/clin"],
        directory.path(),
    );
    assert!(by_doi.status.success());
    assert!(stdout_of(&by_doi).contains("Clinical Trial"));

    // Year range filter.
    let by_year = run_cli(
        &[
            "search",
            "--library",
            &library_dir,
            "--query",
            "",
            "--year-min",
            "2024",
        ],
        directory.path(),
    );
    assert!(by_year.status.success());
    let out = stdout_of(&by_year);
    assert!(out.contains("Gradient Methods"), "out: {out}");
    assert!(!out.contains("Clinical Trial"), "out: {out}");

    // JSON output shape.
    let as_json = run_cli(
        &[
            "search",
            "--library",
            &library_dir,
            "--query",
            "sparse",
            "--json",
        ],
        directory.path(),
    );
    assert!(as_json.status.success());
    let json_out = stdout_of(&as_json);
    let parsed: serde_json::Value =
        serde_json::from_str(&json_out).expect("search --json must emit one JSON document");
    assert_eq!(
        parsed
            .get("indexed_documents")
            .and_then(|value| value.as_u64()),
        Some(2)
    );
    let hits = parsed
        .get("hits")
        .and_then(|value| value.as_array())
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].get("object_sha").is_some());
    assert!(hits[0].get("title").is_some());
    assert!(hits[0].get("text_available").is_some());
}

#[test]
fn search_command_reports_useful_errors() {
    let directory = tempdir().unwrap();
    let (library, _scratch) = fixture_library(directory.path());
    let library_dir = library.root().to_string_lossy().into_owned();

    // Empty query with no filters is an actionable usage error (exit code 2).
    let no_query = run_cli(&["search", "--library", &library_dir], directory.path());
    assert_eq!(no_query.status.code(), Some(2));
    let stderr = stderr_of(&no_query);
    assert!(
        stderr.contains("provide --query") || stderr.contains("at least one filter"),
        "stderr: {stderr}"
    );

    // Zero limit is rejected.
    let zero_limit = run_cli(
        &[
            "search",
            "--library",
            &library_dir,
            "--query",
            "anything",
            "--limit",
            "0",
        ],
        directory.path(),
    );
    assert_eq!(zero_limit.status.code(), Some(2));

    // Searching a library with no index at all yields a clear rebuild hint, not a panic.
    let empty_root = tempdir().unwrap();
    let empty_library = LibraryLayout::new(empty_root.path().join("library"));
    empty_library.create().unwrap();
    let no_index = run_cli(
        &[
            "search",
            "--library",
            &empty_library.root().to_string_lossy(),
            "--query",
            "anything",
        ],
        empty_root.path(),
    );
    assert_eq!(no_index.status.code(), Some(2));
    let stderr = stderr_of(&no_index);
    assert!(
        stderr.contains("rebuild") || stderr.contains("index"),
        "stderr: {stderr}"
    );
}
