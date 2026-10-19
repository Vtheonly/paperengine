//! Integration tests for source adapters and the end-to-end acquisition pipeline against
//! a local HTTP fixture server. No external network access occurs in this file.
//!
//! Covers: HTTP status classification (429/403/404/malformed), adapter discovery over the
//! wire, cursor pagination, and the full discover → policy gate → download → validate →
//! content-addressed storage → provenance path, including dedup, quarantine, policy
//! rejections, and budget exhaustion.

use lopdf::dictionary;
use paperengine::fetcher::stream::{DownloadConfig, ResumableDownloader};
use paperengine::library::{
    run_acquisition, AcquisitionConfig, AcquisitionOutcome, AcquisitionReportEntry, LibraryLayout,
};
use paperengine::sources::arxiv::{ArxivAdapter, ArxivConfig};
use paperengine::sources::openalex::{OpenAlexAdapter, OpenAlexConfig};
use paperengine::sources::work_record::WorkRecord;
use paperengine::sources::{DiscoveryQuery, SourceAdapter, SourceError};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Local HTTP fixture server
// ---------------------------------------------------------------------------

struct Route {
    path: &'static str,
    status: &'static str,
    content_type: &'static str,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

impl Route {
    fn json(path: &'static str, body: String) -> Self {
        Self {
            path,
            status: "200 OK",
            content_type: "application/json",
            headers: Vec::new(),
            body: body.into_bytes(),
        }
    }

    fn xml(path: &'static str, body: String) -> Self {
        Self {
            path,
            status: "200 OK",
            content_type: "application/atom+xml",
            headers: Vec::new(),
            body: body.into_bytes(),
        }
    }

    fn with_status(mut self, status: &'static str) -> Self {
        self.status = status;
        self
    }

    fn with_header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }
}

/// A tiny sequential HTTP/1.1 server bound to an ephemeral port. Requests are matched
/// against routes by path prefix; unmatched paths return 404. The server records every
/// request it receives for assertions.
struct FixtureServer {
    base_url: String,
    requests: Arc<AtomicUsize>,
    _handle: thread::JoinHandle<()>,
}

impl FixtureServer {
    fn start(routes: Vec<Route>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let address = listener.local_addr().expect("local address");
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);

        let handle = thread::spawn(move || {
            for route in routes {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(_) => return,
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let request = read_request(&mut stream);
                counter.fetch_add(1, Ordering::SeqCst);
                let hits_expected_path = request.starts_with("GET ")
                    && request.contains(&format!(" {} ", route.path).replace("  ", " "));
                let path_matches = request_path(&request)
                    .map(|path| path.starts_with(route.path))
                    .unwrap_or(false);
                if !hits_expected_path && !path_matches {
                    let body = b"not found";
                    write_response(
                        &mut stream,
                        "404 Not Found",
                        "text/plain",
                        &[],
                        body.to_vec(),
                    );
                    continue;
                }
                write_response(
                    &mut stream,
                    route.status,
                    route.content_type,
                    &route.headers,
                    route.body.clone(),
                );
                let _ = stream.flush();
                let _ = stream.shutdown(Shutdown::Both);
            }
        });

        Self {
            base_url: format!("http://{address}"),
            requests,
            _handle: handle,
        }
    }

    fn request_count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

fn request_path(request: &str) -> Option<String> {
    let first_line = request.lines().next()?;
    let mut parts = first_line.split(' ');
    let _method = parts.next()?;
    let target = parts.next()?;
    Some(target.split('?').next().unwrap_or(target).to_owned())
}

fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    extra_headers: &[(&'static str, String)],
    body: Vec<u8>,
) {
    let mut headers = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in extra_headers {
        headers.push_str(name);
        headers.push_str(": ");
        headers.push_str(value);
        headers.push_str("\r\n");
    }
    headers.push_str("\r\n");
    let _ = stream.write_all(headers.as_bytes());
    let _ = stream.write_all(&body);
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let count = match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => count,
        };
        request.extend_from_slice(&buffer[..count]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8_lossy(&request).into_owned()
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn small_pdf_bytes() -> Vec<u8> {
    let mut document = lopdf::Document::with_version("1.5");
    let pages_id = document.new_object_id();
    let content_id = document.add_object(lopdf::Stream::new(
        lopdf::Dictionary::new(),
        b"q Q\n".repeat(1024),
    ));
    let page_id = document.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "Contents" => content_id,
        "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        "Resources" => dictionary! {},
    });
    document.objects.insert(
        pages_id,
        lopdf::Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page_id.into()],
            "Count" => 1,
        }),
    );
    let catalog_id = document.add_object(lopdf::dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    document.trailer.set("Root", catalog_id);
    let mut bytes = Vec::new();
    document.save_to(&mut bytes).expect("serialize PDF");
    bytes
}

fn record_with_pdf(source_id: &str, id: &str, pdf_url: impl Into<String>) -> WorkRecord {
    let mut record = WorkRecord::new(source_id, id);
    record.title = format!("Fixture paper {id}");
    record.publication_year = Some(2024);
    record.oa_pdf_url = Some(pdf_url.into());
    record
}

// ---------------------------------------------------------------------------
// HTTP status classification
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// End-to-end acquisition pipeline (offline, local server)
// ---------------------------------------------------------------------------

fn acquisition_config() -> AcquisitionConfig {
    AcquisitionConfig {
        max_downloads: 3,
        min_bytes_per_file: 256,
        max_bytes_per_file: 8 * 1024 * 1024,
        max_total_bytes: 16 * 1024 * 1024,
        request_timeout: Duration::from_secs(10),
        total_timeout: Duration::from_secs(30),
        inter_download_delay: Duration::from_millis(0),
        allow_http: true,
    }
}

fn downloader_for(config: &AcquisitionConfig) -> ResumableDownloader {
    ResumableDownloader::new(DownloadConfig {
        max_bytes: config.max_bytes_per_file,
        min_bytes: config.min_bytes_per_file,
        request_timeout: config.request_timeout,
        total_timeout: config.total_timeout,
    })
    .unwrap()
}

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

    // Re-running the same acquisition deduplicates to the same object.
    let mut second_entries = Vec::new();
    let summary = run_acquisition(
        std::iter::once(&record),
        &library,
        &config,
        &downloader,
        1_700_000_500,
        |entry: &AcquisitionReportEntry| {
            second_entries.push(serde_json::to_string(entry).unwrap());
            Ok(())
        },
    )
    .await
    .unwrap();
    assert_eq!(summary.acquired, 1);
    assert_eq!(summary.deduplicated, 1);
    let entry: AcquisitionReportEntry = serde_json::from_str(&second_entries[0]).unwrap();
    assert!(matches!(
        entry.outcome,
        AcquisitionOutcome::Acquired {
            deduplicated: true,
            ..
        }
    ));
    // No new manifest was created for the duplicate.
    assert_eq!(library.list_manifests().unwrap().len(), 1);
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
