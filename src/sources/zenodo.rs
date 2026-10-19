//! Zenodo adapter (#12).
//!
//! Uses the documented Zenodo records API (`https://zenodo.org/api/records`). Access
//! basis: public REST API for open records; no key required. Every record carries
//! `files[]` with per-file checksums and direct download links
//! (`/api/records/<id>/files/<key>/content`). The adapter maps the first `.pdf` file
//! of a `publication`-type record to `oa_pdf_url` — verified live (the mapped URL
//! returns `application/octet-stream` bytes starting with `%PDF-`). The MD5 checksum
//! Zenodo publishes is retained verbatim in `raw` for provenance.

use crate::sources::contract::{
    Capability, DiscoveryPage, DiscoveryQuery, RatePolicy, SelfTestReport, SourceAdapter,
    SourceDescriptor, SourceError, SOURCE_ADAPTER_CONTRACT_VERSION,
};
use crate::sources::http::{SourceHttpClient, SourceHttpClientOptions};
use crate::sources::normalize::{normalize_doi, normalize_title, normalize_year};
use crate::sources::work_record::{IdentifierKind, WorkRecord};
use async_trait::async_trait;
use std::time::Duration;

pub const ZENODO_DEFAULT_API_BASE: &str = "https://zenodo.org/api";
pub const ZENODO_MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(1_000);

pub struct ZenodoAdapter {
    descriptor: SourceDescriptor,
    client: SourceHttpClient,
    api_base: String,
    publications_only: bool,
}

#[derive(Debug, Clone)]
pub struct ZenodoConfig {
    pub request_timeout: Option<Duration>,
    pub min_request_interval: Option<Duration>,
    /// Override for tests and mirrors; defaults to the public API.
    pub api_base: Option<String>,
    /// Keep only `publication`-typed records (papers, preprints, reports) in the
    /// default set; datasets, software, and images stay out (#10 paper-only default).
    pub publications_only: bool,
}

impl Default for ZenodoConfig {
    fn default() -> Self {
        Self {
            request_timeout: None,
            min_request_interval: None,
            api_base: None,
            publications_only: true,
        }
    }
}

impl ZenodoAdapter {
    pub fn new(config: ZenodoConfig) -> Result<Self, SourceError> {
        let client = SourceHttpClient::new(SourceHttpClientOptions {
            request_timeout: config.request_timeout.unwrap_or(Duration::from_secs(30)),
            min_request_interval: config
                .min_request_interval
                .unwrap_or(ZENODO_MIN_REQUEST_INTERVAL),
            ..SourceHttpClientOptions::default()
        })?;
        Ok(Self {
            descriptor: SourceDescriptor {
                id: "zenodo",
                display_name: "Zenodo",
                adapter_version: 1,
                contract_version: SOURCE_ADAPTER_CONTRACT_VERSION,
                api_base: "https://zenodo.org/api",
                access_basis: "public records REST API; direct file download links",
                capabilities: &[
                    Capability::Discovery,
                    Capability::DoiLookup,
                    Capability::DirectPdfUrls,
                ],
                rate_policy: RatePolicy {
                    min_request_interval: ZENODO_MIN_REQUEST_INTERVAL,
                    default_page_size: 25,
                    max_page_size: 100,
                },
            },
            client,
            api_base: config
                .api_base
                .unwrap_or_else(|| ZENODO_DEFAULT_API_BASE.to_owned()),
            publications_only: config.publications_only,
        })
    }

    fn records_url(&self, query: &str, page: usize, size: u16) -> String {
        format!(
            "{}/records?q={}&size={size}&page={page}",
            self.api_base,
            urlencode(query)
        )
    }

    /// First `.pdf` file of a record, as (url, key) — the direct download endpoint.
    fn first_pdf_file(hit: &serde_json::Value) -> Option<(String, String)> {
        hit.get("files")
            .and_then(|files| files.as_array())
            .and_then(|files| {
                files.iter().find_map(|file| {
                    let key = file.get("key").and_then(|k| k.as_str())?;
                    if !key.to_ascii_lowercase().ends_with(".pdf") {
                        return None;
                    }
                    let url = file
                        .get("links")
                        .and_then(|links| links.get("self"))
                        .and_then(|self_url| self_url.as_str())?;
                    Some((url.replacen("http://", "https://", 1), key.to_owned()))
                })
            })
    }

    fn is_publication(&self, hit: &serde_json::Value) -> bool {
        if !self.publications_only {
            return true;
        }
        hit.get("metadata")
            .and_then(|metadata| metadata.get("resource_type"))
            .and_then(|resource_type| resource_type.get("type"))
            .and_then(|kind| kind.as_str())
            == Some("publication")
    }

    fn parse_record(&self, hit: &serde_json::Value) -> Option<WorkRecord> {
        if !self.is_publication(hit) {
            return None;
        }
        let id = hit.get("id")?;
        let id = id
            .as_u64()
            .map(|id| id.to_string())
            .or_else(|| id.as_str().map(|id| id.to_owned()))?;
        let mut record = WorkRecord::new("zenodo", id);
        record.push_identifier(
            IdentifierKind::Doi,
            hit.get("metadata")
                .and_then(|metadata| metadata.get("doi"))
                .and_then(|doi| doi.as_str())
                .and_then(normalize_doi),
        );
        let metadata = hit.get("metadata")?;
        record.title = metadata
            .get("title")
            .and_then(|t| t.as_str())
            .map(normalize_title)
            .unwrap_or_default();
        if let Some(creators) = metadata.get("creators").and_then(|c| c.as_array()) {
            record.authors = creators
                .iter()
                .filter_map(|creator| creator.get("name").and_then(|n| n.as_str()))
                .map(normalize_title)
                .collect();
        }
        record.publication_year = metadata
            .get("publication_date")
            .and_then(|date| date.as_str())
            .and_then(|date| date.trim().get(..4).and_then(|y| y.parse::<u64>().ok()))
            .and_then(normalize_year);
        record.license = metadata
            .get("license")
            .and_then(|license| license.get("id"))
            .and_then(|id| id.as_str())
            .map(|id| id.to_owned());
        if let Some(journal) = metadata.get("journal").filter(|journal| !journal.is_null()) {
            if let Some(name) = journal.get("name").and_then(|n| n.as_str()) {
                record.venue = Some(name.to_owned());
            }
        }
        if let Some((url, key)) = Self::first_pdf_file(hit) {
            record.oa_pdf_url = Some(url);
            // Retain the matched file and its MD5 provenance in raw alongside the
            // full record; the engine's own integrity hashes are computed at download.
            record.raw = serde_json::json!({
                "record": hit.clone(),
                "acquired_file": key,
            });
        } else {
            record.raw = hit.clone();
        }
        record.landing_url = record
            .doi()
            .map(|doi| format!("https://doi.org/{doi}"))
            .or_else(|| {
                hit.get("links")
                    .and_then(|links| links.get("self_html"))
                    .and_then(|url| url.as_str())
                    .map(|url| url.to_owned())
            });
        Some(record)
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

#[async_trait]
impl SourceAdapter for ZenodoAdapter {
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
                "discovery via /records?q= (page-based)".to_owned(),
                "doi lookup via doi:\"<doi>\" quoted search".to_owned(),
                "direct PDF from the first .pdf file of a publication record".to_owned(),
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
        let url = self.records_url(&query.text, page, page_size);
        let body = self.client.get_json(&url).await?;
        let hits = body
            .get("hits")
            .ok_or_else(|| SourceError::MalformedResponse {
                detail: "missing `hits` object".to_owned(),
            })?;
        let total = hits
            .get("total")
            .and_then(|total| total.as_u64())
            .ok_or_else(|| SourceError::MalformedResponse {
                detail: "missing `hits.total`".to_owned(),
            })?;
        let results = hits
            .get("hits")
            .and_then(|hits| hits.as_array())
            .ok_or_else(|| SourceError::MalformedResponse {
                detail: "missing `hits.hits` array".to_owned(),
            })?;
        let records = results
            .iter()
            .filter_map(|hit| self.parse_record(hit))
            .collect();
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

    async fn lookup_doi(&self, doi: &str) -> Result<Option<WorkRecord>, SourceError> {
        let normalized = normalize_doi(doi).ok_or_else(|| SourceError::InvalidConfiguration {
            detail: format!("not a plausible DOI: {doi}"),
        })?;
        let url = self.records_url(&format!("doi:\"{normalized}\""), 1, 10);
        let body = self.client.get_json(&url).await?;
        let results = body
            .get("hits")
            .and_then(|hits| hits.get("hits"))
            .and_then(|hits| hits.as_array())
            .ok_or_else(|| SourceError::MalformedResponse {
                detail: "missing `hits.hits` array".to_owned(),
            })?;
        for hit in results {
            if let Some(record) = self.parse_record(hit) {
                if record.doi() == Some(normalized.as_str()) {
                    return Ok(Some(record));
                }
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter() -> ZenodoAdapter {
        ZenodoAdapter::new(ZenodoConfig {
            min_request_interval: Some(Duration::from_millis(0)),
            ..ZenodoConfig::default()
        })
        .unwrap()
    }

    #[test]
    fn descriptor_declares_pdf_capabilities() {
        let adapter = adapter();
        let descriptor = adapter.descriptor();
        assert_eq!(descriptor.id, "zenodo");
        assert_eq!(descriptor.contract_version, SOURCE_ADAPTER_CONTRACT_VERSION);
        assert!(descriptor.supports(Capability::Discovery));
        assert!(descriptor.supports(Capability::DoiLookup));
        assert!(descriptor.supports(Capability::DirectPdfUrls));
        assert_eq!(
            descriptor.rate_policy.min_request_interval,
            ZENODO_MIN_REQUEST_INTERVAL
        );
    }

    #[test]
    fn parses_recorded_search_response_with_direct_pdf_link() {
        let adapter = adapter();
        let body: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/zenodo_search.json")).unwrap();
        let hits = body["hits"]["hits"].as_array().unwrap();
        let record = adapter.parse_record(&hits[0]).unwrap();
        assert_eq!(record.source_id, "zenodo");
        assert_eq!(record.source_record_id, "18289747");
        assert_eq!(record.doi(), Some("10.5281/zenodo.18289747"));
        assert_eq!(record.publication_year, Some(2026));
        assert_eq!(record.license.as_deref(), Some("cc-by-4.0"));
        assert_eq!(
            record.oa_pdf_url.as_deref(),
            Some(
                "https://zenodo.org/api/records/18289747/files/2026-doebeli-honegger-attention-is-all-you-need.pdf/content"
            )
        );
        // The matched file name and its MD5 provenance are retained in raw.
        assert_eq!(
            record.raw["acquired_file"],
            "2026-doebeli-honegger-attention-is-all-you-need.pdf"
        );
        assert_eq!(
            record.raw["record"]["files"][0]["checksum"],
            "md5:a2751df6af3f27a9fd373becbd55478c"
        );
        assert_eq!(record.authors, vec!["Döbeli Honegger, Beat"]);
    }

    #[test]
    fn dataset_records_are_filtered_under_the_default_policy() {
        let adapter = adapter();
        let dataset = serde_json::json!({
            "id": 42,
            "metadata": {
                "title": "A dataset",
                "resource_type": {"type": "dataset"},
                "doi": "10.5281/zenodo.42",
            },
            "files": [
                {"key": "data.pdf", "checksum": "md5:x",
                 "links": {"self": "https://zenodo.org/api/records/42/files/data.pdf/content"}}
            ]
        });
        assert!(adapter.parse_record(&dataset).is_none());

        let permissive = ZenodoAdapter::new(ZenodoConfig {
            publications_only: false,
            min_request_interval: Some(Duration::from_millis(0)),
            ..ZenodoConfig::default()
        })
        .unwrap();
        let record = permissive.parse_record(&dataset).unwrap();
        assert!(record.oa_pdf_url.is_some());
    }

    #[test]
    fn records_without_a_pdf_file_never_claim_one() {
        let adapter = adapter();
        let zip_only = serde_json::json!({
            "id": 77,
            "metadata": {
                "title": "Data package",
                "resource_type": {"type": "publication"},
                "doi": "10.5281/zenodo.77",
                "publication_date": "2026-01-01",
            },
            "files": [
                {"key": "bundle.zip", "checksum": "md5:y",
                 "links": {"self": "https://zenodo.org/api/records/77/files/bundle.zip/content"}}
            ]
        });
        let record = adapter.parse_record(&zip_only).unwrap();
        assert!(record.oa_pdf_url.is_none());
        assert_eq!(record.raw["files"][0]["key"], "bundle.zip");
        assert_eq!(record.raw.get("acquired_file"), None);
    }

    #[test]
    fn parses_recorded_doi_lookup_response() {
        let adapter = adapter();
        let body: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/zenodo_doi.json")).unwrap();
        let hits = body["hits"]["hits"].as_array().unwrap();
        assert_eq!(body["hits"]["total"].as_u64(), Some(1));
        let record = adapter.parse_record(&hits[0]).unwrap();
        assert_eq!(record.doi(), Some("10.5281/zenodo.18289747"));
        assert!(record.oa_pdf_url.is_some());
    }

    #[test]
    fn malformed_records_are_skipped_not_fatal() {
        let adapter = adapter();
        assert!(adapter
            .parse_record(&serde_json::json!({ "metadata": {"title": "no id"} }))
            .is_none());
    }
}
