use paperengine::fetcher::stream::{DownloadConfig, DownloadError, ResumableDownloader};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

struct ResponseFixture {
    status: &'static str,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
    advertised_length: Option<usize>,
}

impl ResponseFixture {
    fn new(status: &'static str, body: Vec<u8>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body,
            advertised_length: None,
        }
    }

    fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    fn advertised_length(mut self, length: usize) -> Self {
        self.advertised_length = Some(length);
        self
    }
}

fn start_server(responses: Vec<ResponseFixture>) -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();

    let handle = thread::spawn(move || {
        let mut requests = Vec::new();

        for response in responses {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            requests.push(read_request(&mut stream));

            let length = response.advertised_length.unwrap_or(response.body.len());
            let mut headers = format!(
                "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                response.status, length
            );
            for (name, value) in response.headers {
                headers.push_str(name);
                headers.push_str(": ");
                headers.push_str(&value);
                headers.push_str("\r\n");
            }
            headers.push_str("\r\n");

            let _ = stream.write_all(headers.as_bytes());
            let _ = stream.write_all(&response.body);
            let _ = stream.flush();
            let _ = stream.shutdown(Shutdown::Both);
        }

        requests
    });

    (format!("http://{address}/paper.pdf"), handle)
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];

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

fn pdf_payload(size: usize) -> Vec<u8> {
    assert!(size >= 5);
    let mut payload = b"%PDF-1.7\n".to_vec();
    payload.resize(size, b'P');
    payload
}

#[tokio::test]
async fn public_api_downloads_and_hashes_a_complete_payload() {
    let payload = pdf_payload(2048);
    let (url, server) = start_server(vec![ResponseFixture::new("200 OK", payload.clone())
        .header("Content-Type", "application/pdf")
        .header("ETag", "\"v1\"")]);
    let directory = tempfile::tempdir().unwrap();
    let destination = directory.path().join("paper.pdf");

    let result = ResumableDownloader::new(DownloadConfig::default())
        .unwrap()
        .download(&url, &destination)
        .await
        .unwrap();
    server.join().unwrap();

    assert_eq!(std::fs::read(&destination).unwrap(), payload);
    assert_eq!(result.bytes, payload.len() as u64);
    assert_eq!(result.resumed_from, 0);
    assert_eq!(
        result.blake3_hex,
        blake3::hash(&payload).to_hex().to_string()
    );
    assert_eq!(result.sha256_hex, format!("{:x}", Sha256::digest(&payload)));
}

#[tokio::test]
async fn public_api_resumes_only_with_matching_range_and_validator() {
    let payload = pdf_payload(4096);
    let split = 1500;
    let (url, server) = start_server(vec![
        ResponseFixture::new("200 OK", payload[..split].to_vec())
            .header("Content-Type", "application/pdf")
            .header("ETag", "\"stable-v1\"")
            .advertised_length(payload.len()),
        ResponseFixture::new("206 Partial Content", payload[split..].to_vec())
            .header("Content-Type", "application/pdf")
            .header(
                "Content-Range",
                format!(
                    "bytes {split}-{}/{total}",
                    payload.len() - 1,
                    total = payload.len()
                ),
            )
            .header("ETag", "\"stable-v1\""),
    ]);
    let directory = tempfile::tempdir().unwrap();
    let destination = directory.path().join("paper.pdf");
    let downloader = ResumableDownloader::new(DownloadConfig::default()).unwrap();

    assert!(downloader.download(&url, &destination).await.is_err());
    let result = downloader.download(&url, &destination).await.unwrap();
    let requests = server.join().unwrap();

    assert_eq!(std::fs::read(&destination).unwrap(), payload);
    assert_eq!(result.resumed_from, split as u64);
    assert!(requests[1]
        .to_ascii_lowercase()
        .contains("range: bytes=1500-"));
    assert!(requests[1]
        .to_ascii_lowercase()
        .contains("if-range: \"stable-v1\""));
    assert_eq!(
        result.blake3_hex,
        blake3::hash(&std::fs::read(&destination).unwrap())
            .to_hex()
            .to_string()
    );
    assert_eq!(
        result.sha256_hex,
        format!("{:x}", Sha256::digest(std::fs::read(&destination).unwrap()))
    );
}

#[tokio::test]
async fn public_api_rejects_html_and_removes_unpublishable_partial_files() {
    let html = b"<!DOCTYPE html><html><body>access restricted</body></html>".to_vec();
    let (url, server) = start_server(vec![
        ResponseFixture::new("200 OK", html).header("Content-Type", "text/html")
    ]);
    let directory = tempfile::tempdir().unwrap();
    let destination = directory.path().join("paper.pdf");

    let result = ResumableDownloader::new(DownloadConfig::default())
        .unwrap()
        .download(&url, &destination)
        .await;
    server.join().unwrap();

    assert!(matches!(
        result,
        Err(DownloadError::InvalidSignature { .. })
    ));
    assert!(!destination.exists());
    assert!(!directory.path().join("paper.pdf.part").exists());
    assert!(!directory.path().join("paper.pdf.part.meta").exists());
}
