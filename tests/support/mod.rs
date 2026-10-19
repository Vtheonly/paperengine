//! Shared helpers for the offline integration-test suites: a tiny sequential local
//! HTTP fixture server, a valid one-page PDF built with the real parser, and
//! acquisition-pipeline fixtures. No external network access occurs from here.
//!
//! Each integration test is its own crate and uses a different subset of these
//! helpers, so unused items here are expected.
#![allow(dead_code)]

use lopdf::dictionary;
use paperengine::fetcher::stream::{DownloadConfig, ResumableDownloader};
use paperengine::library::AcquisitionConfig;
use paperengine::sources::work_record::WorkRecord;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// One canned response for the fixture server.
pub struct Route {
    pub path: &'static str,
    pub status: &'static str,
    pub content_type: &'static str,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

impl Route {
    pub fn json(path: &'static str, body: String) -> Self {
        Self {
            path,
            status: "200 OK",
            content_type: "application/json",
            headers: Vec::new(),
            body: body.into_bytes(),
        }
    }

    pub fn xml(path: &'static str, body: String) -> Self {
        Self {
            path,
            status: "200 OK",
            content_type: "application/atom+xml",
            headers: Vec::new(),
            body: body.into_bytes(),
        }
    }

    pub fn with_status(mut self, status: &'static str) -> Self {
        self.status = status;
        self
    }

    pub fn with_header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }
}

/// A tiny sequential HTTP/1.1 server bound to an ephemeral port. Requests are matched
/// against routes by path prefix; unmatched paths return 404. The server records every
/// request it receives for assertions.
pub struct FixtureServer {
    pub base_url: String,
    requests: Arc<AtomicUsize>,
    _handle: thread::JoinHandle<()>,
}

impl FixtureServer {
    pub fn start(routes: Vec<Route>) -> Self {
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

    pub fn request_count(&self) -> usize {
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

/// A structurally valid one-page PDF built with lopdf, the same parser the
/// production validator uses.
pub fn small_pdf_bytes() -> Vec<u8> {
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

/// A work record with a direct PDF URL, ready for the acquisition pipeline.
pub fn record_with_pdf(source_id: &str, id: &str, pdf_url: impl Into<String>) -> WorkRecord {
    let mut record = WorkRecord::new(source_id, id);
    record.title = format!("Fixture paper {id}");
    record.publication_year = Some(2024);
    record.oa_pdf_url = Some(pdf_url.into());
    record
}

/// Bounded acquisition budgets tuned for the local fixture server.
pub fn acquisition_config() -> AcquisitionConfig {
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

/// The production downloader wired to the test budgets.
pub fn downloader_for(config: &AcquisitionConfig) -> ResumableDownloader {
    ResumableDownloader::new(DownloadConfig {
        max_bytes: config.max_bytes_per_file,
        min_bytes: config.min_bytes_per_file,
        request_timeout: config.request_timeout,
        total_timeout: config.total_timeout,
    })
    .unwrap()
}
