//! Unpaywall adapter (#11 partial).
//!
//! Uses the documented Unpaywall API (`https://api.unpaywall.org/v2/{doi}?email=…`).
//! Access basis: free public API that requires a contact e-mail address as the `email`
//! parameter. Unpaywall resolves where an open-access copy of a Crossref-DOI work can be
//! found; when `best_oa_location.url_for_pdf` is present it is a direct OA PDF link, and
//! when only `url_for_landing_page` exists the source is a repository landing page without
//! a confirmed direct PDF.

use crate::sources::contract::{
    Capability, RatePolicy, SelfTestReport, SourceAdapter, SourceDescriptor, SourceError,
    SOURCE_ADAPTER_CONTRACT_VERSION,
};
use crate::sources::http::{SourceHttpClient, SourceHttpClientOptions};
use crate::sources::normalize::{normalize_doi, normalize_title, normalize_year};
use crate::sources::work_record::{IdentifierKind, WorkRecord};
use async_trait::async_trait;
use std::time::Duration;

pub const UNPAYWALL_DEFAULT_API_BASE: &str = "https://api.unpaywall.org";
pub const UNPAYWALL_MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(1_000);

#[derive(Debug)]
pub struct UnpaywallAdapter {
    descriptor: SourceDescriptor,
    client: SourceHttpClient,
    api_base: String,
    email: String,
}

#[derive(Debug, Clone, Default)]
pub struct UnpaywallConfig {
    /// Required by the Unpaywall API; without it the adapter refuses to run.
    pub contact_email: Option<String>,
    pub api_base: Option<String>,
    pub request_timeout: Option<Duration>,
    pub min_request_interval: Option<Duration>,
}

impl UnpaywallAdapter {
    pub fn new(config: UnpaywallConfig) -> Result<Self, SourceError> {
        let email = config
            .contact_email
            .clone()
            .filter(|email| email.contains('@'))
            .ok_or_else(|| SourceError::InvalidConfiguration {
                detail: "unpaywall requires a contact e-mail (config key `contact_email`)"
                    .to_owned(),
            })?;
        let client = SourceHttpClient::new(SourceHttpClientOptions {
            contact_email: Some(email.clone()),
            request_timeout: config.request_timeout.unwrap_or(Duration::from_secs(30)),
            min_request_interval: config
                .min_request_interval
                .unwrap_or(UNPAYWALL_MIN_REQUEST_INTERVAL),
            accept: None,
        })?;
        Ok(Self {
            descriptor: SourceDescriptor {
                id: "unpaywall",
                display_name: "Unpaywall",
                adapter_version: 1,
                contract_version: SOURCE_ADAPTER_CONTRACT_VERSION,
                api_base: "https://api.unpaywall.org/v2",
                access_basis:
                    "free public API; contact e-mail required; OA resolution for Crossref DOIs",
                capabilities: &[Capability::DoiLookup, Capability::DirectPdfUrls],
                rate_policy: RatePolicy {
                    min_request_interval: UNPAYWALL_MIN_REQUEST_INTERVAL,
                    default_page_size: 1,
                    max_page_size: 1,
                },
            },
            client,
            api_base: config
                .api_base
                .unwrap_or_else(|| UNPAYWALL_DEFAULT_API_BASE.to_owned()),
            email,
        })
    }

    fn parse_response(&self, body: &serde_json::Value) -> Option<WorkRecord> {
        let doi = body
            .get("doi")
            .and_then(|doi| doi.as_str())
            .and_then(normalize_doi)?;
        let mut record = WorkRecord::new("unpaywall", doi.clone());
        record.push_identifier(IdentifierKind::Doi, Some(doi));
        record.title = body
            .get("title")
            .and_then(|title| title.as_str())
            .map(normalize_title)
            .unwrap_or_default();
        if let Some(authors) = body.get("z_authors").and_then(|a| a.as_array()) {
            record.authors = authors
                .iter()
                .filter_map(|author| {
                    author
                        .get("raw_author_name")
                        .or_else(|| author.get("name"))
                        .and_then(|name| name.as_str())
                        .map(normalize_title)
                })
                .collect();
        }
        record.publication_year = body
            .get("year")
            .and_then(|year| year.as_u64())
            .and_then(normalize_year);
        record.venue = body
            .get("journal_name")
            .and_then(|journal| journal.as_str())
            .map(|journal| journal.to_owned());
        let best = body
            .get("best_oa_location")
            .filter(|location| !location.is_null());
        if let Some(best) = best {
            record.oa_pdf_url = best
                .get("url_for_pdf")
                .and_then(|pdf| pdf.as_str())
                .map(|pdf| pdf.to_owned());
            record.license = best
                .get("license")
                .and_then(|license| license.as_str())
                .map(|license| license.to_owned());
            record.landing_url = best
                .get("url_for_landing_page")
                .or_else(|| best.get("url"))
                .and_then(|url| url.as_str())
                .map(|url| url.to_owned());
        }
        record.raw = body.clone();
        Some(record)
    }
}

#[async_trait]
impl SourceAdapter for UnpaywallAdapter {
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
                "doi lookup via /v2/{doi}?email=".to_owned(),
                "oa_pdf_url only when best_oa_location.url_for_pdf is present".to_owned(),
            ],
        }
    }

    async fn lookup_doi(&self, doi: &str) -> Result<Option<WorkRecord>, SourceError> {
        let normalized = normalize_doi(doi).ok_or_else(|| SourceError::InvalidConfiguration {
            detail: format!("not a plausible DOI: {doi}"),
        })?;
        let url = format!("{}/v2/{}?email={}", self.api_base, normalized, self.email);
        let body = self.client.get_json(&url).await;
        let body = match body {
            Ok(body) => body,
            Err(SourceError::NotFound { status: 404 }) => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(self.parse_response(&body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter() -> UnpaywallAdapter {
        UnpaywallAdapter::new(UnpaywallConfig {
            contact_email: Some("researcher@example.org".into()),
            min_request_interval: Some(Duration::from_millis(0)),
            ..UnpaywallConfig::default()
        })
        .unwrap()
    }

    #[test]
    fn missing_email_is_a_typed_configuration_error() {
        let error = UnpaywallAdapter::new(UnpaywallConfig::default()).unwrap_err();
        assert!(matches!(error, SourceError::InvalidConfiguration { .. }));
    }

    #[test]
    fn descriptor_declares_doi_lookup_and_pdf_resolution() {
        let adapter = adapter();
        let descriptor = adapter.descriptor();
        assert_eq!(descriptor.id, "unpaywall");
        assert!(!descriptor.supports(Capability::Discovery));
        assert!(descriptor.supports(Capability::DoiLookup));
        assert!(descriptor.supports(Capability::DirectPdfUrls));
    }

    #[test]
    fn parses_recorded_oa_response_with_direct_pdf() {
        let adapter = adapter();
        let body: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/unpaywall_doi.json")).unwrap();
        let record = adapter.parse_response(&body).unwrap();
        assert_eq!(record.source_id, "unpaywall");
        assert_eq!(record.doi(), Some("10.1371/journal.pone.0000219"));
        assert!(record.title.contains("High-Throughput Sequencing"));
        assert_eq!(record.publication_year, Some(2007));
        assert_eq!(record.license.as_deref(), Some("cc-by"));
        assert_eq!(
            record.oa_pdf_url.as_deref(),
            Some("https://journals.plos.org/plosone/article/file?id=10.1371/journal.pone.0000219&type=printable")
        );
        assert!(record.raw["is_oa"].as_bool().unwrap());
    }

    #[test]
    fn records_without_pdf_location_keep_landing_page_only() {
        let adapter = adapter();
        let body = serde_json::json!({
            "doi": "10.1000/no-pdf",
            "title": "Repository Only",
            "year": 2020,
            "best_oa_location": {
                "url_for_pdf": null,
                "url_for_landing_page": "https://repository.example.org/item/1",
                "license": null
            }
        });
        let record = adapter.parse_response(&body).unwrap();
        assert_eq!(record.oa_pdf_url, None);
        assert_eq!(
            record.landing_url.as_deref(),
            Some("https://repository.example.org/item/1")
        );
    }
}
