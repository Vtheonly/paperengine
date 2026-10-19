//! DOAJ adapter (#12).
//!
//! Uses the documented DOAJ application API (`https://doaj.org/api/search/articles/…`).
//! Access basis: public API, no key. DOAJ indexes open-access journals; each article
//! record carries `bibjson.link` entries deposited by the publisher. The adapter maps
//! a `type=fulltext, content_type=pdf` link to `oa_pdf_url` — a direct, openly
//! accessible publisher PDF for an OA journal, verified live before this mapping was
//! written. HTML-only links become the landing page; a record with no usable fulltext
//! link never claims a PDF.

use crate::sources::contract::{
    Capability, DiscoveryPage, DiscoveryQuery, RatePolicy, SelfTestReport, SourceAdapter,
    SourceDescriptor, SourceError, SOURCE_ADAPTER_CONTRACT_VERSION,
};
use crate::sources::http::{SourceHttpClient, SourceHttpClientOptions};
use crate::sources::normalize::{normalize_doi, normalize_title, normalize_year};
use crate::sources::work_record::{IdentifierKind, WorkRecord};
use async_trait::async_trait;
use std::time::Duration;

pub const DOAJ_DEFAULT_API_BASE: &str = "https://doaj.org/api";
pub const DOAJ_MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(1_000);

pub struct DoajAdapter {
    descriptor: SourceDescriptor,
    client: SourceHttpClient,
    api_base: String,
}

#[derive(Debug, Clone, Default)]
pub struct DoajConfig {
    pub request_timeout: Option<Duration>,
    pub min_request_interval: Option<Duration>,
    /// Override for tests and mirrors; defaults to the public API.
    pub api_base: Option<String>,
}

impl DoajAdapter {
    pub fn new(config: DoajConfig) -> Result<Self, SourceError> {
        let client = SourceHttpClient::new(SourceHttpClientOptions {
            request_timeout: config.request_timeout.unwrap_or(Duration::from_secs(30)),
            min_request_interval: config
                .min_request_interval
                .unwrap_or(DOAJ_MIN_REQUEST_INTERVAL),
            ..SourceHttpClientOptions::default()
        })?;
        Ok(Self {
            descriptor: SourceDescriptor {
                id: "doaj",
                display_name: "DOAJ",
                adapter_version: 1,
                contract_version: SOURCE_ADAPTER_CONTRACT_VERSION,
                api_base: "https://doaj.org/api",
                access_basis: "public application API; publisher-deposited fulltext links",
                capabilities: &[
                    Capability::Discovery,
                    Capability::DoiLookup,
                    Capability::DirectPdfUrls,
                ],
                rate_policy: RatePolicy {
                    min_request_interval: DOAJ_MIN_REQUEST_INTERVAL,
                    default_page_size: 25,
                    max_page_size: 100,
                },
            },
            client,
            api_base: config
                .api_base
                .unwrap_or_else(|| DOAJ_DEFAULT_API_BASE.to_owned()),
        })
    }

    fn search_url(&self, query: &str, page: usize, page_size: u16) -> String {
        format!(
            "{}/search/articles/{}?page={page}&pageSize={page_size}",
            self.api_base,
            urlencode(query)
        )
    }

    fn parse_record(&self, hit: &serde_json::Value) -> Option<WorkRecord> {
        let bibjson = hit.get("bibjson")?;
        let id = hit.get("id")?.as_str()?;
        if id.is_empty() {
            return None;
        }
        let mut record = WorkRecord::new("doaj", id.to_owned());
        record.push_identifier(
            IdentifierKind::Doi,
            bibjson
                .get("identifier")
                .and_then(|ids| ids.as_array())
                .and_then(|ids| {
                    ids.iter().find_map(|entry| {
                        (entry.get("type").and_then(|t| t.as_str()) == Some("doi"))
                            .then(|| entry.get("id").and_then(|id| id.as_str()))
                            .flatten()
                    })
                })
                .and_then(normalize_doi),
        );
        record.title = bibjson
            .get("title")
            .and_then(|t| t.as_str())
            .map(normalize_title)
            .unwrap_or_default();
        if let Some(authors) = bibjson.get("author").and_then(|a| a.as_array()) {
            record.authors = authors
                .iter()
                .filter_map(|author| author.get("name").and_then(|n| n.as_str()))
                .map(normalize_title)
                .collect();
        }
        record.publication_year = bibjson
            .get("year")
            .and_then(|y| y.as_str())
            .and_then(|y| y.trim().get(..4).and_then(|y| y.parse::<u64>().ok()))
            .and_then(normalize_year);
        record.venue = bibjson
            .get("journal")
            .and_then(|journal| journal.get("title"))
            .and_then(|t| t.as_str())
            .map(|t| t.to_owned());
        record.license = bibjson
            .get("journal")
            .and_then(|journal| journal.get("license"))
            .and_then(|licenses| licenses.as_array())
            .and_then(|licenses| licenses.first())
            .and_then(|license| license.get("title"))
            .and_then(|t| t.as_str())
            .map(|t| t.to_owned());
        if let Some(links) = bibjson.get("link").and_then(|l| l.as_array()) {
            for link in links {
                let content_type = link.get("content_type").and_then(|c| c.as_str());
                let link_type = link.get("type").and_then(|t| t.as_str());
                let url = link.get("url").and_then(|u| u.as_str());
                let Some(url) = url else { continue };
                let upgraded = url.replacen("http://", "https://", 1);
                if link_type == Some("fulltext") {
                    match content_type {
                        Some("pdf") => {
                            // Publisher-deposited direct PDF for an OA journal.
                            record.oa_pdf_url = Some(upgraded);
                        }
                        Some("html") => {
                            record.landing_url.get_or_insert(upgraded);
                        }
                        _ => {}
                    }
                }
            }
        }
        // A DOI link is the most stable landing page when present.
        if record.landing_url.is_none() {
            record.landing_url = record.doi().map(|doi| format!("https://doi.org/{doi}"));
        }
        record.raw = hit.clone();
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
impl SourceAdapter for DoajAdapter {
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
                "discovery via /search/articles/<query> (page-based)".to_owned(),
                "doi lookup via doi:<doi> exact search".to_owned(),
                "direct PDF only for publisher-deposited fulltext links with content_type=pdf"
                    .to_owned(),
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
        let url = self.search_url(&query.text, page, page_size);
        let body = self.client.get_json(&url).await?;
        let total = body
            .get("total")
            .and_then(|total| total.as_u64())
            .ok_or_else(|| SourceError::MalformedResponse {
                detail: "missing `total`".to_owned(),
            })?;
        let results = body
            .get("results")
            .and_then(|results| results.as_array())
            .ok_or_else(|| SourceError::MalformedResponse {
                detail: "missing `results` array".to_owned(),
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
        let url = self.search_url(&format!("doi:{normalized}"), 1, 10);
        let body = self.client.get_json(&url).await?;
        let results = body
            .get("results")
            .and_then(|results| results.as_array())
            .ok_or_else(|| SourceError::MalformedResponse {
                detail: "missing `results` array".to_owned(),
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

    fn adapter() -> DoajAdapter {
        DoajAdapter::new(DoajConfig {
            min_request_interval: Some(Duration::from_millis(0)),
            ..DoajConfig::default()
        })
        .unwrap()
    }

    #[test]
    fn descriptor_declares_pdf_capabilities() {
        let adapter = adapter();
        let descriptor = adapter.descriptor();
        assert_eq!(descriptor.id, "doaj");
        assert_eq!(descriptor.contract_version, SOURCE_ADAPTER_CONTRACT_VERSION);
        assert!(descriptor.supports(Capability::Discovery));
        assert!(descriptor.supports(Capability::DoiLookup));
        assert!(descriptor.supports(Capability::DirectPdfUrls));
        assert_eq!(
            descriptor.rate_policy.min_request_interval,
            DOAJ_MIN_REQUEST_INTERVAL
        );
    }

    #[test]
    fn parses_recorded_search_response_with_direct_pdf_link() {
        let adapter = adapter();
        let body: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/doaj_search.json")).unwrap();
        let results = body["results"].as_array().unwrap();
        let record = adapter.parse_record(&results[0]).unwrap();
        assert_eq!(record.source_id, "doaj");
        assert_eq!(record.source_record_id, "00457175120948439080c8679841b806");
        assert_eq!(record.doi(), Some("10.22103/jis.2019.13389.1903"));
        assert_eq!(record.publication_year, Some(2020));
        // The publisher deposited a fulltext link with content_type=pdf.
        assert_eq!(
            record.oa_pdf_url.as_deref(),
            Some("https://jis.uk.ac.ir/article_2666_aef0585c84c617658c4d44807222bfbc.pdf")
        );
        assert!(record.venue.is_some());
        assert!(!record.authors.is_empty());
    }

    #[test]
    fn html_only_fulltext_links_never_claim_a_pdf() {
        let adapter = adapter();
        let html_only = serde_json::json!({
            "id": "deadbeef",
            "bibjson": {
                "title": "HTML only article",
                "year": "2021",
                "identifier": [{"id": "10.1000/htmlonly", "type": "doi"}],
                "link": [
                    {"type": "fulltext", "content_type": "html", "url": "http://example.org/a"}
                ]
            }
        });
        let record = adapter.parse_record(&html_only).unwrap();
        assert!(record.oa_pdf_url.is_none());
        assert_eq!(record.landing_url.as_deref(), Some("https://example.org/a"));
        assert_eq!(record.publication_year, Some(2021));
    }

    #[test]
    fn http_fulltext_links_are_upgraded_to_https() {
        let adapter = adapter();
        let http_link = serde_json::json!({
            "id": "cafe1234",
            "bibjson": {
                "title": "Upgrade me",
                "identifier": [{"id": "10.1000/upgrade", "type": "doi"}],
                "link": [
                    {"type": "fulltext", "content_type": "pdf", "url": "http://example.org/p.pdf"}
                ]
            }
        });
        let record = adapter.parse_record(&http_link).unwrap();
        assert_eq!(
            record.oa_pdf_url.as_deref(),
            Some("https://example.org/p.pdf")
        );
    }

    #[test]
    fn records_without_a_doi_are_still_usable() {
        let adapter = adapter();
        let no_doi = serde_json::json!({
            "id": "face5678",
            "bibjson": {
                "title": "No DOI here",
                "year": "2019",
                "identifier": [{"id": "1234-5678", "type": "pissn"}],
                "journal": {"title": "Some Journal"},
            }
        });
        let record = adapter.parse_record(&no_doi).unwrap();
        assert!(record.doi().is_none());
        assert!(record.landing_url.is_none());
        assert_eq!(record.publication_year, Some(2019));
    }

    #[test]
    fn malformed_records_are_skipped_not_fatal() {
        let adapter = adapter();
        assert!(adapter
            .parse_record(&serde_json::json!({ "bibjson": {"title": "no record id"} }))
            .is_none());
    }

    #[test]
    fn parses_recorded_doi_lookup_response() {
        let adapter = adapter();
        let body: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/doaj_doi.json")).unwrap();
        let results = body["results"].as_array().unwrap();
        assert_eq!(body["total"].as_u64(), Some(1));
        let record = adapter.parse_record(&results[0]).unwrap();
        assert_eq!(record.doi(), Some("10.22103/jis.2019.13389.1903"));
        assert!(record.oa_pdf_url.is_some());
    }
}
