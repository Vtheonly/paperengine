//! Public acceptance tests for incremental indexing (#32): restart/rebuild parity,
//! updates, deletes, version separation, and canonical-metadata safety.

use lopdf::content::{Content, Operation};
use lopdf::dictionary;
use lopdf::{Dictionary, Document, Object, Stream};
use paperengine::extract::run_extraction;
use paperengine::index::{
    document_count, run_indexing, search_library, IndexingConfig, IndexingOutcome, SearchFilters,
};
use paperengine::library::{
    LibraryLayout, ProvenanceAcquisition, ProvenanceContent, ProvenanceManifest, ProvenanceWork,
};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use tempfile::tempdir;

/// Write a small text PDF and return (sha256, byte_length).
fn write_text_pdf(path: &Path, title_line: &str) -> (String, u64) {
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
    let content = Content {
        operations: vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 12.into()]),
            Operation::new("Td", vec![72.into(), 720.into()]),
            Operation::new("Tj", vec![Object::string_literal(title_line)]),
            Operation::new("Td", vec![0.into(), (-14).into()]),
            Operation::new(
                "Tj",
                vec![Object::string_literal("quantum dot synthesis method")],
            ),
            Operation::new("ET", vec![]),
        ],
    };
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

struct FixtureWork {
    source_id: &'static str,
    record_id: &'static str,
    title: &'static str,
    authors: Vec<&'static str>,
    year: Option<u16>,
    doi: Option<&'static str>,
    arxiv_id: Option<&'static str>,
}

fn write_work(
    library: &LibraryLayout,
    work: &FixtureWork,
    pdf_dir: &Path,
    title_line: &str,
) -> String {
    let safe_name = work.record_id.replace(['/', ':'], "_");
    let pdf_path = pdf_dir.join(format!("{safe_name}.pdf"));
    let (sha, bytes) = write_text_pdf(&pdf_path, title_line);
    let object_path = library.object_path(&sha).unwrap();
    fs::create_dir_all(object_path.parent().unwrap()).unwrap();
    fs::copy(&pdf_path, &object_path).unwrap();

    let identifiers = vec![
        work.doi
            .map(|doi| paperengine::sources::work_record::WorkIdentifier {
                kind: paperengine::sources::work_record::IdentifierKind::Doi,
                value: doi.to_owned(),
            }),
        work.arxiv_id
            .map(|arxiv| paperengine::sources::work_record::WorkIdentifier {
                kind: paperengine::sources::work_record::IdentifierKind::Arxiv,
                value: arxiv.to_owned(),
            }),
    ]
    .into_iter()
    .flatten()
    .collect();

    let manifest = ProvenanceManifest {
        schema_version: 1,
        work: ProvenanceWork {
            source_id: work.source_id.to_owned(),
            source_record_id: work.record_id.to_owned(),
            identifiers,
            title: work.title.to_owned(),
            authors: work
                .authors
                .iter()
                .map(|author| author.to_string())
                .collect(),
            publication_year: work.year,
            venue: None,
            license: Some("cc-by".to_owned()),
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
        .manifest_path(work.source_id, work.record_id, &sha)
        .unwrap();
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    sha
}

fn extraction_config() -> paperengine::extract::ExtractionConfig {
    paperengine::extract::ExtractionConfig {
        max_text_bytes: 16 * 1024 * 1024,
        max_objects: 256,
    }
}

fn indexing_config() -> IndexingConfig {
    IndexingConfig {
        memory_budget_bytes: 16 * 1024 * 1024,
    }
}

/// Full pipeline: store work, extract text, index it.
fn add_and_process_work(
    library: &LibraryLayout,
    scratch: &Path,
    work: &FixtureWork,
    title_line: &str,
) -> String {
    let sha = write_work(library, work, scratch, title_line);
    let _ = run_extraction(library, &extraction_config(), false, 1, |_| Ok(())).unwrap();
    let _ = run_indexing(library, &indexing_config(), false, |_| Ok(())).unwrap();
    sha
}

fn snapshot_canonical(library: &LibraryLayout) -> Vec<(String, String)> {
    let mut snapshot = Vec::new();
    for dir in ["objects", "manifests", "extraction", "extracted"] {
        let base = library.root().join(dir);
        let mut stack = vec![base.clone()];
        while let Some(dir_path) = stack.pop() {
            if !dir_path.is_dir() {
                continue;
            }
            for entry in fs::read_dir(&dir_path).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    stack.push(entry.path());
                } else {
                    let bytes = fs::read(entry.path()).unwrap();
                    snapshot.push((
                        entry.path().to_string_lossy().into_owned(),
                        format!("{:x}", Sha256::digest(&bytes)),
                    ));
                }
            }
        }
    }
    snapshot.sort();
    snapshot
}

fn sorted_titles(hits: &[paperengine::index::SearchHit]) -> Vec<String> {
    let mut titles: Vec<String> = hits.iter().map(|hit| hit.title.clone()).collect();
    titles.sort();
    titles
}

#[test]
fn incremental_indexing_reaches_rebuild_parity() {
    let directory = tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    library.create().unwrap();
    let scratch = directory.path().join("fixtures");
    fs::create_dir_all(&scratch).unwrap();

    let alpha = FixtureWork {
        source_id: "arxiv",
        record_id: "2401.00001",
        title: "Alpha Attention Networks",
        authors: vec!["Alice Author"],
        year: Some(2024),
        doi: None,
        arxiv_id: Some("2401.00001"),
    };
    add_and_process_work(&library, &scratch, &alpha, "alpha attention paper body");

    // Restart: a second, separate indexing call (simulating a restarted process) with a
    // new work added between runs.
    let beta = FixtureWork {
        source_id: "openalex",
        record_id: "W1",
        title: "Beta Biology Baselines",
        authors: vec!["Bob Author", "Carol Author"],
        year: Some(2023),
        doi: Some("10.1000/beta"),
        arxiv_id: None,
    };
    let _ = run_extraction(&library, &extraction_config(), false, 2, |_| Ok(())).unwrap();
    let sha_beta = write_work(&library, &beta, &scratch, "beta biology baseline body");
    let _ = run_extraction(&library, &extraction_config(), false, 2, |_| Ok(())).unwrap();

    // The second indexing run only processes the new manifest.
    let mut entries = Vec::new();
    let summary = run_indexing(&library, &indexing_config(), false, |entry| {
        entries.push(entry.clone());
        Ok(())
    })
    .unwrap();
    assert_eq!(summary.manifests_examined, 2);
    assert_eq!(summary.added, 1);
    assert_eq!(summary.skipped, 1);
    assert!(entries.iter().any(
        |entry| entry.object_sha == sha_beta && matches!(entry.outcome, IndexingOutcome::Added)
    ));

    let incremental_hits =
        search_library(&library, "alpha OR beta", &SearchFilters::default(), 100).unwrap();
    assert_eq!(incremental_hits.len(), 2);

    // Rebuild parity: a from-scratch rebuild of the same library yields the same set.
    let rebuild_summary = run_indexing(&library, &indexing_config(), true, |_| Ok(())).unwrap();
    assert_eq!(rebuild_summary.added, 2);
    assert_eq!(rebuild_summary.skipped, 0);
    let rebuilt_hits =
        search_library(&library, "alpha OR beta", &SearchFilters::default(), 100).unwrap();
    assert_eq!(
        sorted_titles(&rebuilt_hits),
        sorted_titles(&incremental_hits)
    );
    assert_eq!(document_count(&library).unwrap(), 2);

    // Restart parity: re-running indexing without changes skips everything.
    let settled = run_indexing(&library, &indexing_config(), false, |_| Ok(())).unwrap();
    assert_eq!(settled.skipped, 2);
    assert_eq!(settled.added, 0);
    assert_eq!(settled.updated, 0);
}

#[test]
fn full_text_search_finds_words_from_extracted_body() {
    let directory = tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    library.create().unwrap();
    let scratch = directory.path().join("fixtures");
    fs::create_dir_all(&scratch).unwrap();

    let work = FixtureWork {
        source_id: "zenodo",
        record_id: "12345",
        title: "Unrelated Title About Trees",
        authors: vec!["Dana Researcher"],
        year: Some(2025),
        doi: Some("10.5281/zenodo.12345"),
        arxiv_id: None,
    };
    add_and_process_work(
        &library,
        &scratch,
        &work,
        "unique zanzibar methodology token",
    );

    // The query word appears ONLY in the extracted body text, not in the title.
    let hits = search_library(&library, "zanzibar", &SearchFilters::default(), 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].title, "Unrelated Title About Trees");
    assert!(hits[0].text_available);
    assert_eq!(hits[0].doi.as_deref(), Some("10.5281/zenodo.12345"));
    assert_eq!(hits[0].publication_year, Some(2025));
    assert!(hits[0].page_count.is_some());
    assert_eq!(hits[0].source_id, "zenodo");

    // A word that appears nowhere returns no hits.
    let misses = search_library(&library, "xyzzyplugh", &SearchFilters::default(), 10).unwrap();
    assert!(misses.is_empty());
}

#[test]
fn filters_narrow_results_by_source_year_and_doi() {
    let directory = tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    library.create().unwrap();
    let scratch = directory.path().join("fixtures");
    fs::create_dir_all(&scratch).unwrap();

    let works = [
        (
            FixtureWork {
                source_id: "arxiv",
                record_id: "2401.11111",
                title: "Attention Variants",
                authors: vec!["Eve Analyst"],
                year: Some(2024),
                doi: None,
                arxiv_id: Some("2401.11111"),
            },
            "attention study of network variants",
        ),
        (
            FixtureWork {
                source_id: "openalex",
                record_id: "W2",
                title: "Attention In Biology",
                authors: vec!["Frank Biologist"],
                year: Some(2022),
                doi: Some("10.1000/bio"),
                arxiv_id: None,
            },
            "attention mechanisms in biology research",
        ),
    ];
    for (work, body) in works {
        add_and_process_work(&library, &scratch, &work, body);
    }

    let by_source = search_library(
        &library,
        "attention",
        &SearchFilters {
            source_id: Some("arxiv".to_owned()),
            ..SearchFilters::default()
        },
        10,
    )
    .unwrap();
    assert_eq!(by_source.len(), 1);
    assert_eq!(by_source[0].source_id, "arxiv");

    let by_doi = search_library(
        &library,
        "",
        &SearchFilters {
            doi: Some("10.1000/bio".to_owned()),
            ..SearchFilters::default()
        },
        10,
    )
    .unwrap();
    assert_eq!(by_doi.len(), 1);
    assert_eq!(by_doi[0].title, "Attention In Biology");

    let by_arxiv = search_library(
        &library,
        "",
        &SearchFilters {
            arxiv_id: Some("2401.11111".to_owned()),
            ..SearchFilters::default()
        },
        10,
    )
    .unwrap();
    assert_eq!(by_arxiv.len(), 1);

    let by_year = search_library(
        &library,
        "attention",
        &SearchFilters {
            year_min: Some(2023),
            ..SearchFilters::default()
        },
        10,
    )
    .unwrap();
    assert_eq!(by_year.len(), 1);
    assert_eq!(by_year[0].title, "Attention Variants");

    let all = search_library(&library, "", &SearchFilters::default(), 10);
    assert!(
        all.is_err(),
        "empty query with no filters must be an actionable error"
    );
}

#[test]
fn deleting_an_object_removes_its_index_document() {
    let directory = tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    library.create().unwrap();
    let scratch = directory.path().join("fixtures");
    fs::create_dir_all(&scratch).unwrap();

    let work = FixtureWork {
        source_id: "doaj",
        record_id: "999",
        title: "Vanishing Volume",
        authors: vec!["Grace Tester"],
        year: Some(2021),
        doi: Some("10.1000/vanish"),
        arxiv_id: None,
    };
    let sha = add_and_process_work(&library, &scratch, &work, "vanishing body text");
    assert_eq!(document_count(&library).unwrap(), 1);

    // Remove the object, manifest, extraction record, and text artifact: the work is gone.
    fs::remove_file(library.object_path(&sha).unwrap()).unwrap();
    let manifest_path = library.manifest_path("doaj", "999", &sha).unwrap();
    fs::remove_file(&manifest_path).unwrap();
    fs::remove_file(library.extraction_dir().join(format!("{sha}.json"))).unwrap();
    fs::remove_file(
        library
            .extracted_dir()
            .join(&sha[..2])
            .join(format!("{sha}.txt")),
    )
    .unwrap();

    let mut entries = Vec::new();
    let summary = run_indexing(&library, &indexing_config(), false, |entry| {
        entries.push(entry.clone());
        Ok(())
    })
    .unwrap();
    assert_eq!(summary.deleted, 1);
    assert!(entries
        .iter()
        .any(|entry| matches!(entry.outcome, IndexingOutcome::Deleted { .. })));
    assert_eq!(document_count(&library).unwrap(), 0);
    let hits = search_library(&library, "vanishing", &SearchFilters::default(), 10).unwrap();
    assert!(hits.is_empty());
}

#[test]
fn distinct_versions_of_one_work_are_both_retained() {
    let directory = tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    library.create().unwrap();
    let scratch = directory.path().join("fixtures");
    fs::create_dir_all(&scratch).unwrap();

    let v1 = FixtureWork {
        source_id: "arxiv",
        record_id: "2401.22222v1",
        title: "Versioned Work",
        authors: vec!["Heidi Reviser"],
        year: Some(2024),
        doi: None,
        arxiv_id: Some("2401.22222v1"),
    };
    let v2 = FixtureWork {
        source_id: "arxiv",
        record_id: "2401.22222v2",
        title: "Versioned Work",
        authors: vec!["Heidi Reviser"],
        year: Some(2024),
        doi: None,
        arxiv_id: Some("2401.22222v2"),
    };
    let sha_v1 = add_and_process_work(&library, &scratch, &v1, "first version body");
    let sha_v2 = add_and_process_work(&library, &scratch, &v2, "second version body revised");

    // Both versions are distinct objects with distinct documents.
    assert_ne!(sha_v1, sha_v2);
    assert_eq!(document_count(&library).unwrap(), 2);
    let hits = search_library(&library, "versioned", &SearchFilters::default(), 10).unwrap();
    assert_eq!(hits.len(), 2);
    let shas: Vec<&str> = hits.iter().map(|hit| hit.object_sha.as_str()).collect();
    assert!(shas.contains(&sha_v1.as_str()));
    assert!(shas.contains(&sha_v2.as_str()));
}

#[test]
fn failed_manifests_are_reported_and_canonical_metadata_is_untouched() {
    let directory = tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    library.create().unwrap();
    let scratch = directory.path().join("fixtures");
    fs::create_dir_all(&scratch).unwrap();

    let work = FixtureWork {
        source_id: "osf",
        record_id: "abcde",
        title: "Good Record",
        authors: vec!["Ivan Keeper"],
        year: Some(2026),
        doi: None,
        arxiv_id: None,
    };
    add_and_process_work(&library, &scratch, &work, "good record body");

    // Corrupt a second manifest so the indexing run must tolerate and report it.
    fs::write(
        library.manifests_dir().join("osf-broken-deadbeef.json"),
        b"{ not valid json",
    )
    .unwrap();

    let before = snapshot_canonical(&library);
    let mut entries = Vec::new();
    let summary = run_indexing(&library, &indexing_config(), false, |entry| {
        entries.push(entry.clone());
        Ok(())
    })
    .unwrap();
    assert_eq!(summary.failed, 1);
    assert!(entries.iter().any(|entry| matches!(
        &entry.outcome,
        IndexingOutcome::Failed { stage, .. } if stage == "load"
    )));

    // Canonical metadata is byte-identical after the failed run.
    let after = snapshot_canonical(&library);
    assert_eq!(before, after);

    // Repairing the manifest lets the next run index it: retries work.
    fs::remove_file(library.manifests_dir().join("osf-broken-deadbeef.json")).unwrap();
    let repaired = run_indexing(&library, &indexing_config(), false, |_| Ok(())).unwrap();
    assert_eq!(repaired.failed, 0);
    assert_eq!(repaired.skipped, 1);
    assert_eq!(document_count(&library).unwrap(), 1);
}

#[test]
fn extraction_appearing_later_updates_the_index_document() {
    let directory = tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    library.create().unwrap();
    let scratch = directory.path().join("fixtures");
    fs::create_dir_all(&scratch).unwrap();

    let work = FixtureWork {
        source_id: "europepmc",
        record_id: "E1",
        title: "Late Extraction",
        authors: vec!["Jack Late"],
        year: Some(2020),
        doi: Some("10.1000/late"),
        arxiv_id: None,
    };
    // Index BEFORE extraction: the document has no body text yet.
    let sha = write_work(&library, &work, &scratch, "late extraction body words");
    let _ = run_indexing(&library, &indexing_config(), false, |_| Ok(())).unwrap();
    let hits = search_library(&library, "extraction", &SearchFilters::default(), 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(!hits[0].text_available, "no extraction record yet");

    // Now extract and re-index: the state digest changes, so the document is updated.
    let _ = run_extraction(&library, &extraction_config(), false, 5, |_| Ok(())).unwrap();
    let mut entries = Vec::new();
    let summary = run_indexing(&library, &indexing_config(), false, |entry| {
        entries.push(entry.clone());
        Ok(())
    })
    .unwrap();
    assert_eq!(summary.updated, 1);
    assert!(entries
        .iter()
        .any(|entry| entry.object_sha == sha && matches!(entry.outcome, IndexingOutcome::Updated)));

    // Full-text search now finds words that only exist in the extracted body.
    let body_hits = search_library(&library, "extraction", &SearchFilters::default(), 10).unwrap();
    assert_eq!(body_hits.len(), 1);
    assert!(body_hits[0].text_available);
    assert!(body_hits[0].page_count.is_some());

    // ... and the text-only filter now matches.
    let text_only = search_library(
        &library,
        "",
        &SearchFilters {
            text_only: true,
            ..SearchFilters::default()
        },
        10,
    )
    .unwrap();
    assert_eq!(text_only.len(), 1);
}

#[test]
fn one_object_with_two_manifests_indexes_one_document() {
    let directory = tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    library.create().unwrap();
    let scratch = directory.path().join("fixtures");
    fs::create_dir_all(&scratch).unwrap();

    // The same bytes discovered through two sources: one object, two manifests.
    let via_arxiv = FixtureWork {
        source_id: "arxiv",
        record_id: "2401.33333",
        title: "Shared Payload Work",
        authors: vec!["Karen Dual"],
        year: Some(2025),
        doi: Some("10.1000/shared"),
        arxiv_id: Some("2401.33333"),
    };
    let via_unpaywall = FixtureWork {
        source_id: "unpaywall",
        record_id: "10.1000/shared",
        title: "Shared Payload Work (resolved copy)",
        authors: vec!["Karen Dual"],
        year: Some(2025),
        doi: Some("10.1000/shared"),
        arxiv_id: None,
    };
    let sha = write_work(
        &library,
        &via_arxiv,
        &scratch,
        "identical shared payload text",
    );
    write_work(
        &library,
        &via_unpaywall,
        &scratch,
        "identical shared payload text",
    );

    let _ = run_extraction(&library, &extraction_config(), false, 1, |_| Ok(())).unwrap();
    let mut entries = Vec::new();
    let summary = run_indexing(&library, &indexing_config(), false, |entry| {
        entries.push(entry.clone());
        Ok(())
    })
    .unwrap();

    // Both manifests are examined; the object is indexed exactly once.
    assert_eq!(summary.manifests_examined, 2);
    assert_eq!(summary.added, 1);
    assert_eq!(summary.skipped, 1);
    assert!(entries.iter().any(|entry| matches!(
        &entry.outcome,
        IndexingOutcome::Skipped { reason } if reason.contains("duplicate manifest")
    )));
    assert_eq!(document_count(&library).unwrap(), 1);

    // The single document is searchable and carries the first manifest's metadata.
    let hits = search_library(&library, "shared", &SearchFilters::default(), 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].object_sha, sha);
    assert_eq!(hits[0].source_id, "arxiv");
}
