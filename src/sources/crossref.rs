//! Crossref adapter (#11 partial).
//!
//! Uses the documented Crossref REST API (`https://api.crossref.org/works`). Access basis:
//! public API with a polite contact e-mail.
//!
//! Crossref provides rich metadata but generally **no openly accessible full text**: the
//! `link` entries it returns are publisher text-and-data-mining links that usually require a
//! subscription. This adapter therefore declares no direct-PDF capability, records the DOI
//! landing page only, and leaves OA resolution to Unpaywall.

use crate::sources::contract::{
    Capability, DiscoveryPage, DiscoveryQuery, RatePolicy, SelfTestReport, SourceAdapter,
    SourceDescriptor, SourceError, SOURCE_ADAPTER_CONTRACT_VERSION,
};
use crate::sources::http::{SourceHttpClient, SourceHttpClientOptions};
use crate::sources::normalize::{normalize_doi, normalize_title, normalize_year};
use crate::sources::work_record::{IdentifierKind, WorkRecord, WorkTypeFilter};
use async_trait::async_trait;
use std::time::Duration;

pub const CROSSREF_DEFAULT_API_BASE: &str = "https://api.crossref.org";
pub const CROSSREF_MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(1_000);

pub struct CrossrefAdapter {
    descriptor: SourceDescriptor,
    client: SourceHttpClient,
    api_base: String,
    paper_types: WorkTypeFilter,
}

#[derive(Debug, Clone, Default)]
pub struct CrossrefConfig {
    pub contact_email: Option<String>,
    pub api_base: Option<String>,
    pub paper_types: WorkTypeFilter,
    pub request_timeout: Option<Duration>,
    pub min_request_interval: Option<Duration>,
}

impl CrossrefAdapter {
    pub fn new(config: CrossrefConfig) -> Result<Self, SourceError> {
        let client = SourceHttpClient::new(SourceHttpClientOptions {
            contact_email: config.contact_email.clone(),
            request_timeout: config.request_timeout.unwrap_or(Duration::from_secs(30)),
            min_request_interval: config
                .min_request_interval
                .unwrap_or(CROSSREF_MIN_REQUEST_INTERVAL),
            accept: None,
        })?;
        Ok(Self {
            descriptor: SourceDescriptor {
                id: "crossref",
                display_name: "Crossref",
                adapter_version: 1,
                contract_version: SOURCE_ADAPTER_CONTRACT_VERSION,
                api_base: "https://api.crossref.org/works",
                access_basis: "public REST API with polite contact e-mail; metadata only",
                capabilities: &[Capability::Discovery, Capability::DoiLookup],
                rate_policy: RatePolicy {
                    min_request_interval: CROSSREF_MIN_REQUEST_INTERVAL,
                    default_page_size: 20,
                    max_page_size: 100,
                },
            },
            client,
            api_base: config
                .api_base
                .unwrap_or_else(|| CROSSREF_DEFAULT_API_BASE.to_owned()),
            paper_types: config.paper_types,
        })
    }

    fn accepts_type(&self, item_type: Option<&str>) -> bool {
        self.paper_types.accepts(item_type)
    }

    fn parse_item(&self, item: &serde_json::Value) -> Option<WorkRecord> {
        let doi = item
            .get("DOI")
            .and_then(|doi| doi.as_str())
            .and_then(normalize_doi)?;
        let mut record = WorkRecord::new("crossref", doi.clone());
        record
            .identifiers
            .push(crate::sources::work_record::WorkIdentifier::new(
                IdentifierKind::Crossref,
                doi.clone(),
            ));
        record.push_identifier(IdentifierKind::Doi, Some(doi.clone()));
        record.title = item
            .get("title")
            .and_then(|titles| titles.as_array())
            .and_then(|titles| titles.first())
            .and_then(|title| title.as_str())
            .map(normalize_title)
            .unwrap_or_default();
        if let Some(authors) = item.get("author").and_then(|a| a.as_array()) {
            record.authors = authors
                .iter()
                .filter_map(|author| {
                    let given = author.get("given").and_then(|g| g.as_str()).unwrap_or("");
                    let family = author.get("family").and_then(|f| f.as_str()).unwrap_or("");
                    let name = author.get("name").and_then(|n| n.as_str());
                    match name {
                        Some(name) => Some(normalize_title(name)),
                        None if given.is_empty() && family.is_empty() => None,
                        None => Some(normalize_title(&format!("{given} {family}"))),
                    }
                })
                .collect();
        }
        record.publication_year = item
            .get("issued")
            .and_then(|issued| issued.get("date-parts"))
            .and_then(|parts| parts.as_array())
            .and_then(|parts| parts.first())
            .and_then(|first| first.as_array())
            .and_then(|first| first.first())
            .and_then(|year| year.as_u64())
            .and_then(normalize_year);
        record.venue = item
            .get("container-title")
            .and_then(|titles| titles.as_array())
            .and_then(|titles| titles.first())
            .and_then(|title| title.as_str())
            .map(|title| title.to_owned());
        // Crossref `link` entries are publisher TDM links, not open full text. The DOI
        // landing page is the only access route recorded here.
        record.landing_url = Some(format!("https://doi.org/{doi}"));
        record.raw = item.clone();
        Some(record)
    }
}

#[async_trait]
impl SourceAdapter for CrossrefAdapter {
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
                "discovery via /works?query=".to_owned(),
                "metadata only: no direct full-text links are recorded".to_owned(),
                "use the unpaywall adapter to resolve OA locations for a DOI".to_owned(),
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
        let offset: usize = cursor.and_then(|cursor| cursor.parse().ok()).unwrap_or(0);
        let url = format!(
            "{}/works?query={}&rows={page_size}&offset={offset}",
            self.api_base,
            urlencode(&query.text),
        );
        let body = self.client.get_json(&url).await?;
        let items = body
            .get("message")
            .and_then(|message| message.get("items"))
            .and_then(|items| items.as_array())
            .ok_or_else(|| SourceError::MalformedResponse {
                detail: "missing message.items array".to_owned(),
            })?;
        let records = items
            .iter()
            .filter(|item| {
                let item_type = item.get("type").and_then(|t| t.as_str());
                self.accepts_type(item_type)
            })
            .filter_map(|item| self.parse_item(item))
            .collect::<Vec<_>>();
        let next_cursor = if records.len() == page_size as usize {
            Some((offset + records.len()).to_string())
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
        let url = format!("{}/works/{}", self.api_base, urlencode(&normalized));
        let body = self.client.get_json(&url).await;
        let body = match body {
            Ok(body) => body,
            Err(SourceError::NotFound { status: 404 }) => return Ok(None),
            Err(error) => return Err(error),
        };
        let item = body
            .get("message")
            .ok_or_else(|| SourceError::MalformedResponse {
                detail: "missing message object".to_owned(),
            })?;
        Ok(self.parse_item(item))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter() -> CrossrefAdapter {
        CrossrefAdapter::new(CrossrefConfig {
            min_request_interval: Some(Duration::from_millis(0)),
            ..CrossrefConfig::default()
        })
        .unwrap()
    }

    #[test]
    fn descriptor_declares_metadata_only_capabilities() {
        let adapter = adapter();
        let descriptor = adapter.descriptor();
        assert_eq!(descriptor.id, "crossref");
        assert!(descriptor.supports(Capability::Discovery));
        assert!(descriptor.supports(Capability::DoiLookup));
        assert!(
            !descriptor.supports(Capability::DirectPdfUrls),
            "Crossref must not be advertised as a full-text source"
        );
    }

    #[test]
    fn parses_recorded_search_response() {
        let adapter = adapter();
        let body: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/crossref_search.json"))
                .unwrap();
        let items = body["message"]["items"].as_array().unwrap();
        let records: Vec<_> = items.iter().filter_map(|i| adapter.parse_item(i)).collect();
        assert!(!records.is_empty());
        let first = &records[0];
        assert_eq!(first.source_id, "crossref");
        assert_eq!(first.doi(), Some("10.1007/978-3-031-84300-6_13"));
        assert_eq!(first.title, "Is Attention All You Need?");
        assert_eq!(first.publication_year, Some(2025));
        assert!(first
            .authors
            .iter()
            .any(|author| author.contains("Mineault")));
        assert_eq!(
            first.landing_url.as_deref(),
            Some("https://doi.org/10.1007/978-3-031-84300-6_13")
        );
        assert_eq!(
            first.oa_pdf_url, None,
            "publisher TDM links must never be recorded as OA PDFs"
        );
    }

    #[test]
    fn self_test_documents_metadata_only_behavior() {
        let report = adapter().self_test();
        assert!(report
            .notes
            .iter()
            .any(|note| note.contains("metadata only")));
    }
}
