//! OSF preprints adapter (#12).
//!
//! Uses the documented OSF API v2 (`https://api.osf.io/v2`, JSON:API media type).
//! Access basis: public API; no account needed for open preprints. Discovery filters
//! preprints by title substring. Each preprint exposes a `primary_file` relationship;
//! resolving it costs one extra request per record, which the rate policy spaces.
//! The resolved file's `links.download` URL is recorded as `oa_pdf_url` only when
//! the preprint is published and the primary file is a `.pdf` — verified live (the
//! mapped URL returns bytes starting with `%PDF-`).

use crate::sources::contract::{
    Capability, DiscoveryPage, DiscoveryQuery, RatePolicy, SelfTestReport, SourceAdapter,
    SourceDescriptor, SourceError, SOURCE_ADAPTER_CONTRACT_VERSION,
};
use crate::sources::http::{SourceHttpClient, SourceHttpClientOptions};
use crate::sources::normalize::{normalize_doi, normalize_title, normalize_year};
use crate::sources::work_record::{IdentifierKind, WorkRecord};
use async_trait::async_trait;
use std::time::Duration;

pub const OSF_DEFAULT_API_BASE: &str = "https://api.osf.io/v2";
pub const OSF_MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(1_000);

pub struct OsfAdapter {
    descriptor: SourceDescriptor,
    client: SourceHttpClient,
    api_base: String,
}

#[derive(Debug, Clone, Default)]
pub struct OsfConfig {
    pub request_timeout: Option<Duration>,
    pub min_request_interval: Option<Duration>,
    /// Override for tests and mirrors; defaults to the public API.
    pub api_base: Option<String>,
}

/// Parsed `primary_file` object: enough to decide and record a permitted download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsfPrimaryFile {
    pub guid: String,
    pub name: String,
    pub download_url: String,
}

impl OsfAdapter {
    pub fn new(config: OsfConfig) -> Result<Self, SourceError> {
        let client = SourceHttpClient::new(SourceHttpClientOptions {
            request_timeout: config.request_timeout.unwrap_or(Duration::from_secs(30)),
            min_request_interval: config
                .min_request_interval
                .unwrap_or(OSF_MIN_REQUEST_INTERVAL),
            accept: Some("application/vnd.api+json".to_owned()),
            ..SourceHttpClientOptions::default()
        })?;
        Ok(Self {
            descriptor: SourceDescriptor {
                id: "osf",
                display_name: "OSF Preprints",
                adapter_version: 1,
                contract_version: SOURCE_ADAPTER_CONTRACT_VERSION,
                api_base: "https://api.osf.io/v2",
                access_basis: "public JSON:API; open preprint servers; \
                               resolved primary-file downloads",
                capabilities: &[Capability::Discovery, Capability::DirectPdfUrls],
                rate_policy: RatePolicy {
                    min_request_interval: OSF_MIN_REQUEST_INTERVAL,
                    default_page_size: 10,
                    max_page_size: 50,
                },
            },
            client,
            api_base: config
                .api_base
                .unwrap_or_else(|| OSF_DEFAULT_API_BASE.to_owned()),
        })
    }

    fn preprints_url(&self, query: &str, page: usize, page_size: u16) -> String {
        format!(
            "{}/preprints/?filter[title]={}&page={page}&page[size]={page_size}",
            self.api_base,
            urlencode(query)
        )
    }

    fn parse_preprint(&self, item: &serde_json::Value) -> Option<(WorkRecord, Option<String>)> {
        let id = item.get("id")?.as_str()?;
        let attributes = item.get("attributes")?;
        let mut record = WorkRecord::new("osf", id.to_owned());
        record.push_identifier(
            IdentifierKind::Doi,
            attributes
                .get("doi")
                .and_then(|doi| doi.as_str())
                .and_then(normalize_doi),
        );
        record.title = attributes
            .get("title")
            .and_then(|t| t.as_str())
            .map(normalize_title)
            .unwrap_or_default();
        record.authors = attributes
            .get("authors")
            .and_then(|a| a.as_array())
            .map(|authors| {
                authors
                    .iter()
                    .filter_map(|author| author.get("name").and_then(|n| n.as_str()))
                    .map(normalize_title)
                    .collect()
            })
            .unwrap_or_default();
        for date_field in ["date_published", "date_created", "date_modified"] {
            let year = attributes
                .get(date_field)
                .and_then(|date| date.as_str())
                .and_then(|date| date.trim().get(..4).and_then(|y| y.parse::<u64>().ok()))
                .and_then(normalize_year);
            if let Some(year) = year {
                record.publication_year = Some(year);
                break;
            }
        }
        record.license = attributes
            .get("license")
            .and_then(|license| license.as_str())
            .map(|license| license.to_owned());
        record.landing_url = item
            .get("links")
            .and_then(|links| links.get("html"))
            .and_then(|html| html.as_str())
            .map(|html| html.to_owned());
        record.raw = serde_json::json!({
            "preprint": item.clone(),
        });
        // Only published preprints are eligible for full-text resolution.
        let published = attributes
            .get("is_published")
            .and_then(|p| p.as_bool())
            .unwrap_or(false);
        let primary_file_url = item
            .get("relationships")
            .and_then(|relationships| relationships.get("primary_file"))
            .and_then(|primary| primary.get("links"))
            .and_then(|links| links.get("related"))
            .and_then(|related| related.get("href"))
            .and_then(|href| href.as_str())
            .map(|href| href.to_owned());
        if published {
            Some((record, primary_file_url))
        } else {
            Some((record, None))
        }
    }

    fn apply_primary_file(&self, record: &mut WorkRecord, file: &OsfPrimaryFile) {
        if file.name.to_ascii_lowercase().ends_with(".pdf") {
            record.oa_pdf_url = Some(file.download_url.clone());
            record.raw["resolved_primary_file"] = serde_json::json!({
                "guid": file.guid,
                "name": file.name,
                "download_url": file.download_url,
            });
        }
    }
}

fn urlencode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            _ => {
                encoded.push('%');
                encoded.push_str(&format!("{byte:02X}"));
            }
        }
    }
    encoded
}

/// Parse one OSF `files` document (`/v2/files/<guid>/`) into a resolvable primary file.
pub fn parse_primary_file(document: &serde_json::Value) -> Option<OsfPrimaryFile> {
    let data = document.get("data")?;
    let attributes = data.get("attributes")?;
    let guid = data.get("id")?.as_str()?;
    let name = attributes.get("name")?.as_str()?;
    let download_url = data
        .get("links")
        .and_then(|links| links.get("download"))
        .and_then(|download| download.as_str())?;
    if guid.is_empty() || name.is_empty() || download_url.is_empty() {
        return None;
    }
    Some(OsfPrimaryFile {
        guid: guid.to_owned(),
        name: name.to_owned(),
        download_url: download_url.replacen("http://", "https://", 1),
    })
}

#[async_trait]
impl SourceAdapter for OsfAdapter {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    fn self_test(&self) -> SelfTestReport {
        SelfTestReport {
            source_id: self.descriptor.id.to_owned(),
            adapter_version: self.descriptor.adapter_version,
            contract_version: self.descriptor.contract_version,
            capabilities: self
                .descriptor
                .capabilities
                .iter()
                .map(|capability| capability.as_str().to_owned())
                .collect(),
            configured: true,
            notes: vec![
                "discovery via /preprints?filter[title]=… (page-based)".to_owned(),
                "doi lookup unsupported: OSF exposes no documented DOI filter".to_owned(),
                "direct PDF from the resolved primary file of published preprints".to_owned(),
                "resolving primary files costs one extra request per record".to_owned(),
            ],
        }
    }

    async fn discover(
        &self,
        query: &DiscoveryQuery,
        cursor: Option<&str>,
    ) -> Result<DiscoveryPage, SourceError> {
        let page_size = if query.page_size == 0 {
            self.descriptor.rate_policy.default_page_size
        } else {
            query
                .page_size
                .min(self.descriptor.rate_policy.max_page_size)
        };
        let page: usize = cursor
            .and_then(|cursor| cursor.parse().ok())
            .filter(|page| *page >= 1)
            .unwrap_or(1);
        let url = self.preprints_url(&query.text, page, page_size);
        let body = self.client.get_json(&url).await?;
        let data = body
            .get("data")
            .and_then(|data| data.as_array())
            .ok_or_else(|| SourceError::MalformedResponse {
                detail: "missing `data` array".to_owned(),
            })?;
        let total = body
            .get("links")
            .and_then(|links| links.get("meta"))
            .and_then(|meta| meta.get("total"))
            .and_then(|total| total.as_u64())
            .ok_or_else(|| SourceError::MalformedResponse {
                detail: "missing links.meta.total".to_owned(),
            })?;
        let mut records = Vec::with_capacity(data.len());
        for item in data {
            let Some((mut record, primary_file_url)) = self.parse_preprint(item) else {
                continue;
            };
            // Resolve the primary file only for published preprints; one request each.
            if let Some(file_url) = primary_file_url {
                match self.client.get_json(&file_url).await {
                    Ok(file_document) => {
                        if let Some(file) = parse_primary_file(&file_document) {
                            self.apply_primary_file(&mut record, &file);
                        }
                    }
                    Err(error) => {
                        // A broken per-record relationship must not fail the page; the
                        // record simply has no direct PDF link.
                        eprintln!(
                            "osf: primary file resolution failed for {}: {error}",
                            record.source_record_id
                        );
                    }
                }
            }
            records.push(record);
        }
        let next_cursor = if (page as u64).saturating_mul(page_size as u64) < total {
            Some((page + 1).to_string())
        } else {
            None
        };
        Ok(DiscoveryPage {
            records,
            next_cursor,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter() -> OsfAdapter {
        OsfAdapter::new(OsfConfig {
            min_request_interval: Some(Duration::from_millis(0)),
            ..OsfConfig::default()
        })
        .unwrap()
    }

    #[test]
    fn descriptor_declares_pdf_but_no_doi_capabilities() {
        let adapter = adapter();
        let descriptor = adapter.descriptor();
        assert_eq!(descriptor.id, "osf");
        assert_eq!(descriptor.contract_version, SOURCE_ADAPTER_CONTRACT_VERSION);
        assert!(descriptor.supports(Capability::Discovery));
        assert!(!descriptor.supports(Capability::DoiLookup));
        assert!(descriptor.supports(Capability::DirectPdfUrls));
        assert_eq!(
            descriptor.rate_policy.min_request_interval,
            OSF_MIN_REQUEST_INTERVAL
        );
    }

    #[test]
    fn parses_recorded_preprints_list() {
        let adapter = adapter();
        let body: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/osf_preprints.json")).unwrap();
        let data = body["data"].as_array().unwrap();
        let (record, primary_file_url) = adapter.parse_preprint(&data[0]).unwrap();
        assert_eq!(record.source_id, "osf");
        assert_eq!(record.source_record_id, "pybrz_v1");
        assert!(record.title.contains("Middle Way"));
        assert_eq!(
            record.landing_url.as_deref(),
            Some("https://osf.io/preprints/socarxiv/pybrz_v1/")
        );
        assert!(record.publication_year.is_some());
        // The fixture preprint is published and exposes a primary_file relationship.
        assert_eq!(
            primary_file_url.as_deref(),
            Some("https://api.osf.io/v2/files/6abeeb3c4eb5b6d31a39da8a/")
        );
        // No PDF link until the file is resolved.
        assert!(record.oa_pdf_url.is_none());
    }

    #[test]
    fn parses_recorded_primary_file_and_applies_pdf_link() {
        let adapter = adapter();
        let body: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/osf_preprints.json")).unwrap();
        let data = body["data"].as_array().unwrap();
        let (mut record, primary_file_url) = adapter.parse_preprint(&data[0]).unwrap();
        assert_eq!(
            primary_file_url.as_deref(),
            Some("https://api.osf.io/v2/files/6abeeb3c4eb5b6d31a39da8a/")
        );
        let file_document: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/osf_primary_file.json"))
                .unwrap();
        let file = parse_primary_file(&file_document).unwrap();
        assert_eq!(file.guid, "6abeeb3c4eb5b6d31a39da8a");
        assert_eq!(
            file.name,
            "Provete - Middle Way open access - MetaArXiv preprint.pdf"
        );
        assert_eq!(
            file.download_url,
            "https://osf.io/download/6abeeb3c4eb5b6d31a39da8a/"
        );
        adapter.apply_primary_file(&mut record, &file);
        assert_eq!(
            record.oa_pdf_url.as_deref(),
            Some("https://osf.io/download/6abeeb3c4eb5b6d31a39da8a/")
        );
        assert_eq!(
            record.raw["resolved_primary_file"]["name"],
            "Provete - Middle Way open access - MetaArXiv preprint.pdf"
        );
    }

    #[test]
    fn non_pdf_primary_files_never_claim_a_pdf() {
        let adapter = adapter();
        let mut record = WorkRecord::new("osf", "test_v1");
        let file = OsfPrimaryFile {
            guid: "abc".into(),
            name: "presentation.pptx".into(),
            download_url: "https://osf.io/download/abc/".into(),
        };
        adapter.apply_primary_file(&mut record, &file);
        assert!(record.oa_pdf_url.is_none());
    }

    #[test]
    fn unpublished_preprints_are_not_eligible_for_resolution() {
        let adapter = adapter();
        let unpublished = serde_json::json!({
            "id": "draft_v1",
            "type": "preprints",
            "attributes": {
                "title": "Draft",
                "is_published": false,
                "date_created": "2026-01-01T00:00:00.000000",
            },
            "links": {"html": "https://osf.io/preprints/socarxiv/draft_v1/"},
            "relationships": {"primary_file": {"links": {"related": {"href": "https://api.osf.io/v2/files/x/"}}}}
        });
        let (record, primary_file_url) = adapter.parse_preprint(&unpublished).unwrap();
        assert!(primary_file_url.is_none());
        assert!(record.oa_pdf_url.is_none());
    }

    #[test]
    fn malformed_preprints_are_skipped_not_fatal() {
        let adapter = adapter();
        assert!(adapter
            .parse_preprint(&serde_json::json!({ "attributes": {"title": "no id"} }))
            .is_none());
    }

    #[test]
    fn malformed_primary_file_documents_are_rejected() {
        assert!(parse_primary_file(&serde_json::json!({})).is_none());
        assert!(parse_primary_file(&serde_json::json!({ "data": {} })).is_none());
        assert!(parse_primary_file(&serde_json::json!({
            "data": {"id": "g", "attributes": {"name": "n"},
                     "links": {"html": "https://osf.io/x"}}
        }))
        .is_none());
    }
}
