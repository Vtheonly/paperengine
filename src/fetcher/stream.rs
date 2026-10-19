//! Bounded HTTP downloads with validator-guarded resume support.
//!
//! This module never bypasses access restrictions. Callers must validate source policy and
//! authorization before invoking it. Partial bytes are kept in a sidecar-backed .part file and
//! become visible at the destination path only after the response has completed and length checks
//! have passed. Content-addressed storage and multi-volume finalization are separate layers.

use futures_util::StreamExt;
use reqwest::header::HeaderMap;
use reqwest::{Client, Response, StatusCode, Url};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::time::Instant;

#[derive(Debug, Clone)]
pub struct DownloadConfig {
    /// Hard cap for the complete representation, in bytes.
    pub max_bytes: u64,
    /// Upper bound for one HTTP request, including streamed response time.
    pub request_timeout: Duration,
    /// Upper bound for the complete transfer, including resume-range requests.
    pub total_timeout: Duration,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            max_bytes: 512 * 1024 * 1024,
            request_timeout: Duration::from_secs(45),
            total_timeout: Duration::from_secs(15 * 60),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadResult {
    pub destination: PathBuf,
    pub bytes: u64,
    /// Byte offset safely reused from an earlier invocation. Zero when the server restarted
    /// the transfer or the saved partial lacked a usable validator.
    pub resumed_from: u64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("filesystem operation failed: {0}")]
    Io(#[from] io::Error),
    #[error("metadata serialization failed: {0}")]
    Metadata(#[from] serde_json::Error),
    #[error("HTTP status {0}")]
    HttpStatus(StatusCode),
    #[error("download deadline elapsed")]
    Timeout,
    #[error("response exceeds the configured maximum of {limit} bytes")]
    TooLarge { limit: u64 },
    #[error("invalid download response: {0}")]
    Protocol(String),
    #[error("destination already exists: {0}")]
    DestinationExists(PathBuf),
    #[error("invalid downloader configuration: {0}")]
    InvalidConfig(&'static str),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ResumeMetadata {
    source_url: String,
    etag: Option<String>,
    last_modified: Option<String>,
    total_length: Option<u64>,
}

impl ResumeMetadata {
    fn from_headers(source_url: &str, headers: &HeaderMap, total_length: Option<u64>) -> Self {
        Self {
            source_url: source_url.to_owned(),
            etag: header_text(headers, "etag"),
            last_modified: header_text(headers, "last-modified"),
            total_length,
        }
    }

    fn validator(&self) -> Option<(&'static str, &str)> {
        if let Some(etag) = self.etag.as_deref() {
            // If-Range must use a strong entity tag. A weak ETag is not a safe resume validator.
            if !etag.trim_start().starts_with("W/") && !etag.is_empty() {
                return Some(("if-range", etag));
            }
        }
        self.last_modified
            .as_deref()
            .filter(|value| !value.is_empty())
            .map(|value| ("if-range", value))
    }

    fn matches_response(&self, headers: &HeaderMap) -> bool {
        if let Some(etag) = self.etag.as_deref().filter(|tag| !tag.trim_start().starts_with("W/")) {
            return header_text(headers, "etag").as_deref() == Some(etag);
        }
        match self.last_modified.as_deref() {
            Some(value) if !value.is_empty() => {
                header_text(headers, "last-modified").as_deref() == Some(value)
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ContentRange {
    start: u64,
    end: u64,
    total: u64,
}

pub struct ResumableDownloader {
    client: Client,
    config: DownloadConfig,
}

impl ResumableDownloader {
    pub fn new(config: DownloadConfig) -> Result<Self, DownloadError> {
        if config.max_bytes == 0 {
            return Err(DownloadError::InvalidConfig("max_bytes must be positive"));
        }
        if config.request_timeout.is_zero() {
            return Err(DownloadError::InvalidConfig(
                "request_timeout must be positive",
            ));
        }
        if config.total_timeout.is_zero() {
            return Err(DownloadError::InvalidConfig(
                "total_timeout must be positive",
            ));
        }

        let client = Client::builder()
            .connect_timeout(config.request_timeout)
            .timeout(config.request_timeout)
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()?;
        Ok(Self { client, config })
    }

    /// Download one HTTP(S) representation to a destination file.
    ///
    /// Interrupted bodies retain their .part file and validator metadata when the server supplies
    /// an ETag or Last-Modified value. A subsequent call resumes only with Range + If-Range and
    /// validates Content-Range and the returned validator before appending. If Range is ignored,
    /// the object changed, or the server's range response is inconsistent, the partial is thrown
    /// away and a fresh request is made. The destination is created with a no-clobber hard link,
    /// so an existing completed file is never silently overwritten.
    pub async fn download(
        &self,
        url: &str,
        destination: impl AsRef<Path>,
    ) -> Result<DownloadResult, DownloadError> {
        let parsed_url = Url::parse(url)
            .map_err(|error| DownloadError::Protocol(format!("invalid URL: {error}")))?;
        if parsed_url.scheme() != "http" && parsed_url.scheme() != "https" {
            return Err(DownloadError::Protocol(
                "only http and https URLs are supported".to_owned(),
            ));
        }
        if !parsed_url.username().is_empty() || parsed_url.password().is_some() {
            return Err(DownloadError::Protocol(
                "credentials embedded in URLs are not accepted".to_owned(),
            ));
        }

        let source_url = parsed_url.to_string();
        let destination = destination.as_ref().to_path_buf();
        if path_exists(&destination).await? {
            return Err(DownloadError::DestinationExists(destination));
        }

        let parent = destination
            .parent()
            .filter(|value| !value.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        tokio::fs::create_dir_all(parent).await?;

        let part_path = append_suffix(&destination, ".part");
        let metadata_path = append_suffix(&destination, ".part.meta");
        let deadline = Instant::now() + self.config.total_timeout;

        let part_length = match tokio::fs::metadata(&part_path).await {
            Ok(metadata) if metadata.is_file() => Some(metadata.len()),
            Ok(_) => {
                return Err(DownloadError::Protocol(
                    "partial-download path exists but is not a regular file".to_owned(),
                ))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let loaded_metadata = match tokio::fs::read(&metadata_path).await {
            Ok(bytes) => serde_json::from_slice::<ResumeMetadata>(&bytes).ok(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };

        let resume_metadata = loaded_metadata.filter(|metadata| {
            metadata.source_url == source_url && metadata.validator().is_some()
        });
        let mut offset = match (part_length, resume_metadata.as_ref()) {
            (Some(length), Some(_)) if length > 0 => length,
            _ => 0,
        };
        let mut saved_metadata = resume_metadata;
        if offset == 0 {
            reset_partial(&part_path, &metadata_path).await?;
            saved_metadata = None;
        }
        let mut resumed_from = offset;
        let mut protocol_restarts = 0_u8;

        loop {
            if Instant::now() >= deadline {
                return Err(DownloadError::Timeout);
            }

            let request_offset = offset;
            let mut request = self.client.get(parsed_url.clone());
            if request_offset > 0 {
                let metadata = saved_metadata.as_ref().ok_or_else(|| {
                    DownloadError::Protocol("resume metadata disappeared".to_owned())
                })?;
                let (_, validator) = metadata.validator().ok_or_else(|| {
                    DownloadError::Protocol("partial file has no safe validator".to_owned())
                })?;
                request = request
                    .header("range", format!("bytes={request_offset}-"))
                    .header("if-range", validator);
            }

            let response = match tokio::time::timeout_at(deadline, request.send()).await {
                Err(_) => return Err(DownloadError::Timeout),
                Ok(Err(error)) if error.is_timeout() => return Err(DownloadError::Timeout),
                Ok(Err(error)) => return Err(DownloadError::Http(error)),
                Ok(Ok(response)) => response,
            };

            if request_offset > 0 && response.status() == StatusCode::OK {
                // If-Range changed or Range was ignored. Reuse this 200 response, but truncate
                // the old partial before any new bytes are written.
                reset_partial(&part_path, &metadata_path).await?;
                offset = 0;
                resumed_from = 0;
                saved_metadata = None;
            } else if request_offset > 0 && response.status() == StatusCode::RANGE_NOT_SATISFIABLE {
                if protocol_restarts >= 1 {
                    return Err(DownloadError::Protocol(
                        "server rejected a valid resume range twice".to_owned(),
                    ));
                }
                protocol_restarts += 1;
                reset_partial(&part_path, &metadata_path).await?;
                offset = 0;
                resumed_from = 0;
                saved_metadata = None;
                continue;
            } else if request_offset > 0 && response.status() == StatusCode::PARTIAL_CONTENT {
                let content_range = response
                    .headers()
                    .get("content-range")
                    .and_then(|value| value.to_str().ok())
                    .and_then(parse_content_range);
                let range_length_matches = content_range.is_some_and(|range| {
                    response
                        .content_length()
                        .map(|length| length == range.end - range.start + 1)
                        .unwrap_or(true)
                });
                let resume_matches = content_range.is_some_and(|range| {
                    range.start == request_offset
                        && saved_metadata
                            .as_ref()
                            .is_some_and(|saved| saved.matches_response(response.headers()))
                        && saved_metadata
                            .as_ref()
                            .and_then(|saved| saved.total_length)
                            .map(|total| total == range.total)
                            .unwrap_or(true)
                });

                if !range_length_matches || !resume_matches {
                    if protocol_restarts >= 1 {
                        return Err(DownloadError::Protocol(
                            "server returned an inconsistent resume response".to_owned(),
                        ));
                    }
                    protocol_restarts += 1;
                    reset_partial(&part_path, &metadata_path).await?;
                    offset = 0;
                    resumed_from = 0;
                    saved_metadata = None;
                    continue;
                }
            }

            let status = response.status();
            if status != StatusCode::OK && status != StatusCode::PARTIAL_CONTENT {
                return Err(DownloadError::HttpStatus(status));
            }

            let range = if status == StatusCode::PARTIAL_CONTENT {
                let parsed = response
                    .headers()
                    .get("content-range")
                    .and_then(|value| value.to_str().ok())
                    .and_then(parse_content_range)
                    .ok_or_else(|| {
                        DownloadError::Protocol(
                            "206 response has no valid finite Content-Range".to_owned(),
                        )
                    })?;
                if parsed.start != offset {
                    return Err(DownloadError::Protocol(format!(
                        "range starts at {}, expected {offset}",
                        parsed.start
                    )));
                }
                Some(parsed)
            } else {
                None
            };

            let total_length = range.map(|value| value.total).or_else(|| response.content_length());
            if let Some(total) = total_length {
                if total > self.config.max_bytes {
                    reset_partial(&part_path, &metadata_path).await?;
                    return Err(DownloadError::TooLarge {
                        limit: self.config.max_bytes,
                    });
                }
            }
            let expected_segment_bytes = match (range, response.content_length()) {
                (Some(value), Some(header_length))
                    if header_length != value.end - value.start + 1 =>
                {
                    return Err(DownloadError::Protocol(
                        "Content-Length disagrees with Content-Range".to_owned(),
                    ))
                }
                (Some(value), _) => Some(value.end - value.start + 1),
                (None, header_length) => header_length,
            };

            let response_metadata =
                ResumeMetadata::from_headers(&source_url, response.headers(), total_length);
            if range.is_some_and(|value| value.end + 1 < value.total)
                && response_metadata.validator().is_none()
            {
                return Err(DownloadError::Protocol(
                    "server split the range but supplied no safe validator for continuation"
                        .to_owned(),
                ));
            }
            save_metadata(&metadata_path, &response_metadata).await?;

            let append = offset > 0;
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .append(append)
                .truncate(!append)
                .open(&part_path)
                .await?;

            let received = pump_response_body(
                response,
                &mut file,
                offset,
                self.config.max_bytes,
                deadline,
            )
            .await;
            let received = match received {
                Ok(count) => count,
                Err(error) => {
                    // Preserve successfully written bytes so a later call can resume them.
                    let _ = file.sync_all().await;
                    drop(file);
                    if matches!(error, DownloadError::TooLarge { .. }) {
                        reset_partial(&part_path, &metadata_path).await?;
                    }
                    return Err(error);
                }
            };

            file.sync_all().await?;
            drop(file);

            if expected_segment_bytes.is_some_and(|expected| expected != received) {
                return Err(DownloadError::Protocol(format!(
                    "response body length mismatch: received {received} bytes"
                )));
            }

            let actual_length = tokio::fs::metadata(&part_path).await?.len();
            if actual_length > self.config.max_bytes {
                reset_partial(&part_path, &metadata_path).await?;
                return Err(DownloadError::TooLarge {
                    limit: self.config.max_bytes,
                });
            }

            if let Some(total) = total_length {
                if actual_length > total {
                    reset_partial(&part_path, &metadata_path).await?;
                    return Err(DownloadError::Protocol(
                        "download exceeds advertised representation length".to_owned(),
                    ));
                }
                if actual_length < total {
                    if actual_length <= offset || response_metadata.validator().is_none() {
                        return Err(DownloadError::Protocol(
                            "incomplete response cannot be safely continued".to_owned(),
                        ));
                    }
                    offset = actual_length;
                    saved_metadata = Some(response_metadata);
                    continue;
                }
            }

            // Hard-linking publishes a complete file atomically and refuses to overwrite an
            // existing destination. Part and destination are siblings, hence on the same volume.
            match tokio::fs::hard_link(&part_path, &destination).await {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    return Err(DownloadError::DestinationExists(destination));
                }
                Err(error) => return Err(error.into()),
            }
            let _ = tokio::fs::remove_file(&part_path).await;
            let _ = tokio::fs::remove_file(&metadata_path).await;

            return Ok(DownloadResult {
                destination,
                bytes: actual_length,
                resumed_from,
                etag: response_metadata.etag,
                last_modified: response_metadata.last_modified,
            });
        }
    }
}

async fn pump_response_body<W>(
    response: Response,
    output: &mut W,
    existing_bytes: u64,
    max_bytes: u64,
    deadline: Instant,
) -> Result<u64, DownloadError>
where
    W: AsyncWrite + Unpin,
{
    let mut stream = response.bytes_stream();
    let mut received = 0_u64;

    loop {
        let next = match tokio::time::timeout_at(deadline, stream.next()).await {
            Err(_) => return Err(DownloadError::Timeout),
            Ok(value) => value,
        };
        match next {
            None => break,
            Some(Err(error)) if error.is_timeout() => return Err(DownloadError::Timeout),
            Some(Err(error)) => return Err(DownloadError::Http(error)),
            Some(Ok(chunk)) => {
                let total = existing_bytes
                    .checked_add(received)
                    .and_then(|value| value.checked_add(chunk.len() as u64))
                    .ok_or(DownloadError::TooLarge { limit: max_bytes })?;
                if total > max_bytes {
                    return Err(DownloadError::TooLarge { limit: max_bytes });
                }
                output.write_all(&chunk).await?;
                received += chunk.len() as u64;
            }
        }
    }

    Ok(received)
}

fn parse_content_range(value: &str) -> Option<ContentRange> {
    let value = value.strip_prefix("bytes ")?;
    let (range, total) = value.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start = start.parse::<u64>().ok()?;
    let end = end.parse::<u64>().ok()?;
    let total = total.parse::<u64>().ok()?;
    if start > end || end >= total {
        return None;
    }
    Some(ContentRange { start, end, total })
}

fn header_text(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

async fn path_exists(path: &Path) -> Result<bool, DownloadError> {
    match tokio::fs::metadata(path).await {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

async fn remove_if_exists(path: &Path) -> Result<(), DownloadError> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

async fn reset_partial(part_path: &Path, metadata_path: &Path) -> Result<(), DownloadError> {
    remove_if_exists(part_path).await?;
    remove_if_exists(metadata_path).await
}

async fn save_metadata(path: &Path, metadata: &ResumeMetadata) -> Result<(), DownloadError> {
    let temporary = append_suffix(path, ".tmp");
    let bytes = serde_json::to_vec(metadata)?;
    tokio::fs::write(&temporary, bytes).await?;
    let file = tokio::fs::OpenOptions::new()
        .read(true)
        .open(&temporary)
        .await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(temporary, path).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::pin::Pin;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::task::{Context, Poll};
    use std::thread;

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    struct WireResponse {
        status: &'static str,
        headers: Vec<(&'static str, &'static str)>,
        body: Vec<u8>,
        declared_length: Option<usize>,
        delay: Duration,
    }

    impl WireResponse {
        fn new(status: &'static str, body: &[u8]) -> Self {
            Self {
                status,
                headers: Vec::new(),
                body: body.to_vec(),
                declared_length: None,
                delay: Duration::ZERO,
            }
        }

        fn header(mut self, name: &'static str, value: &'static str) -> Self {
            self.headers.push((name, value));
            self
        }

        fn declared_length(mut self, value: usize) -> Self {
            self.declared_length = Some(value);
            self
        }

        fn delay(mut self, value: Duration) -> Self {
            self.delay = value;
            self
        }
    }

    fn start_server(
        responses: Vec<WireResponse>,
    ) -> (String, thread::JoinHandle<Vec<String>>) {
        let _sequence = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let request = read_request(&mut stream);
                requests.push(request);
                if !response.delay.is_zero() {
                    thread::sleep(response.delay);
                }

                let length = response
                    .declared_length
                    .unwrap_or(response.body.len());
                let mut head = format!(
                    "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                    response.status, length
                );
                for (name, value) in response.headers {
                    head.push_str(name);
                    head.push_str(": ");
                    head.push_str(value);
                    head.push_str("\r\n");
                }
                head.push_str("\r\n");
                let _ = stream.write_all(head.as_bytes());
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

    fn test_config(max_bytes: u64) -> DownloadConfig {
        DownloadConfig {
            max_bytes,
            request_timeout: Duration::from_millis(800),
            total_timeout: Duration::from_secs(4),
        }
    }

    #[tokio::test]
    async fn accepts_a_complete_206_range_response() {
        let (url, server) = start_server(vec![WireResponse::new("206 Partial Content", b"data")
            .header("Content-Range", "bytes 0-3/4")
            .header("ETag", "\"v1\"")]);
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("paper.pdf");
        let result = ResumableDownloader::new(test_config(100))
            .unwrap()
            .download(&url, &destination)
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(result.bytes, 4);
        assert_eq!(result.resumed_from, 0);
        assert_eq!(std::fs::read(destination).unwrap(), b"data");
    }

    #[tokio::test]
    async fn resumes_interrupted_body_only_with_matching_validator() {
        let (url, server) = start_server(vec![
            WireResponse::new("200 OK", b"hello")
                .header("ETag", "\"v1\"")
                .declared_length(10),
            WireResponse::new("206 Partial Content", b"world")
                .header("Content-Range", "bytes 5-9/10")
                .header("ETag", "\"v1\""),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("paper.pdf");
        let downloader = ResumableDownloader::new(test_config(100)).unwrap();
        assert!(downloader.download(&url, &destination).await.is_err());
        let result = downloader.download(&url, &destination).await.unwrap();
        let requests = server.join().unwrap();

        assert_eq!(result.resumed_from, 5);
        assert_eq!(std::fs::read(destination).unwrap(), b"helloworld");
        assert!(requests[1].to_ascii_lowercase().contains("range: bytes=5-"));
        assert!(requests[1].to_ascii_lowercase().contains("if-range: \"v1\""));
    }

    #[tokio::test]
    async fn ignored_range_restarts_without_appending_to_partial_bytes() {
        let (url, server) = start_server(vec![
            WireResponse::new("200 OK", b"old!!")
                .header("ETag", "\"v1\"")
                .declared_length(10),
            WireResponse::new("200 OK", b"replacement")
                .header("ETag", "\"v2\""),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("paper.pdf");
        let downloader = ResumableDownloader::new(test_config(100)).unwrap();
        assert!(downloader.download(&url, &destination).await.is_err());
        let result = downloader.download(&url, &destination).await.unwrap();
        server.join().unwrap();

        assert_eq!(result.resumed_from, 0);
        assert_eq!(std::fs::read(destination).unwrap(), b"replacement");
    }

    #[tokio::test]
    async fn changed_object_in_206_forces_a_clean_full_restart() {
        let (url, server) = start_server(vec![
            WireResponse::new("200 OK", b"first")
                .header("ETag", "\"v1\"")
                .declared_length(10),
            WireResponse::new("206 Partial Content", b"other")
                .header("Content-Range", "bytes 5-9/10")
                .header("ETag", "\"v2\""),
            WireResponse::new("200 OK", b"new-object!")
                .header("ETag", "\"v2\""),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("paper.pdf");
        let downloader = ResumableDownloader::new(test_config(100)).unwrap();
        assert!(downloader.download(&url, &destination).await.is_err());
        let result = downloader.download(&url, &destination).await.unwrap();
        let requests = server.join().unwrap();

        assert_eq!(result.resumed_from, 0);
        assert_eq!(std::fs::read(destination).unwrap(), b"new-object!");
        assert!(requests[2].to_ascii_lowercase().find("range:").is_none());
    }

    #[tokio::test]
    async fn rejects_known_oversized_response_before_publishing_file() {
        let (url, server) =
            start_server(vec![WireResponse::new("200 OK", b"123456").header("ETag", "\"v1\"")]);
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("paper.pdf");
        let result = ResumableDownloader::new(test_config(5))
            .unwrap()
            .download(&url, &destination)
            .await;
        server.join().unwrap();

        assert!(matches!(result, Err(DownloadError::TooLarge { limit: 5 })));
        assert!(!destination.exists());
        assert!(!append_suffix(&destination, ".part").exists());
    }

    #[tokio::test]
    async fn enforces_request_timeout() {
        let (url, server) = start_server(vec![
            WireResponse::new("200 OK", b"late").delay(Duration::from_millis(350)),
        ]);
        let mut config = test_config(100);
        config.request_timeout = Duration::from_millis(100);
        config.total_timeout = Duration::from_secs(2);
        let dir = tempfile::tempdir().unwrap();
        let result = ResumableDownloader::new(config)
            .unwrap()
            .download(&url, dir.path().join("paper.pdf"))
            .await;
        server.join().unwrap();

        assert!(matches!(result, Err(DownloadError::Timeout)));
    }

    struct NoSpaceWriter;

    impl AsyncWrite for NoSpaceWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            _buffer: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Err(io::Error::from_raw_os_error(28)))
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn propagates_disk_full_write_errors_for_recovery() {
        let (url, server) = start_server(vec![WireResponse::new("200 OK", b"disk-space-test")]);
        let response = Client::new().get(url).send().await.unwrap();
        let mut output = NoSpaceWriter;
        let result = pump_response_body(
            response,
            &mut output,
            0,
            100,
            Instant::now() + Duration::from_secs(2),
        )
        .await;
        server.join().unwrap();

        match result {
            Err(DownloadError::Io(error)) => assert_eq!(error.raw_os_error(), Some(28)),
            other => panic!("expected simulated ENOSPC, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn refuses_unsupported_schemes_and_existing_destinations() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("paper.pdf");
        let downloader = ResumableDownloader::new(test_config(100)).unwrap();
        assert!(matches!(
            downloader.download("file:///etc/passwd", &destination).await,
            Err(DownloadError::Protocol(_))
        ));

        std::fs::write(&destination, b"already complete").unwrap();
        assert!(matches!(
            downloader.download("http://127.0.0.1:1/paper.pdf", &destination).await,
            Err(DownloadError::DestinationExists(_))
        ));
    }
}
