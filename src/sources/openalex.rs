//! OpenAlex adapter (#11 partial).
//!
//! Uses the documented OpenAlex REST API (`https://api.openalex.org/works`). Access basis:
//! public API with a polite contact e-mail; since 2026 OpenAlex also offers per-key budgets,
//! so an optional API key is supported through configuration. 403/429 responses are surfaced
//! as typed stop conditions and are never retried past refusal.
//!
//! OpenAlex reports `best_oa_location.pdf_url` for openly accessible works. That field is
//! `null` for works without a known direct OA PDF; the adapter then leaves `oa_pdf_url`
//! empty rather than guessing. The default work-type filter keeps paper-like records.

use crate::sources::contract::{
    Capability, DiscoveryPage, DiscoveryQuery, RatePolicy, SelfTestReport, SourceAdapter,
    SourceDescriptor, SourceError, SOURCE_ADAPTER_CONTRACT_VERSION,
};
use crate::sources::http::{SourceHttpClient, SourceHttpClientOptions};
use crate::sources::normalize::{
    normalize_doi, normalize_openalex_id, normalize_title, normalize_year,
};
use crate::sources::work_record::{
    IdentifierKind, WorkRecord, WorkTypeFilter, PAPER_LIKE_WORK_TYPES,
};
use async_trait::async_trait;
use std::time::Duration;

pub const OPENALEX_DEFAULT_API_BASE: &str = "https://api.openalex.org";
pub const OPENALEX_MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(1_000);

/// Work types treated as paper-like by default (#10 partial). Books, datasets, and
/// paratext are excluded unless the caller opts in with `types: None`.
pub struct OpenAlexAdapter {
    descriptor: SourceDescriptor,
    client: SourceHttpClient,
    api_base: String,
    api_key: Option<String>,
    paper_types: WorkTypeFilter,
}

#[derive(Debug, Clone, Default)]
pub struct OpenAlexConfig {
    pub contact_email: Option<String>,
    pub api_key: Option<String>,
    /// Override for tests and mirrors; defaults to the public API.
    pub api_base: Option<String>,
    /// Work-type filter; defaults to paper-only.
    pub paper_types: WorkTypeFilter,
    pub request_timeout: Option<Duration>,
    pub min_request_interval: Option<Duration>,
}

impl OpenAlexAdapter {
    pub fn new(config: OpenAlexConfig) -> Result<Self, SourceError> {
        let client = SourceHttpClient::new(SourceHttpClientOptions {
            contact_email: config.contact_email.clone(),
            request_timeout: config.request_timeout.unwrap_or(Duration::from_secs(30)),
            min_request_interval: config
                .min_request_interval
                .unwrap_or(OPENALEX_MIN_REQUEST_INTERVAL),
            accept: None,
        })?;
        Ok(Self {
            descriptor: SourceDescriptor {
                id: "openalex",
                display_name: "OpenAlex",
                adapter_version: 1,
                contract_version: SOURCE_ADAPTER_CONTRACT_VERSION,
                api_base: "https://api.openalex.org",
                access_basis: "public REST API; polite contact e-mail; optional free API key",
                capabilities: &[Capability::Discovery, Capability::DoiLookup],
                rate_policy: RatePolicy {
                    min_request_interval: OPENALEX_MIN_REQUEST_INTERVAL,
                    default_page_size: 25,
                    max_page_size: 200,
                },
            },
            client,
            api_base: config
                .api_base
                .unwrap_or_else(|| OPENALEX_DEFAULT_API_BASE.to_owned()),
            api_key: config.api_key,
            paper_types: config.paper_types,
        })
    }

    fn works_url(&self, params: &[(&str, String)]) -> String {
        let mut url = format!("{}/works", self.api_base);
        let mut first = true;
        let mut push = |url: &mut String, key: &str, value: &str| {
            url.push(if first { '?' } else { '&' });
            first = false;
            url.push_str(key);
            url.push('=');
            url.push_str(&urlencode(value));
        };
        for (key, value) in params {
            push(&mut url, key, value);
        }
        if let Some(key) = self.api_key.as_deref() {
            push(&mut url, "api_key", key);
        }
        url
    }

    fn accepts_type(&self, work_type: Option<&str>) -> bool {
        self.paper_types.accepts(work_type)
    }

    fn parse_work(&self, work: &serde_json::Value) -> Option<WorkRecord> {
        let work_type = work.get("type").and_then(|t| t.as_str());
        if !self.accepts_type(work_type) {
            return None;
        }
        let id = normalize_openalex_id(work.get("id")?.as_str()?)?;
        let mut record = WorkRecord::new("openalex", id.clone());
        record
            .identifiers
            .push(crate::sources::work_record::WorkIdentifier::new(
                IdentifierKind::Openalex,
                id,
            ));
        record.push_identifier(
            IdentifierKind::Doi,
            work.get("doi")
                .and_then(|doi| doi.as_str())
                .and_then(normalize_doi),
        );
        let title = work
            .get("display_name")
            .or_else(|| work.get("title"))
            .and_then(|title| title.as_str())
            .map(normalize_title)
            .unwrap_or_default();
        record.title = title;
        if let Some(authors) = work.get("authorships").and_then(|a| a.as_array()) {
            record.authors = authors
                .iter()
                .filter_map(|authorship| {
                    authorship
                        .get("author")
                        .and_then(|author| author.get("display_name"))
                        .and_then(|name| name.as_str())
                        .map(normalize_title)
                })
                .collect();
        }
        record.publication_year = work
            .get("publication_year")
            .and_then(|year| year.as_u64())
            .and_then(normalize_year);
        record.venue = work
            .get("primary_location")
            .and_then(|location| location.get("source"))
            .and_then(|source| source.get("display_name"))
            .and_then(|name| name.as_str())
            .map(|name| name.to_owned());
        let best_oa = work
            .get("best_oa_location")
            .filter(|location| !location.is_null());
        if let Some(best_oa) = best_oa {
            record.oa_pdf_url = best_oa
                .get("pdf_url")
                .and_then(|pdf| pdf.as_str())
                .map(|pdf| pdf.to_owned());
            record.license = best_oa
                .get("license")
                .and_then(|license| license.as_str())
                .map(|license| license.to_owned());
            if record.landing_url.is_none() {
                record.landing_url = best_oa
                    .get("landing_page_url")
                    .and_then(|url| url.as_str())
                    .map(|url| url.to_owned());
            }
        }
        if record.landing_url.is_none() {
            record.landing_url = work
                .get("primary_location")
                .and_then(|location| location.get("landing_page_url"))
                .and_then(|url| url.as_str())
                .map(|url| url.to_owned());
        }
        record.raw = work.clone();
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
impl SourceAdapter for OpenAlexAdapter {
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
                "discovery via /works?search= with cursor pagination".to_owned(),
                "doi lookup via /works/doi:<doi>".to_owned(),
                format!(
                    "paper-type filter: {}",
                    match &self.paper_types {
                        WorkTypeFilter::All => "disabled (all types accepted)".to_owned(),
                        WorkTypeFilter::Only(types) => format!("only: {}", types.join(",")),
                        WorkTypeFilter::PaperOnly => {
                            format!("paper-only: {}", PAPER_LIKE_WORK_TYPES.join(","))
                        }
                    }
                ),
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
        let cursor = cursor.unwrap_or("*");
        let url = self.works_url(&[
            ("search", query.text.clone()),
            ("per-page", page_size.to_string()),
            ("cursor", cursor.to_owned()),
        ]);
        let body = self.client.get_json(&url).await?;
        let results = body
            .get("results")
            .and_then(|results| results.as_array())
            .ok_or_else(|| SourceError::MalformedResponse {
                detail: "missing `results` array".to_owned(),
            })?;
        let next_cursor = body
            .get("meta")
            .and_then(|meta| meta.get("next_cursor"))
            .and_then(|next| next.as_str())
            .map(|next| next.to_owned());
        let records = results
            .iter()
            .filter_map(|work| self.parse_work(work))
            .collect();
        Ok(DiscoveryPage {
            records,
            next_cursor,
        })
    }

    async fn lookup_doi(&self, doi: &str) -> Result<Option<WorkRecord>, SourceError> {
        let normalized = normalize_doi(doi).ok_or_else(|| SourceError::InvalidConfiguration {
            detail: format!("not a plausible DOI: {doi}"),
        })?;
        let url = format!("{}/works/doi:{}", self.api_base, urlencode(&normalized));
        let body = self.client.get_json(&url).await;
        let body = match body {
            Ok(body) => body,
            Err(SourceError::NotFound { status: 404 }) => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(self.parse_work(&body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter() -> OpenAlexAdapter {
        OpenAlexAdapter::new(OpenAlexConfig {
            min_request_interval: Some(Duration::from_millis(0)),
            ..OpenAlexConfig::default()
        })
        .unwrap()
    }

    #[test]
    fn descriptor_declares_capabilities_and_rate_policy() {
        let adapter = adapter();
        let descriptor = adapter.descriptor();
        assert_eq!(descriptor.id, "openalex");
        assert_eq!(descriptor.contract_version, SOURCE_ADAPTER_CONTRACT_VERSION);
        assert!(descriptor.supports(Capability::Discovery));
        assert!(descriptor.supports(Capability::DoiLookup));
        assert!(!descriptor.supports(Capability::DirectPdfUrls));
        assert_eq!(
            descriptor.rate_policy.min_request_interval,
            OPENALEX_MIN_REQUEST_INTERVAL
        );
    }

    #[test]
    fn parses_recorded_search_response() {
        let adapter = adapter();
        let body: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/openalex_search.json"))
                .unwrap();
        let results = body["results"].as_array().unwrap();
        let record = adapter.parse_work(&results[0]).unwrap();
        assert_eq!(record.source_id, "openalex");
        assert_eq!(record.source_record_id, "W2741809807");
        assert_eq!(record.doi(), Some("10.7717/peerj.4375"));
        assert!(record.title.contains("The state of OA"));
        assert_eq!(record.publication_year, Some(2018));
        assert!(!record.authors.is_empty());
        assert!(record.raw["id"].is_string());
    }

    #[test]
    fn parses_recorded_single_work_lookup() {
        let adapter = adapter();
        let body: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/openalex_work.json")).unwrap();
        let record = adapter.parse_work(&body).unwrap();
        assert_eq!(record.source_record_id, "W2741809807");
        assert_eq!(record.venue.as_deref(), Some("PeerJ"));
        assert_eq!(record.publication_year, Some(2018));
        // The recorded work has best_oa_location with pdf_url: null — the adapter must
        // leave oa_pdf_url unset rather than inventing one.
        assert_eq!(record.oa_pdf_url, None);
        assert!(record.landing_url.is_some());
    }

    #[test]
    fn extracts_pdf_url_from_schema_accurate_fixture() {
        let adapter = adapter();
        let body: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/openalex_search_synthetic.json"
        ))
        .unwrap();
        let results = body["results"].as_array().unwrap();
        let with_pdf = adapter.parse_work(&results[0]).unwrap();
        assert_eq!(
            with_pdf.oa_pdf_url.as_deref(),
            Some("https://arxiv.org/pdf/1706.03762")
        );
        // A `book` type is filtered out by the default paper-only set.
        assert!(adapter.parse_work(&results[1]).is_none());
    }

    #[test]
    fn paper_type_filter_can_be_disabled() {
        let adapter = OpenAlexAdapter::new(OpenAlexConfig {
            paper_types: WorkTypeFilter::All,
            min_request_interval: Some(Duration::from_millis(0)),
            ..OpenAlexConfig::default()
        })
        .unwrap();
        let body: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/openalex_search_synthetic.json"
        ))
        .unwrap();
        let results = body["results"].as_array().unwrap();
        assert!(adapter.parse_work(&results[1]).is_some());
    }

    #[test]
    fn malformed_works_are_skipped_not_fatal() {
        let adapter = adapter();
        assert!(adapter
            .parse_work(&serde_json::json!({ "display_name": "no id" }))
            .is_none());
    }

    #[test]
    fn self_test_is_offline_and_reports_configuration() {
        let report = adapter().self_test();
        assert_eq!(report.source_id, "openalex");
        assert!(report.configured);
        assert!(report.capabilities.contains(&"discovery".to_owned()));
    }
}
