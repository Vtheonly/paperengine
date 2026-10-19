//! Europe PMC adapter (#12).
//!
//! Uses the documented Europe PMC REST search API
//! (`https://www.ebi.ac.uk/europepmc/webservices/rest/search`). Access basis: public
//! REST API; no key or account required. The API's documented full-text form for open
//! access articles is JATS XML (`/rest/{PMCID}/fullTextXML`), which is retained for a
//! future extraction slice. Europe PMC does **not** expose a documented public PDF
//! download endpoint: the site's `ptpmcrender.fcgi` render link is undocumented and
//! answers 403 to API clients, so this adapter never claims a direct PDF link.
//! `hasPDF`/`inEPMC` availability flags are preserved verbatim in `raw` instead.

use crate::sources::contract::{
    Capability, DiscoveryPage, DiscoveryQuery, RatePolicy, SelfTestReport, SourceAdapter,
    SourceDescriptor, SourceError, SOURCE_ADAPTER_CONTRACT_VERSION,
};
use crate::sources::http::{SourceHttpClient, SourceHttpClientOptions};
use crate::sources::normalize::{
    normalize_doi, normalize_pmcid, normalize_pmid, normalize_title, normalize_year,
};
use crate::sources::work_record::{IdentifierKind, WorkRecord};
use async_trait::async_trait;
use std::time::Duration;

pub const EUROPEPMC_DEFAULT_API_BASE: &str = "https://www.ebi.ac.uk/europepmc/webservices/rest";
pub const EUROPEPMC_MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(1_000);

pub struct EuropePmcAdapter {
    descriptor: SourceDescriptor,
    client: SourceHttpClient,
    api_base: String,
    paper_only: bool,
}

#[derive(Debug, Clone)]
pub struct EuropePmcConfig {
    pub request_timeout: Option<Duration>,
    pub min_request_interval: Option<Duration>,
    /// Override for tests and mirrors; defaults to the public API.
    pub api_base: Option<String>,
    /// Keep only paper-like records (articles and preprints); default true (#10).
    pub paper_only: bool,
}

impl Default for EuropePmcConfig {
    fn default() -> Self {
        Self {
            request_timeout: None,
            min_request_interval: None,
            api_base: None,
            paper_only: true,
        }
    }
}

impl EuropePmcAdapter {
    pub fn new(config: EuropePmcConfig) -> Result<Self, SourceError> {
        let client = SourceHttpClient::new(SourceHttpClientOptions {
            request_timeout: config.request_timeout.unwrap_or(Duration::from_secs(30)),
            min_request_interval: config
                .min_request_interval
                .unwrap_or(EUROPEPMC_MIN_REQUEST_INTERVAL),
            ..SourceHttpClientOptions::default()
        })?;
        Ok(Self {
            descriptor: SourceDescriptor {
                id: "europepmc",
                display_name: "Europe PMC",
                adapter_version: 1,
                contract_version: SOURCE_ADAPTER_CONTRACT_VERSION,
                api_base: "https://www.ebi.ac.uk/europepmc/webservices/rest",
                access_basis: "public REST search API; JATS full text for OA records; \
                               no documented public PDF endpoint",
                capabilities: &[Capability::Discovery, Capability::DoiLookup],
                rate_policy: RatePolicy {
                    min_request_interval: EUROPEPMC_MIN_REQUEST_INTERVAL,
                    default_page_size: 25,
                    max_page_size: 100,
                },
            },
            client,
            api_base: config
                .api_base
                .unwrap_or_else(|| EUROPEPMC_DEFAULT_API_BASE.to_owned()),
            paper_only: config.paper_only,
        })
    }

    fn search_url(&self, query: &str, page: usize, page_size: u16) -> String {
        format!(
            "{}/search?query={}&format=json&pageSize={}&page={}",
            self.api_base,
            urlencode(query),
            page_size,
            page
        )
    }

    /// Paper-like under the default filter: journal/research articles, or preprints
    /// (Europe PMC source `PPR`). Reviews and books without an article token are kept
    /// out of the default set; callers can widen with `paper_only: false`.
    fn accepts_type(&self, record: &serde_json::Value) -> bool {
        if !self.paper_only {
            return true;
        }
        if record.get("source").and_then(|s| s.as_str()) == Some("PPR") {
            return true;
        }
        record
            .get("pubType")
            .and_then(|t| t.as_str())
            .map(|types| types.to_ascii_lowercase().contains("article"))
            .unwrap_or(false)
    }

    fn parse_record(&self, hit: &serde_json::Value) -> Option<WorkRecord> {
        if !self.accepts_type(hit) {
            return None;
        }
        let source_id = hit.get("id")?.as_str()?;
        let mut record = WorkRecord::new("europepmc", source_id.to_owned());
        record.push_identifier(
            IdentifierKind::Doi,
            hit.get("doi")
                .and_then(|d| d.as_str())
                .and_then(normalize_doi),
        );
        record.push_identifier(
            IdentifierKind::Pmid,
            hit.get("pmid")
                .and_then(|p| p.as_str())
                .and_then(normalize_pmid),
        );
        record.push_identifier(
            IdentifierKind::Pmc,
            hit.get("pmcid")
                .and_then(|p| p.as_str())
                .and_then(normalize_pmcid),
        );
        record.title = hit
            .get("title")
            .and_then(|t| t.as_str())
            .map(normalize_title)
            .unwrap_or_default();
        if let Some(authors) = hit.get("authorString").and_then(|a| a.as_str()) {
            record.authors = authors
                .split(',')
                .filter_map(|author| {
                    let author = author.trim_end_matches('.').trim();
                    (!author.is_empty()).then(|| normalize_title(author))
                })
                .collect();
        }
        record.publication_year = hit
            .get("pubYear")
            .and_then(|y| y.as_str())
            .and_then(|y| y.trim().get(..4).and_then(|y| y.parse::<u64>().ok()))
            .and_then(normalize_year);
        record.venue = hit
            .get("journalTitle")
            .and_then(|j| j.as_str())
            .map(|j| j.to_owned());
        // Europe PMC exposes no documented public PDF endpoint; the adapter must not
        // invent one from availability flags. oa_pdf_url stays unset.
        record.landing_url = record.doi().map(|doi| format!("https://doi.org/{doi}"));
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
impl SourceAdapter for EuropePmcAdapter {
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
                "discovery via /search (JSON, page-based)".to_owned(),
                "doi lookup via DOI:\"…\" fielded search".to_owned(),
                "full text available as JATS XML only; no direct PDF links".to_owned(),
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
        let hit_count = body
            .get("hitCount")
            .and_then(|count| count.as_u64())
            .ok_or_else(|| SourceError::MalformedResponse {
                detail: "missing `hitCount`".to_owned(),
            })?;
        let results = body
            .get("resultList")
            .and_then(|list| list.get("result"))
            .and_then(|result| result.as_array())
            .ok_or_else(|| SourceError::MalformedResponse {
                detail: "missing `resultList.result` array".to_owned(),
            })?;
        let records = results
            .iter()
            .filter_map(|hit| self.parse_record(hit))
            .collect();
        let next_cursor = if (page as u64).saturating_mul(page_size as u64) < hit_count {
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
        // Documented fielded search; quotes keep the match exact.
        let url = self.search_url(&format!("DOI:\"{normalized}\""), 1, 10);
        let body = self.client.get_json(&url).await?;
        let results = body
            .get("resultList")
            .and_then(|list| list.get("result"))
            .and_then(|result| result.as_array())
            .ok_or_else(|| SourceError::MalformedResponse {
                detail: "missing `resultList.result` array".to_owned(),
            })?;
        for hit in results {
            let hit_doi = hit
                .get("doi")
                .and_then(|d| d.as_str())
                .and_then(normalize_doi);
            if hit_doi.as_deref() == Some(normalized.as_str()) {
                return Ok(self.parse_record(hit));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter() -> EuropePmcAdapter {
        EuropePmcAdapter::new(EuropePmcConfig {
            min_request_interval: Some(Duration::from_millis(0)),
            ..EuropePmcConfig::default()
        })
        .unwrap()
    }

    #[test]
    fn descriptor_declares_metadata_only_capabilities() {
        let adapter = adapter();
        let descriptor = adapter.descriptor();
        assert_eq!(descriptor.id, "europepmc");
        assert_eq!(descriptor.contract_version, SOURCE_ADAPTER_CONTRACT_VERSION);
        assert!(descriptor.supports(Capability::Discovery));
        assert!(descriptor.supports(Capability::DoiLookup));
        assert!(!descriptor.supports(Capability::DirectPdfUrls));
        assert_eq!(
            descriptor.rate_policy.min_request_interval,
            EUROPEPMC_MIN_REQUEST_INTERVAL
        );
    }

    #[test]
    fn parses_recorded_search_response() {
        let adapter = adapter();
        let body: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/europepmc_search.json"))
                .unwrap();
        let results = body["resultList"]["result"].as_array().unwrap();
        let record = adapter.parse_record(&results[0]).unwrap();
        assert_eq!(record.source_id, "europepmc");
        assert_eq!(record.source_record_id, "42441585");
        assert_eq!(record.doi(), Some("10.1371/journal.pone.0353788"));
        assert_eq!(record.identifier(IdentifierKind::Pmc), Some("PMC13362148"));
        assert_eq!(record.identifier(IdentifierKind::Pmid), Some("42441585"));
        assert_eq!(record.publication_year, Some(2026));
        assert_eq!(record.venue.as_deref(), Some("PLoS One"));
        assert!(!record.authors.is_empty());
        assert!(record.title.contains("edgeR"));
    }

    #[test]
    fn records_with_full_text_availability_still_never_claim_a_pdf() {
        // The fixture record has inEPMC:Y and hasPDF:Y. Europe PMC still exposes no
        // documented public PDF endpoint, so the adapter must leave oa_pdf_url unset
        // rather than inventing a render URL.
        let adapter = adapter();
        let body: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/europepmc_search.json"))
                .unwrap();
        let results = body["resultList"]["result"].as_array().unwrap();
        let record = adapter.parse_record(&results[0]).unwrap();
        assert!(record.raw["hasPDF"] == "Y");
        assert!(record.raw["inEPMC"] == "Y");
        assert!(record.oa_pdf_url.is_none());
        assert_eq!(
            record.landing_url.as_deref(),
            Some("https://doi.org/10.1371/journal.pone.0353788")
        );
    }

    #[test]
    fn preprint_records_pass_the_paper_filter() {
        let adapter = adapter();
        let preprint = serde_json::json!({
            "id": "PPR1328024",
            "source": "PPR",
            "doi": "10.64898/2026.09.20.26363284",
            "title": "A preprint",
            "pubYear": "2026",
        });
        let record = adapter.parse_record(&preprint).unwrap();
        assert_eq!(record.source_record_id, "PPR1328024");
        assert_eq!(record.identifier(IdentifierKind::Pmc), None);
        assert_eq!(record.publication_year, Some(2026));
    }

    #[test]
    fn non_article_records_are_filtered_under_the_default_policy() {
        let adapter = adapter();
        let book_review = serde_json::json!({
            "id": "12345",
            "source": "MED",
            "pubType": "book review",
            "title": "A book review",
        });
        assert!(adapter.parse_record(&book_review).is_none());

        let permissive = EuropePmcAdapter::new(EuropePmcConfig {
            paper_only: false,
            min_request_interval: Some(Duration::from_millis(0)),
            ..EuropePmcConfig::default()
        })
        .unwrap();
        assert!(permissive.parse_record(&book_review).is_some());
    }

    #[test]
    fn malformed_records_are_skipped_not_fatal() {
        let adapter = adapter();
        assert!(adapter
            .parse_record(&serde_json::json!({ "title": "no id" }))
            .is_none());
    }

    #[test]
    fn search_url_is_well_formed() {
        let adapter = adapter();
        assert_eq!(
            adapter.search_url("DESeq2", 3, 25),
            format!(
                "{}/search?query=DESeq2&format=json&pageSize=25&page=3",
                EUROPEPMC_DEFAULT_API_BASE
            )
        );
    }
}
