//! PubMed Central adapter (#12).
//!
//! Uses NCBI E-Utilities (`esearch` + `esummary` against `db=pmc`), the documented
//! programmatic interface to PubMed Central. Access basis: public E-utilities with
//! modest request spacing (NCBI documents 3 requests/second without a key; this
//! adapter uses 1 second). Discovery costs two requests per page (search then
//! summary), which the rate policy spaces.
//!
//! PMC's official bulk/full-text route is the OA utility
//! (`https://www.ncbi.nlm.nih.gov/pmc/utils/oa/oa.fcgi`), which serves `.tar.gz`
//! article packages. That endpoint answered HTTP 404 from this environment when
//! probed (2026-10-19), so full-text acquisition through PMC is recorded as blocked
//! in the coverage registry, and this adapter never claims a PDF link. Metadata
//! discovery and DOI lookup (`term=<doi>[PID]`, exact-match verified) work.

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
use quick_xml::events::Event;
use quick_xml::Reader;
use std::time::Duration;

pub const PMC_DEFAULT_API_BASE: &str = "https://eutils.ncbi.nlm.nih.gov/entrez/eutils";
pub const PMC_MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(1_000);

pub struct PmcAdapter {
    descriptor: SourceDescriptor,
    client: SourceHttpClient,
    api_base: String,
}

#[derive(Debug, Clone, Default)]
pub struct PmcConfig {
    pub request_timeout: Option<Duration>,
    pub min_request_interval: Option<Duration>,
    /// Override for tests and mirrors; defaults to the public E-utilities.
    pub api_base: Option<String>,
}

/// Result of parsing one `esearch` response: total count and the accession list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EsearchSummary {
    pub count: u64,
    pub ids: Vec<String>,
}

impl PmcAdapter {
    pub fn new(config: PmcConfig) -> Result<Self, SourceError> {
        let client = SourceHttpClient::new(SourceHttpClientOptions {
            request_timeout: config.request_timeout.unwrap_or(Duration::from_secs(30)),
            min_request_interval: config
                .min_request_interval
                .unwrap_or(PMC_MIN_REQUEST_INTERVAL),
            ..SourceHttpClientOptions::default()
        })?;
        Ok(Self {
            descriptor: SourceDescriptor {
                id: "pmc",
                display_name: "PubMed Central",
                adapter_version: 1,
                contract_version: SOURCE_ADAPTER_CONTRACT_VERSION,
                api_base: "https://eutils.ncbi.nlm.nih.gov/entrez/eutils",
                access_basis: "public E-utilities (esearch/esummary, db=pmc); OA bulk \
                               utility currently blocked (404); no direct PDF links",
                capabilities: &[Capability::Discovery, Capability::DoiLookup],
                rate_policy: RatePolicy {
                    min_request_interval: PMC_MIN_REQUEST_INTERVAL,
                    default_page_size: 25,
                    max_page_size: 100,
                },
            },
            client,
            api_base: config
                .api_base
                .unwrap_or_else(|| PMC_DEFAULT_API_BASE.to_owned()),
        })
    }

    fn esearch_url(&self, term: &str, retmax: u16, retstart: usize) -> String {
        format!(
            "{}/esearch.fcgi?db=pmc&retmax={retmax}&retstart={retstart}&term={}",
            self.api_base,
            urlencode(term)
        )
    }

    fn esummary_url(&self, ids: &[String]) -> String {
        format!(
            "{}/esummary.fcgi?db=pmc&retmode=json&id={}",
            self.api_base,
            ids.join(",")
        )
    }

    fn parse_summary(&self, summary: &serde_json::Value) -> Option<WorkRecord> {
        let article_ids = summary.get("articleids").and_then(|a| a.as_array())?;
        let field = |idtype: &str| -> Option<String> {
            article_ids
                .iter()
                .find(|entry| entry.get("idtype").and_then(|t| t.as_str()) == Some(idtype))
                .and_then(|entry| entry.get("value").and_then(|v| v.as_str()))
                .map(|value| value.to_owned())
        };
        let pmcid = field("pmcid").and_then(|value| normalize_pmcid(&value))?;
        let mut record = WorkRecord::new("pmc", pmcid.clone());
        record.push_identifier(IdentifierKind::Pmcid, Some(pmcid.clone()));
        record.push_identifier(
            IdentifierKind::Doi,
            field("doi").as_deref().and_then(normalize_doi),
        );
        record.push_identifier(
            IdentifierKind::Pmid,
            field("pmid").as_deref().and_then(normalize_pmid),
        );
        record.title = summary
            .get("title")
            .and_then(|t| t.as_str())
            .map(normalize_title)
            .unwrap_or_default();
        if let Some(authors) = summary.get("authors").and_then(|a| a.as_array()) {
            record.authors = authors
                .iter()
                .filter_map(|author| author.get("name").and_then(|n| n.as_str()))
                .map(normalize_title)
                .collect();
        }
        record.publication_year = summary
            .get("pubdate")
            .and_then(|date| date.as_str())
            .and_then(|date| date.trim().get(..4).and_then(|y| y.parse::<u64>().ok()))
            .and_then(normalize_year);
        record.venue = summary
            .get("fulljournalname")
            .or_else(|| summary.get("source"))
            .and_then(|j| j.as_str())
            .map(|j| j.to_owned());
        record.license = summary
            .get("license")
            .and_then(|license| license.as_object())
            .and_then(|license| {
                license
                    .get("type")
                    .or_else(|| license.get("title"))
                    .and_then(|t| t.as_str())
                    .map(|t| t.to_owned())
            });
        // No documented direct PDF route for db=pmc; oa.fcgi is blocked. Never claim one.
        record.landing_url = record.doi().map(|doi| format!("https://doi.org/{doi}"));
        record.raw = summary.clone();
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

/// Parse an `esearch` XML document: `<Count>` and the `<Id>` list.
pub fn parse_esearch(document: &str) -> Result<EsearchSummary, SourceError> {
    let mut reader = Reader::from_str(document);
    let mut count: Option<u64> = None;
    let mut ids = Vec::new();
    let mut in_idlist = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(start)) => match start.name().as_ref() {
                b"IdList" => in_idlist = true,
                b"Count" => {
                    let text = read_text(&mut reader)?;
                    count = text.trim().parse().ok();
                }
                b"Id" if in_idlist => {
                    let text = read_text(&mut reader)?;
                    if !text.trim().is_empty() {
                        ids.push(text.trim().to_owned());
                    }
                }
                _ => {}
            },
            Ok(Event::End(end)) if end.name().as_ref() == b"IdList" => in_idlist = false,
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => {
                return Err(SourceError::MalformedResponse {
                    detail: format!("invalid esearch XML: {error}"),
                })
            }
        }
    }
    let count = count.ok_or_else(|| SourceError::MalformedResponse {
        detail: "missing <Count> in esearch response".to_owned(),
    })?;
    Ok(EsearchSummary { count, ids })
}

fn read_text(reader: &mut Reader<&[u8]>) -> Result<String, SourceError> {
    let mut text = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Text(value)) => text.push_str(&value.unescape().map_err(|error| {
                SourceError::MalformedResponse {
                    detail: format!("undecodable text: {error}"),
                }
            })?),
            Ok(Event::End(_)) | Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => {
                return Err(SourceError::MalformedResponse {
                    detail: format!("invalid XML text: {error}"),
                })
            }
        }
    }
    Ok(text)
}

#[async_trait]
impl SourceAdapter for PmcAdapter {
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
                "discovery via esearch + esummary (db=pmc), two spaced requests per page"
                    .to_owned(),
                "doi lookup via esearch term=<doi>[PID] with exact-match verification".to_owned(),
                "OA bulk utility (oa.fcgi) observed 404; no direct PDF links claimed".to_owned(),
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
        let retstart: usize = cursor
            .and_then(|cursor| cursor.parse().ok())
            .filter(|offset| *offset > 0)
            .unwrap_or(0);
        let search_text = self
            .client
            .get_text(&self.esearch_url(&query.text, page_size, retstart))
            .await?;
        let search = parse_esearch(&search_text)?;
        let count = search.count;
        let ids = search.ids;
        if ids.is_empty() {
            return Ok(DiscoveryPage {
                records: Vec::new(),
                next_cursor: None,
            });
        }
        let summaries = self.client.get_json(&self.esummary_url(&ids)).await?;
        let result = summaries
            .get("result")
            .ok_or_else(|| SourceError::MalformedResponse {
                detail: "missing `result` object in esummary".to_owned(),
            })?;
        let uids = result
            .get("uids")
            .and_then(|uids| uids.as_array())
            .cloned()
            .unwrap_or_default();
        let records = uids
            .iter()
            .filter_map(|uid| uid.as_str())
            .filter_map(|uid| result.get(uid))
            .filter_map(|summary| self.parse_summary(summary))
            .collect();
        let next_cursor = if (retstart as u64) + (page_size as u64) < count {
            Some((retstart + page_size as usize).to_string())
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
        // The [PID] field matches the publisher id / DOI exactly (verified live:
        // Count=1 for a known DOI); the article ids are re-verified below anyway.
        let url = self.esearch_url(&format!("{normalized}[PID]"), 5, 0);
        let search_text = self.client.get_text(&url).await?;
        let search = parse_esearch(&search_text)?;
        let ids = search.ids;
        if ids.is_empty() {
            return Ok(None);
        }
        let summaries = self.client.get_json(&self.esummary_url(&ids)).await?;
        let result = summaries
            .get("result")
            .ok_or_else(|| SourceError::MalformedResponse {
                detail: "missing `result` object in esummary".to_owned(),
            })?;
        for uid in result
            .get("uids")
            .and_then(|uids| uids.as_array())
            .cloned()
            .unwrap_or_default()
        {
            let uid = uid.as_str().map(|uid| uid.to_owned());
            let Some(uid) = uid else { continue };
            let Some(summary) = result.get(&uid) else {
                continue;
            };
            if let Some(record) = self.parse_summary(summary) {
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

    fn adapter() -> PmcAdapter {
        PmcAdapter::new(PmcConfig {
            min_request_interval: Some(Duration::from_millis(0)),
            ..PmcConfig::default()
        })
        .unwrap()
    }

    #[test]
    fn descriptor_declares_metadata_only_capabilities() {
        let adapter = adapter();
        let descriptor = adapter.descriptor();
        assert_eq!(descriptor.id, "pmc");
        assert_eq!(descriptor.contract_version, SOURCE_ADAPTER_CONTRACT_VERSION);
        assert!(descriptor.supports(Capability::Discovery));
        assert!(descriptor.supports(Capability::DoiLookup));
        assert!(!descriptor.supports(Capability::DirectPdfUrls));
        assert_eq!(
            descriptor.rate_policy.min_request_interval,
            PMC_MIN_REQUEST_INTERVAL
        );
    }

    #[test]
    fn parses_recorded_esummary_response() {
        let adapter = adapter();
        let body: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/pmc_esummary.json")).unwrap();
        let result = &body["result"];
        let uid = result["uids"][0].as_str().unwrap();
        let record = adapter.parse_summary(&result[uid]).unwrap();
        assert_eq!(record.source_id, "pmc");
        assert_eq!(record.source_record_id, "PMC13362148");
        assert_eq!(
            record.identifier(IdentifierKind::Pmcid),
            Some("PMC13362148")
        );
        assert_eq!(record.doi(), Some("10.1371/journal.pone.0353788"));
        assert_eq!(record.identifier(IdentifierKind::Pmid), Some("42441585"));
        assert_eq!(record.publication_year, Some(2026));
        // `fulljournalname` is preferred over the abbreviated `source` field.
        assert_eq!(record.venue.as_deref(), Some("PloS one"));
        assert!(record.title.contains("edgeR"));
        assert!(!record.authors.is_empty());
    }

    #[test]
    fn summaries_never_claim_a_pdf_link() {
        let adapter = adapter();
        let body: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/pmc_esummary_deseq2.json"
        ))
        .unwrap();
        let result = &body["result"];
        let record = adapter.parse_summary(&result["4302049"]).unwrap();
        assert_eq!(record.source_record_id, "PMC4302049");
        assert_eq!(
            record.title,
            "Moderated estimation of fold change and dispersion for RNA-seq data with DESeq2."
        );
        assert_eq!(record.venue.as_deref(), Some("Genome biology"));
        assert_eq!(record.publication_year, Some(2014));
        assert!(record.oa_pdf_url.is_none());
        assert_eq!(
            record.landing_url.as_deref(),
            Some("https://doi.org/10.1186/s13059-014-0550-8")
        );
    }

    #[test]
    fn parses_recorded_esearch_xml() {
        let document = include_str!("../../tests/fixtures/pmc_esearch.xml");
        let summary = parse_esearch(document).unwrap();
        assert_eq!(summary.count, 6);
        assert_eq!(summary.ids, vec!["13362148", "12519442"]);
    }

    #[test]
    fn parses_recorded_esearch_doi_xml() {
        let document = include_str!("../../tests/fixtures/pmc_esearch_doi.xml");
        let summary = parse_esearch(document).unwrap();
        assert_eq!(summary.count, 1);
        assert_eq!(summary.ids, vec!["4302049"]);
    }

    #[test]
    fn malformed_esearch_xml_is_a_typed_error() {
        let error = parse_esearch("not xml at all").unwrap_err();
        assert!(matches!(error, SourceError::MalformedResponse { .. }));
        let error = parse_esearch("<eSearchResult><IdList><Id>1</Id></IdList></eSearchResult>")
            .unwrap_err();
        assert!(error.to_string().contains("Count"));
    }

    #[test]
    fn malformed_summaries_are_skipped_not_fatal() {
        let adapter = adapter();
        assert!(adapter
            .parse_summary(&serde_json::json!({ "title": "no articleids" }))
            .is_none());
    }
}
