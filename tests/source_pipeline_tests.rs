//! Integration tests for source adapters and the end-to-end acquisition pipeline against
//! a local HTTP fixture server. No external network access occurs in this file.
//!
//! Covers: HTTP status classification (429/403/404/malformed), adapter discovery over the
//! wire, cursor pagination, and the full discover → policy gate → download → validate →
//! content-addressed storage → provenance path, including dedup, quarantine, policy
//! rejections, and budget exhaustion.

mod support;

use paperengine::jobs::JobPolicy;
use paperengine::library::{
    run_acquisition, AcquisitionOutcome, AcquisitionReportEntry, LibraryLayout,
};
use paperengine::sources::arxiv::{ArxivAdapter, ArxivConfig};
use paperengine::sources::europepmc::{EuropePmcAdapter, EuropePmcConfig};
use paperengine::sources::openalex::{OpenAlexAdapter, OpenAlexConfig};
use paperengine::sources::work_record::{IdentifierKind, WorkRecord};
use paperengine::sources::{DiscoveryQuery, SourceAdapter, SourceError};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::Duration;
use support::{
    acquisition_config, downloader_for, record_with_pdf, small_pdf_bytes, FixtureServer, Route,
};

fn openalex_for(server: &FixtureServer) -> OpenAlexAdapter {
    OpenAlexAdapter::new(OpenAlexConfig {
        api_base: Some(server.base_url.clone()),
        min_request_interval: Some(Duration::from_millis(0)),
        ..OpenAlexConfig::default()
    })
    .unwrap()
}

#[tokio::test]
async fn http_429_is_classified_as_rate_limited_with_retry_hint() {
    let server = FixtureServer::start(vec![Route::json(
        "/works",
        "{\"error\": \"slow down\"}".to_owned(),
    )
    .with_status("429 Too Many Requests")
    .with_header("Retry-After", "7")]);
    let adapter = openalex_for(&server);
    let error = adapter
        .discover(&DiscoveryQuery::new("test"), None)
        .await
        .unwrap_err();
    match error {
        SourceError::RateLimited {
            status,
            retry_after_seconds,
        } => {
            assert_eq!(status, 429);
            assert_eq!(retry_after_seconds, Some(7));
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

#[tokio::test]
async fn http_403_is_a_stop_condition_never_a_challenge() {
    let server = FixtureServer::start(vec![
        Route::json("/works", "{}".to_owned()).with_status("403 Forbidden")
    ]);
    let adapter = openalex_for(&server);
    let error = adapter
        .discover(&DiscoveryQuery::new("test"), None)
        .await
        .unwrap_err();
    assert!(matches!(error, SourceError::Forbidden { status: 403 }));
    // A single request is made; the engine does not hammer a refusing source.
    assert_eq!(server.request_count(), 1);
}

#[tokio::test]
async fn http_404_lookup_returns_none() {
    let server = FixtureServer::start(vec![
        Route::json("/works", "{}".to_owned()).with_status("404 Not Found")
    ]);
    let adapter = openalex_for(&server);
    assert!(adapter
        .lookup_doi("10.1000/missing")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn invalid_json_is_a_malformed_response_error() {
    let server = FixtureServer::start(vec![Route::json("/works", "not json".to_owned())]);
    let adapter = openalex_for(&server);
    let error = adapter
        .discover(&DiscoveryQuery::new("test"), None)
        .await
        .unwrap_err();
    assert!(matches!(error, SourceError::MalformedResponse { .. }));
}

// ---------------------------------------------------------------------------
// Adapter discovery over the wire
// ---------------------------------------------------------------------------

#[tokio::test]
async fn openalex_discovery_uses_cursor_pagination_over_http() {
    let page1 = serde_json::json!({
        "meta": { "next_cursor": "cursor-2" },
        "results": [
            {
                "id": "https://openalex.org/W1",
                "doi": "https://doi.org/10.1000/a",
                "display_name": "First Paper",
                "publication_year": 2024,
                "type": "article",
                "authorships": [],
                "best_oa_location": { "pdf_url": "https://example.org/first.pdf" }
            }
        ]
    })
    .to_string();
    let page2 = serde_json::json!({
        "meta": { "next_cursor": null },
        "results": [
            {
                "id": "https://openalex.org/W2",
                "doi": null,
                "display_name": "Second Paper",
                "publication_year": 2023,
                "type": "article",
                "authorships": [],
                "best_oa_location": null
            }
        ]
    })
    .to_string();
    let server = FixtureServer::start(vec![
        Route::json("/works", page1),
        Route::json("/works", page2),
    ]);
    let adapter = openalex_for(&server);

    let page = adapter
        .discover(
            &DiscoveryQuery {
                text: "papers".into(),
                page_size: 1,
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(page.records.len(), 1);
    assert_eq!(page.records[0].source_record_id, "W1");
    assert_eq!(
        page.records[0].oa_pdf_url.as_deref(),
        Some("https://example.org/first.pdf")
    );
    assert_eq!(page.next_cursor.as_deref(), Some("cursor-2"));

    let page = adapter
        .discover(
            &DiscoveryQuery {
                text: "papers".into(),
                page_size: 1,
            },
            page.next_cursor.as_deref(),
        )
        .await
        .unwrap();
    assert_eq!(page.records.len(), 1);
    assert_eq!(page.records[0].source_record_id, "W2");
    assert_eq!(page.records[0].oa_pdf_url, None);
    assert_eq!(page.next_cursor, None);
    assert_eq!(server.request_count(), 2);
}

#[tokio::test]
async fn arxiv_discovery_paginates_with_start_offsets() {
    let feed = r#"
<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom" xmlns:arxiv="http://arxiv.org/schemas/atom">
  <entry>
    <id>http://arxiv.org/abs/2401.00001v1</id>
    <title>Atom Paper</title>
    <published>2024-01-02T00:00:00Z</published>
    <link href="https://arxiv.org/abs/2401.00001v1" rel="alternate" type="text/html"/>
    <link href="https://arxiv.org/pdf/2401.00001v1" rel="related" type="application/pdf" title="pdf"/>
    <author><name>Author Name</name></author>
  </entry>
</feed>"#;
    let server = FixtureServer::start(vec![Route::xml("/api/query", feed.to_owned())]);
    let adapter = ArxivAdapter::new(ArxivConfig {
        api_base: Some(server.base_url.clone()),
        min_request_interval: Some(Duration::from_millis(0)),
        ..ArxivConfig::default()
    })
    .unwrap();

    let page = adapter
        .discover(
            &DiscoveryQuery {
                text: "atom".into(),
                page_size: 1,
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(page.records.len(), 1);
    assert_eq!(page.records[0].source_record_id, "2401.00001v1");
    assert_eq!(
        page.records[0].oa_pdf_url.as_deref(),
        Some("https://arxiv.org/pdf/2401.00001v1")
    );
    // A full page advertises a next cursor with the next start offset.
    assert_eq!(page.next_cursor.as_deref(), Some("1"));
}

fn europepmc_for(server: &FixtureServer) -> EuropePmcAdapter {
    EuropePmcAdapter::new(EuropePmcConfig {
        api_base: Some(server.base_url.clone()),
        min_request_interval: Some(Duration::from_millis(0)),
        ..EuropePmcConfig::default()
    })
    .unwrap()
}

fn epmc_page(next_cursor_mark: &str, id: &str, pmcid: Option<&str>) -> String {
    serde_json::json!({
        "hitCount": 42,
        "nextCursorMark": next_cursor_mark,
        "resultList": {
            "result": [
                {
                    "id": id,
                    "source": "MED",
                    "pmcid": pmcid,
                    "pmid": id,
                    "doi": format!("10.1371/journal.pone.{id}"),
                    "title": format!("Fixture record {id}"),
                    "authorString": "Doe J., Roe A.,",
                    "pubYear": "2024",
                    "isOpenAccess": "Y",
                    "inEPMC": "Y",
                    "hasPDF": "Y",
                    "license": "cc by",
                    "pubTypeList": { "pubType": ["Journal Article"] },
                    "journalInfo": { "journal": { "title": "PloS one" } },
                    "fullTextUrlList": { "fullTextUrl": [
                        {
                            "availability": "Subscription required",
                            "availabilityCode": "S",
                            "documentStyle": "doi",
                            "site": "DOI",
                            "url": format!("https://doi.org/10.1371/journal.pone.{id}")
                        },
                        {
                            "availability": "Open access",
                            "availabilityCode": "OA",
                            "documentStyle": "pdf",
                            "site": "Europe_PMC",
                            "url": format!("https://europepmc.org/articles/{}?pdf=render", pmcid.unwrap_or(""))
                        }
                    ]}
                }
            ]
        }
    })
    .to_string()
}

#[tokio::test]
async fn europepmc_discovery_uses_cursormark_pagination_over_http() {
    let server = FixtureServer::start(vec![
        Route::json("/search", epmc_page("cursor-two", "1", Some("PMC1111111"))),
        Route::json("/search", epmc_page("cursor-two", "2", Some("PMC2222222"))),
    ]);
    let adapter = europepmc_for(&server);

    let page = adapter
        .discover(
            &DiscoveryQuery {
                text: "attention".into(),
                page_size: 1,
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(page.records.len(), 1);
    assert_eq!(page.records[0].source_record_id, "MED/1");
    assert_eq!(
        page.records[0].identifier(IdentifierKind::Pmcid),
        Some("PMC1111111")
    );
    assert_eq!(
        page.records[0].oa_pdf_url.as_deref(),
        Some("https://europepmc.org/articles/PMC1111111?pdf=render")
    );
    // A full page with a fresh cursor advertises the next cursorMark.
    assert_eq!(page.next_cursor.as_deref(), Some("cursor-two"));

    // Feeding the same cursor back stops the traversal: nextCursorMark did not advance.
    let page = adapter
        .discover(
            &DiscoveryQuery {
                text: "attention".into(),
                page_size: 1,
            },
            page.next_cursor.as_deref(),
        )
        .await
        .unwrap();
    assert_eq!(page.records[0].source_record_id, "MED/2");
    assert_eq!(page.next_cursor, None);
    assert_eq!(server.request_count(), 2);
}

#[tokio::test]
async fn europepmc_doi_lookup_over_http_resolves_the_record() {
    let server = FixtureServer::start(vec![Route::json(
        "/search",
        epmc_page("*", "17299599", Some("PMC1790633")),
    )]);
    let adapter = europepmc_for(&server);
    let record = adapter
        .lookup_doi("10.1371/journal.pone.0000219")
        .await
        .unwrap()
        .expect("the DOI must resolve");
    assert_eq!(record.source_id, "europepmc");
    assert_eq!(record.source_record_id, "MED/17299599");
    assert_eq!(record.doi(), Some("10.1371/journal.pone.17299599"));
    assert_eq!(record.identifier(IdentifierKind::Pmcid), Some("PMC1790633"));
    assert_eq!(record.license.as_deref(), Some("cc by"));
}

// ---------------------------------------------------------------------------
// End-to-end acquisition pipeline (offline, local server)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn end_to_end_discover_then_acquire_into_content_addressed_library() {
    let pdf = small_pdf_bytes();
    let pdf_route = || Route {
        path: "/paper.pdf",
        status: "200 OK",
        content_type: "application/pdf",
        headers: vec![("ETag", "\"e2e-v1\"".to_owned())],
        body: pdf.clone(),
    };
    // Two acquisition runs (initial + dedup pass) each transfer the payload, so the
    // sequential fixture server needs the PDF route twice.
    let pdf_server = FixtureServer::start(vec![pdf_route(), pdf_route()]);
    let metadata = serde_json::json!({
        "meta": { "next_cursor": null },
        "results": [
            {
                "id": "https://openalex.org/W1234",
                "doi": "https://doi.org/10.1000/e2e",
                "display_name": "End to End Paper",
                "publication_year": 2024,
                "type": "article",
                "authorships": [
                    { "author": { "display_name": "Author One" } },
                    { "author": { "display_name": "Author Two" } }
                ],
                "best_oa_location": {
                    "pdf_url": format!("{}/paper.pdf", pdf_server.base_url),
                    "license": "cc-by"
                }
            }
        ]
    })
    .to_string();
    let discovery_server = FixtureServer::start(vec![Route::json("/works", metadata)]);
    let adapter = OpenAlexAdapter::new(OpenAlexConfig {
        api_base: Some(discovery_server.base_url.clone()),
        min_request_interval: Some(Duration::from_millis(0)),
        ..OpenAlexConfig::default()
    })
    .unwrap();

    // Discovery phase: metadata only, no payload bytes.
    let page = adapter
        .discover(
            &DiscoveryQuery {
                text: "end to end".into(),
                page_size: 5,
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(page.records.len(), 1);
    let record = page.records.into_iter().next().unwrap();
    assert_eq!(
        record.oa_pdf_url.as_deref(),
        Some(format!("{}/paper.pdf", pdf_server.base_url).as_str())
    );

    // Acquisition phase: the production pipeline over the record.
    let directory = tempfile::tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    let config = acquisition_config();
    let downloader = downloader_for(&config);

    let mut entries = Vec::new();
    let summary = run_acquisition(
        std::iter::once(&record),
        &library,
        &config,
        &downloader,
        &JobPolicy::default(),
        1_700_000_000,
        |entry: &AcquisitionReportEntry| {
            entries.push(serde_json::to_string(entry).unwrap());
            Ok(())
        },
    )
    .await
    .unwrap();

    assert_eq!(summary.acquired, 1);
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.quarantined, 0);

    let entry: AcquisitionReportEntry = serde_json::from_str(&entries[0]).unwrap();
    let AcquisitionOutcome::Acquired {
        byte_length,
        sha256_hex,
        page_count,
        object_path,
        deduplicated,
        ..
    } = entry.outcome
    else {
        panic!("expected acquisition to succeed, got {:?}", entry.outcome)
    };
    assert_eq!(byte_length, pdf.len() as u64);
    assert_eq!(page_count, 1);
    assert!(!deduplicated);
    assert_eq!(sha256_hex, format!("{:x}", Sha256::digest(&pdf)));

    // The finalized object exists, matches the payload, and is content-addressed.
    let object_path = Path::new(&object_path).to_path_buf();
    assert!(object_path.exists());
    assert_eq!(std::fs::read(&object_path).unwrap(), pdf);
    assert!(object_path.to_string_lossy().contains("objects"));

    // Provenance manifest identifies source, identifiers, retrieval time, and checksums.
    let manifests = library.list_manifests().unwrap();
    assert_eq!(manifests.len(), 1);
    let manifest = &manifests[0];
    assert_eq!(manifest.work.source_id, "openalex");
    assert_eq!(manifest.work.source_record_id, "W1234");
    assert_eq!(manifest.work.title, "End to End Paper");
    assert_eq!(manifest.content.sha256_hex, sha256_hex);
    assert_eq!(manifest.content.page_count, 1);
    assert_eq!(
        manifest.acquisition.retrieval_rfc3339,
        "2023-11-14T22:13:20Z"
    );
    assert!(manifest.acquisition.pdf_url.starts_with("http://127.0.0.1"));

    // Re-running the same acquisition is gated by the durable job ledger: the work is
    // already terminal-acquired, so no bytes are re-transferred and the run reports a
    // skip that names the explicit re-entry flag.
    let mut second_entries = Vec::new();
    let summary = run_acquisition(
        std::iter::once(&record),
        &library,
        &config,
        &downloader,
        &JobPolicy::default(),
        1_700_000_500,
        |entry: &AcquisitionReportEntry| {
            second_entries.push(serde_json::to_string(entry).unwrap());
            Ok(())
        },
    )
    .await
    .unwrap();
    assert_eq!(summary.acquired, 0);
    assert_eq!(summary.skipped, 1);
    let entry: AcquisitionReportEntry = serde_json::from_str(&second_entries[0]).unwrap();
    let AcquisitionOutcome::Skipped { reason } = entry.outcome else {
        panic!("expected skip, got {:?}", entry.outcome)
    };
    assert!(reason.contains("--refetch"), "{reason}");
    // No new manifest was created and no re-download happened.
    assert_eq!(library.list_manifests().unwrap().len(), 1);
    assert_eq!(pdf_server.request_count(), 1);

    // Explicit re-entry (--refetch) re-acquires and deduplicates to the same object.
    let mut third_entries = Vec::new();
    let summary = run_acquisition(
        std::iter::once(&record),
        &library,
        &config,
        &downloader,
        &JobPolicy {
            refetch: true,
            ..JobPolicy::default()
        },
        1_700_001_000,
        |entry: &AcquisitionReportEntry| {
            third_entries.push(serde_json::to_string(entry).unwrap());
            Ok(())
        },
    )
    .await
    .unwrap();
    assert_eq!(summary.acquired, 1);
    assert_eq!(summary.deduplicated, 1);
    let entry: AcquisitionReportEntry = serde_json::from_str(&third_entries[0]).unwrap();
    assert!(matches!(
        entry.outcome,
        AcquisitionOutcome::Acquired {
            deduplicated: true,
            ..
        }
    ));
    // Still exactly one object and one manifest; the second transfer hit the wire.
    assert_eq!(library.list_objects().unwrap().len(), 1);
    assert_eq!(library.list_manifests().unwrap().len(), 1);
    assert_eq!(pdf_server.request_count(), 2);
}

#[tokio::test]
async fn html_payload_is_quarantined_and_never_finalized() {
    let html = b"<!DOCTYPE html><html><body>restricted</body></html>".repeat(20);
    let server = FixtureServer::start(vec![Route {
        path: "/paper.pdf",
        status: "200 OK",
        content_type: "text/html",
        headers: Vec::new(),
        body: html.clone(),
    }]);
    let record = record_with_pdf(
        "openalex",
        "W-html",
        format!("{}/paper.pdf", server.base_url),
    );

    let directory = tempfile::tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    let config = acquisition_config();
    let downloader = downloader_for(&config);

    let mut entries = Vec::new();
    let summary = run_acquisition(
        std::iter::once(&record),
        &library,
        &config,
        &downloader,
        &JobPolicy::default(),
        1_700_000_000,
        |entry: &AcquisitionReportEntry| {
            entries.push(serde_json::to_string(entry).unwrap());
            Ok(())
        },
    )
    .await
    .unwrap();

    assert_eq!(summary.acquired, 0);
    assert_eq!(
        summary.quarantined, 0,
        "the downloader rejects HTML before validation"
    );
    assert_eq!(summary.failed, 1, "a rejected payload is a failed acquisition, not a quarantine, when the signature gate fires");
    let entry: AcquisitionReportEntry = serde_json::from_str(&entries[0]).unwrap();
    assert!(matches!(entry.outcome, AcquisitionOutcome::Failed { .. }));
    assert!(library.list_objects().unwrap().is_empty());
    assert!(library
        .tmp_dir()
        .read_dir()
        .map(|mut d| d.next().is_none())
        .unwrap());
}

#[tokio::test]
async fn policy_gate_rejects_disallowed_schemes_and_urls_without_payloads() {
    let directory = tempfile::tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    let mut strict_config = acquisition_config();
    strict_config.allow_http = false;
    let downloader = downloader_for(&strict_config);

    let http_record = record_with_pdf("openalex", "W-http", "http://example.org/p.pdf");
    let no_pdf = {
        let mut record = WorkRecord::new("crossref", "10.1000/meta");
        record.title = "Metadata only".into();
        record
    };
    let ftp_record = record_with_pdf("openalex", "W-ftp", "ftp://example.org/p.pdf");
    let credentials_record = record_with_pdf(
        "openalex",
        "W-creds",
        "https://user:secret@example.org/p.pdf",
    );

    let records = [http_record, no_pdf, ftp_record, credentials_record];
    let mut entries = Vec::new();
    let summary = run_acquisition(
        records.iter(),
        &library,
        &strict_config,
        &downloader,
        &JobPolicy::default(),
        1_700_000_000,
        |entry: &AcquisitionReportEntry| {
            entries.push(serde_json::to_string(entry).unwrap());
            Ok(())
        },
    )
    .await
    .unwrap();

    assert_eq!(summary.acquired, 0);
    assert_eq!(summary.skipped, 4);
    assert_eq!(summary.failed, 0);
    for line in &entries {
        let entry: AcquisitionReportEntry = serde_json::from_str(line).unwrap();
        let AcquisitionOutcome::Skipped { reason } = entry.outcome else {
            panic!("expected skip, got {:?}", entry.outcome)
        };
        assert!(!reason.is_empty());
    }
    assert!(library.list_objects().unwrap().is_empty());
}

#[tokio::test]
async fn download_budget_bounds_the_run_and_reports_exhaustion() {
    let pdf = small_pdf_bytes();
    let server = FixtureServer::start(vec![
        Route {
            path: "/one.pdf",
            status: "200 OK",
            content_type: "application/pdf",
            headers: Vec::new(),
            body: pdf.clone(),
        },
        Route {
            path: "/two.pdf",
            status: "200 OK",
            content_type: "application/pdf",
            headers: Vec::new(),
            body: pdf.clone(),
        },
    ]);
    let first = record_with_pdf("openalex", "W-1", format!("{}/one.pdf", server.base_url));
    let second = record_with_pdf("openalex", "W-2", format!("{}/two.pdf", server.base_url));

    let directory = tempfile::tempdir().unwrap();
    let library = LibraryLayout::new(directory.path().join("library"));
    let mut config = acquisition_config();
    config.max_downloads = 1;
    let downloader = downloader_for(&config);

    let records = [first, second];
    let mut entries = Vec::new();
    let summary = run_acquisition(
        records.iter(),
        &library,
        &config,
        &downloader,
        &JobPolicy::default(),
        1_700_000_000,
        |entry: &AcquisitionReportEntry| {
            entries.push(serde_json::to_string(entry).unwrap());
            Ok(())
        },
    )
    .await
    .unwrap();

    assert_eq!(summary.acquired, 1);
    assert_eq!(summary.skipped, 1);
    assert!(summary.download_budget_exhausted);
    let last: AcquisitionReportEntry = serde_json::from_str(entries.last().unwrap()).unwrap();
    assert!(matches!(last.outcome, AcquisitionOutcome::Skipped { .. }));
    // Only one transfer hit the wire.
    assert_eq!(server.request_count(), 1);
}
