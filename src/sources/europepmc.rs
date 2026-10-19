//! Europe PMC adapter (#12).
//!
//! Uses the documented Europe PMC REST API
//! (`https://www.ebi.ac.uk/europepmc/webservices/rest/search`). Access basis: free
//! public API over PubMed and the PMC collections for modest, rate-limited queries;
//! EBI asks clients to stay well below their service limits, which the one-second
//! spacing policy honors. Deep pagination uses the documented
//! `cursorMark`/`nextCursorMark` protocol, and searches request `resultType=core` so
//! records carry the `fullTextUrlList`.
//!
//! For open-access records the API reports a direct render PDF entry
//! (`availabilityCode: "OA"`, `documentStyle: "pdf"`, `site: "Europe_PMC"`,
//! `https://europepmc.org/articles/{pmcid}?pdf=render`). Those URLs are recorded as
//! `oa_pdf_url` — the source itself reports them — and the adapter additionally keeps
//! the JATS full-text endpoint (`/rest/{PMCID}/fullTextXML`) in scope for the future
//! extraction slice. Subscription and free-while-subscribed entries never become PDF
//! links.
//!
//! The render URLs live on the europepmc.org **website**, which applies bot protection
//! that rejects datacenter clients with HTTP 403. The refusal is surfaced as a stop
//! condition and recorded as BLOCKED evidence — the engine never retries past it or
//! tries to evade it. Acquisition from an unrestricted network (for example Colab)
//! remains the documented way to complete Layer-3 verification.

use crate::sources::contract::{
    Capability, DiscoveryPage, DiscoveryQuery, RatePolicy, SelfTestReport, SourceAdapter,
    SourceDescriptor, SourceError, SOURCE_ADAPTER_CONTRACT_VERSION,
};
use crate::sources::http::{urlencode, SourceHttpClient, SourceHttpClientOptions};
use crate::sources::normalize::{
    normalize_doi, normalize_pmcid, normalize_pmid, normalize_title, normalize_year,
};
use crate::sources::work_record::{
    IdentifierKind, WorkRecord, WorkTypeFilter, PAPER_LIKE_WORK_TYPES,
};
use async_trait::async_trait;
use std::time::Duration;

pub const EPMC_DEFAULT_API_BASE: &str = "https://www.ebi.ac.uk/europepmc/webservices/rest";
/// EBI service guidance asks clients to stay well below three requests per second; one
/// second of spacing is the conservative default this adapter enforces.
pub const EPMC_MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(1_000);
/// Documented `cursorMark` value that starts a new search traversal.
const CURSOR_START: &str = "*";

pub struct EuropePmcAdapter {
    descriptor: SourceDescriptor,
    client: SourceHttpClient,
    api_base: String,
    paper_types: WorkTypeFilter,
}

#[derive(Debug, Clone, Default)]
pub struct EuropePmcConfig {
    /// Appended to the User-Agent for the polite pool; optional for this API.
    pub contact_email: Option<String>,
    /// Override for tests and mirrors; defaults to the public API.
    pub api_base: Option<String>,
    /// Work-type filter; defaults to paper-only.
    pub paper_types: WorkTypeFilter,
    pub request_timeout: Option<Duration>,
    pub min_request_interval: Option<Duration>,
}

impl EuropePmcAdapter {
    pub fn new(config: EuropePmcConfig) -> Result<Self, SourceError> {
        let client = SourceHttpClient::new(SourceHttpClientOptions {
            contact_email: config.contact_email.clone(),
            request_timeout: config.request_timeout.unwrap_or(Duration::from_secs(30)),
            min_request_interval: config
                .min_request_interval
                .unwrap_or(EPMC_MIN_REQUEST_INTERVAL),
            accept: None,
        })?;
        Ok(Self {
            descriptor: SourceDescriptor {
                id: "europepmc",
                display_name: "Europe PMC",
                adapter_version: 1,
                contract_version: SOURCE_ADAPTER_CONTRACT_VERSION,
                api_base: "https://www.ebi.ac.uk/europepmc/webservices/rest/search",
                access_basis: "free public REST API over PubMed and PMC; the OA subset \
                               reports direct PDF render URLs",
                capabilities: &[
                    Capability::Discovery,
                    Capability::DoiLookup,
                    Capability::DirectPdfUrls,
                ],
                rate_policy: RatePolicy {
                    min_request_interval: EPMC_MIN_REQUEST_INTERVAL,
                    default_page_size: 25,
                    max_page_size: 100,
                },
            },
            client,
            api_base: config
                .api_base
                .unwrap_or_else(|| EPMC_DEFAULT_API_BASE.to_owned()),
            paper_types: config.paper_types,
        })
    }

    fn search_url(&self, query: &str, page_size: u16, cursor_mark: &str) -> String {
        format!(
            "{}/search?query={}&format=json&resultType=core&pageSize={page_size}&cursorMark={}",
            self.api_base,
            urlencode(query),
            urlencode(cursor_mark),
        )
    }
    /// Extract the result list from a search response, or a typed malformed-response error.
    fn result_entries<'a>(
        &self,
        body: &'a serde_json::Value,
    ) -> Result<&'a [serde_json::Value], SourceError> {
        body.get("resultList")
            .and_then(|list| list.get("result"))
            .and_then(|results| results.as_array())
            .map(|results| results.as_slice())
            .ok_or_else(|| SourceError::MalformedResponse {
                detail: "search response is missing resultList.result".to_owned(),
            })
    }

    /// Paper-like under the default filter. Three shapes are honored: the preprint
    /// collection (`source: "PPR"`), the structured `pubTypeList` of `resultType=core`
    /// responses, and the flat semicolon `pubType` string of `resultType=lite`
    /// responses. Errata, corrections, books, and datasets stay out of the default
    /// set; callers widen with `WorkTypeFilter::All`.
    fn accepts_pub_types(&self, record: &serde_json::Value) -> bool {
        if matches!(self.paper_types, WorkTypeFilter::All) {
            return true;
        }
        if record.get("source").and_then(|source| source.as_str()) == Some("PPR") {
            return true;
        }
        let entries = pub_type_entries(record);
        match &self.paper_types {
            WorkTypeFilter::All => true,
            WorkTypeFilter::Only(accepted) => entries
                .iter()
                .any(|entry| accepted.iter().any(|candidate| candidate == entry)),
            // Europe PMC reports compound clinical-trial phases ("Clinical Trial,
            // Phase III") as distinct strings; the prefix rule keeps them paper-like
            // without enumerating every phase combination.
            WorkTypeFilter::PaperOnly => entries.iter().any(|entry| {
                PAPER_LIKE_WORK_TYPES.contains(&entry.as_str())
                    || entry.starts_with("clinical-trial")
            }),
        }
    }

    /// Parse one result-list record into a normalized work record. Records without a
    /// usable source id, or filtered out by the work-type policy, yield `None` and are
    /// skipped.
    fn parse_record(&self, record: &serde_json::Value) -> Option<WorkRecord> {
        if !self.accepts_pub_types(record) {
            return None;
        }
        let source = record.get("source")?.as_str()?;
        let id = record.get("id")?.as_str()?;
        if source.is_empty() || id.is_empty() {
            return None;
        }
        let mut work = WorkRecord::new("europepmc", format!("{source}/{id}"));
        work.push_identifier(
            IdentifierKind::Pmcid,
            record
                .get("pmcid")
                .and_then(|value| value.as_str())
                .and_then(normalize_pmcid),
        );
        work.push_identifier(
            IdentifierKind::Pmid,
            record
                .get("pmid")
                .and_then(|value| value.as_str())
                .and_then(normalize_pmid),
        );
        work.push_identifier(
            IdentifierKind::Doi,
            record
                .get("doi")
                .and_then(|value| value.as_str())
                .and_then(normalize_doi),
        );
        work.title = record
            .get("title")
            .and_then(|value| value.as_str())
            .map(normalize_title)
            .unwrap_or_default();
        work.authors = parse_authors(record);
        work.publication_year = record
            .get("pubYear")
            .and_then(|value| value.as_str())
            .and_then(|year| year.trim().get(..4))
            .and_then(|year| year.parse::<u64>().ok())
            .and_then(normalize_year);
        work.venue = record
            .get("journalInfo")
            .and_then(|info| info.get("journal"))
            .and_then(|journal| journal.get("title"))
            .and_then(|title| title.as_str())
            .or_else(|| record.get("journalTitle").and_then(|t| t.as_str()))
            .map(|title| title.to_owned());
        work.license = record
            .get("license")
            .and_then(|value| value.as_str())
            .map(|license| license.to_owned());
        let links = parse_full_text_urls(record);
        // A PDF URL is recorded only when Europe PMC itself marks the entry as open
        // access: subscription and free-while-subscribed entries are landing pages,
        // not permitted direct downloads.
        work.oa_pdf_url = links
            .iter()
            .find(|link| {
                link.availability_code == "OA"
                    && link.document_style == "pdf"
                    && link.site == "Europe_PMC"
            })
            .map(|link| link.url.clone());
        work.landing_url = links
            .iter()
            .find(|link| link.document_style == "html" && link.site == "Europe_PMC")
            .map(|link| link.url.clone())
            .or_else(|| {
                links
                    .iter()
                    .find(|link| link.document_style == "doi" && link.site == "DOI")
                    .map(|link| link.url.clone())
            })
            .or_else(|| work.doi().map(|doi| format!("https://doi.org/{doi}")));
        work.raw = record.clone();
        Some(work)
    }
}

/// Canonicalized publication-type strings: `pubTypeList.pubType[]` (core responses)
/// plus the flat semicolon-separated `pubType` string (lite responses), lowercased
/// with interior whitespace runs collapsed to single hyphens so "Journal Article" and
/// "Case Reports" become "journal-article" and "case-reports".
fn pub_type_entries(record: &serde_json::Value) -> Vec<String> {
    let mut entries = Vec::new();
    if let Some(types) = record
        .get("pubTypeList")
        .and_then(|list| list.get("pubType"))
        .and_then(|types| types.as_array())
    {
        entries.extend(types.iter().filter_map(|entry| entry.as_str()));
    }
    if let Some(flat) = record.get("pubType").and_then(|t| t.as_str()) {
        entries.extend(flat.split(';'));
    }
    entries
        .into_iter()
        .map(|entry| {
            entry
                .split_whitespace()
                .collect::<Vec<_>>()
                .join("-")
                .to_ascii_lowercase()
        })
        .collect()
}

/// Europe PMC reports authors as `authorList.author[].fullName` (core responses) with
/// the flat `authorString` ("Surname I, Surname I") fallback. The fallback splits on
/// commas, trims, and strips trailing periods so both paths produce the same
/// normalized name form.
fn parse_authors(record: &serde_json::Value) -> Vec<String> {
    let structured = record
        .get("authorList")
        .and_then(|list| list.get("author"))
        .and_then(|authors| authors.as_array())
        .map(|authors| {
            authors
                .iter()
                .filter_map(|author| {
                    author
                        .get("fullName")
                        .or_else(|| author.get("collectName"))
                        .and_then(|name| name.as_str())
                        .map(normalize_title)
                })
                .collect::<Vec<String>>()
        })
        .unwrap_or_default();
    if !structured.is_empty() {
        return structured;
    }
    record
        .get("authorString")
        .and_then(|value| value.as_str())
        .map(|author_string| {
            author_string
                .split(',')
                .filter_map(|author| {
                    let author = author.trim_end_matches('.').trim();
                    (!author.is_empty()).then(|| normalize_title(author))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// One entry of the record's `fullTextUrlList`.
struct FullTextLink {
    availability_code: String,
    document_style: String,
    site: String,
    url: String,
}

fn parse_full_text_urls(record: &serde_json::Value) -> Vec<FullTextLink> {
    record
        .get("fullTextUrlList")
        .and_then(|list| list.get("fullTextUrl"))
        .and_then(|urls| urls.as_array())
        .map(|urls| {
            urls.iter()
                .filter_map(|entry| {
                    Some(FullTextLink {
                        availability_code: entry.get("availabilityCode")?.as_str()?.to_owned(),
                        document_style: entry.get("documentStyle")?.as_str()?.to_owned(),
                        site: entry.get("site")?.as_str()?.to_owned(),
                        url: entry.get("url")?.as_str()?.to_owned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
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
                "discovery and doi lookup via /search with cursorMark pagination".to_owned(),
                "oa_pdf_url only from fullTextUrlList entries marked Open access + pdf".to_owned(),
                "render PDFs live on europepmc.org, which may 403 datacenter clients; \
                  that refusal is surfaced, never bypassed"
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
        let cursor_mark = cursor.unwrap_or(CURSOR_START);
        let url = self.search_url(&query.text, page_size, cursor_mark);
        let body = self.client.get_json(&url).await?;
        let results = self.result_entries(&body)?;
        let raw_count = results.len();
        let mut records = Vec::new();
        for entry in results {
            if let Some(record) = self.parse_record(entry) {
                records.push(record);
            }
        }
        // Continue only while the source filled the page we asked for and produced a
        // new cursor. An all-filtered page stops the traversal: the bounded runner
        // treats an empty page as source exhaustion.
        let next_cursor = body
            .get("nextCursorMark")
            .and_then(|value| value.as_str())
            .filter(|next| !next.is_empty() && *next != cursor_mark)
            .filter(|_| raw_count >= page_size as usize && !records.is_empty())
            .map(|next| next.to_owned());
        Ok(DiscoveryPage {
            records,
            next_cursor,
        })
    }

    async fn lookup_doi(&self, doi: &str) -> Result<Option<WorkRecord>, SourceError> {
        let normalized = normalize_doi(doi).ok_or_else(|| SourceError::InvalidConfiguration {
            detail: format!("not a plausible DOI: {doi}"),
        })?;
        let url = self.search_url(&format!("DOI:\"{normalized}\""), 1, CURSOR_START);
        let body = self.client.get_json(&url).await?;
        let results = self.result_entries(&body)?;
        Ok(results.first().and_then(|entry| self.parse_record(entry)))
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

    fn adapter_all_types() -> EuropePmcAdapter {
        EuropePmcAdapter::new(EuropePmcConfig {
            paper_types: WorkTypeFilter::All,
            min_request_interval: Some(Duration::from_millis(0)),
            ..EuropePmcConfig::default()
        })
        .unwrap()
    }

    #[test]
    fn descriptor_declares_capabilities_and_rate_policy() {
        let adapter = adapter();
        let descriptor = adapter.descriptor();
        assert_eq!(descriptor.id, "europepmc");
        assert_eq!(descriptor.contract_version, SOURCE_ADAPTER_CONTRACT_VERSION);
        assert!(descriptor.supports(Capability::Discovery));
        assert!(descriptor.supports(Capability::DoiLookup));
        assert!(descriptor.supports(Capability::DirectPdfUrls));
        assert_eq!(
            descriptor.rate_policy.min_request_interval,
            EPMC_MIN_REQUEST_INTERVAL
        );
    }

    #[test]
    fn parses_recorded_core_search_response_with_mixed_access() {
        // Recorded live response for query "attention is all you need" (2026-10-19):
        // two PPR preprints without PMCIDs, an open-access erratum, and an open-access
        // journal article. Under the paper-only default the erratum is filtered out.
        let adapter = adapter();
        let body: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/epmc_search.json")).unwrap();
        let results = body["resultList"]["result"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|entry| adapter.parse_record(entry))
            .collect::<Vec<WorkRecord>>();
        assert_eq!(
            results.len(),
            3,
            "two preprints + one OA article pass paper-only"
        );

        let first = &results[0];
        assert_eq!(first.source_id, "europepmc");
        assert_eq!(first.source_record_id, "PPR/PPR1328024");
        assert_eq!(first.oa_pdf_url, None, "non-OA preprint has no PDF link");
        assert_eq!(
            first.doi(),
            Some("10.64898/2026.09.20.26363284"),
            "preprint DOIs are normalized"
        );
        assert_eq!(first.publication_year, Some(2026));
        assert_eq!(
            first.authors.first().map(String::as_str),
            Some("Zhao D"),
            "structured authorList feeds the author names"
        );

        let second = &results[1];
        assert_eq!(second.source_record_id, "PPR/PPR1136986");
        assert_eq!(second.oa_pdf_url, None);

        let oa = &results[2];
        assert_eq!(oa.source_record_id, "MED/42715167");
        assert_eq!(oa.identifier(IdentifierKind::Pmcid), Some("PMC13557405"));
        assert_eq!(oa.identifier(IdentifierKind::Pmid), Some("42715167"));
        assert_eq!(oa.doi(), Some("10.1371/journal.pone.0356147"));
        assert_eq!(oa.venue.as_deref(), Some("PloS one"));
        assert_eq!(oa.license.as_deref(), Some("cc by"));
        assert_eq!(
            oa.oa_pdf_url.as_deref(),
            Some("https://europepmc.org/articles/PMC13557405?pdf=render"),
            "the Open access + pdf + Europe_PMC entry becomes the OA PDF link"
        );
        assert_eq!(
            oa.landing_url.as_deref(),
            Some("https://europepmc.org/articles/PMC13557405")
        );
        assert!(oa.raw["isOpenAccess"].is_string(), "raw record retained");
    }

    #[test]
    fn recorded_erratum_is_filtered_by_paper_only_default() {
        // The recorded open-access erratum (MED/42059664, "Published Erratum"/"correction")
        // is excluded under the paper-only default even though it carries an OA PDF link;
        // opting into all work types keeps it.
        let adapter = adapter();
        let body: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/epmc_search.json")).unwrap();
        let entries = body["resultList"]["result"].as_array().unwrap();
        let erratum = entries
            .iter()
            .find(|entry| entry["id"].as_str() == Some("42059664"))
            .unwrap();
        assert!(adapter.parse_record(erratum).is_none());
        assert!(adapter_all_types().parse_record(erratum).is_some());
    }

    #[test]
    fn parses_recorded_lite_search_response_without_inventing_pdfs() {
        // Recorded lite response (query "DESeq2"): lite records carry no
        // fullTextUrlList, so no render link is invented; the DOI landing page is used
        // and the availability flags stay verbatim in raw.
        let adapter = adapter();
        let body: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/europepmc_search.json"))
                .unwrap();
        let entries = body["resultList"]["result"].as_array().unwrap();
        let record = adapter.parse_record(&entries[0]).unwrap();
        assert_eq!(record.source_record_id, "MED/42441585");
        assert_eq!(record.doi(), Some("10.1371/journal.pone.0353788"));
        assert_eq!(
            record.identifier(IdentifierKind::Pmcid),
            Some("PMC13362148")
        );
        assert_eq!(record.identifier(IdentifierKind::Pmid), Some("42441585"));
        assert_eq!(record.publication_year, Some(2026));
        assert_eq!(record.venue.as_deref(), Some("PLoS One"));
        assert_eq!(
            record.authors.first().map(String::as_str),
            Some("Rezapour M")
        );
        assert!(record.raw["hasPDF"] == "Y");
        assert!(record.raw["inEPMC"] == "Y");
        assert_eq!(
            record.oa_pdf_url, None,
            "lite records carry no fullTextUrlList; no PDF link is fabricated"
        );
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
        assert_eq!(record.source_record_id, "PPR/PPR1328024");
        assert_eq!(record.identifier(IdentifierKind::Pmcid), None);
        assert_eq!(record.publication_year, Some(2026));
    }

    #[test]
    fn records_without_pmcid_still_parse() {
        let adapter = adapter();
        let record = serde_json::json!({
            "id": "42", "source": "MED",
            "title": "No PMCID",
            "pubTypeList": { "pubType": ["Journal Article"] },
            "pmid": "42",
            "pubYear": "2024",
        });
        let parsed = adapter.parse_record(&record).unwrap();
        assert_eq!(parsed.source_record_id, "MED/42");
        assert_eq!(parsed.identifier(IdentifierKind::Pmid), Some("42"));
        assert_eq!(parsed.identifier(IdentifierKind::Pmcid), None);
        assert_eq!(parsed.oa_pdf_url, None);
    }

    #[test]
    fn author_string_fallback_splits_on_commas() {
        let adapter = adapter();
        let record = serde_json::json!({
            "id": "7", "source": "MED",
            "title": "Authors",
            "pubTypeList": { "pubType": ["Journal Article"] },
            "authorString": "Cooper DG, Grunkemeyer E, Fassler JS.",
        });
        let parsed = adapter.parse_record(&record).unwrap();
        assert_eq!(
            parsed.authors,
            vec!["Cooper DG", "Grunkemeyer E", "Fassler JS"],
            "trailing periods are stripped and separators trimmed"
        );
    }

    #[test]
    fn missing_pub_type_list_is_rejected_under_paper_only() {
        let adapter = adapter();
        let record = serde_json::json!({ "id": "9", "source": "MED", "title": "Untyped" });
        assert!(adapter.parse_record(&record).is_none());
        assert!(adapter_all_types().parse_record(&record).is_some());
    }

    #[test]
    fn clinical_trial_phases_are_paper_like() {
        let adapter = adapter();
        let record = serde_json::json!({
            "id": "11", "source": "MED",
            "title": "Trial",
            "pubTypeList": { "pubType": ["Clinical Trial, Phase III", "Journal Article"] },
        });
        assert!(adapter.parse_record(&record).is_some());
        let phase_only = serde_json::json!({
            "id": "12", "source": "MED",
            "title": "Trial",
            "pubTypeList": { "pubType": ["Clinical Trial, Phase III"] },
        });
        assert!(adapter.parse_record(&phase_only).is_some());
    }

    #[test]
    fn search_url_encodes_query_and_cursor() {
        let adapter = adapter();
        let url = adapter.search_url(
            "attention AND OPEN_ACCESS:Y",
            25,
            "AoIIQGTqLyg1NjU0ODk2OA==",
        );
        assert!(url.starts_with(EPMC_DEFAULT_API_BASE));
        assert!(url.contains("query=attention%20AND%20OPEN_ACCESS%3AY"));
        assert!(url.contains("format=json&resultType=core&pageSize=25"));
        assert!(url.contains("cursorMark=AoIIQGTqLyg1NjU0ODk2OA%3D%3D"));
    }

    #[test]
    fn doi_lookup_url_quotes_the_doi() {
        let adapter = adapter();
        let url = adapter.search_url("DOI:\"10.1371/journal.pone.0000219\"", 1, "*");
        assert!(url.contains("query=DOI%3A%2210.1371%2Fjournal.pone.0000219%22"));
        assert!(url.contains("pageSize=1"));
    }

    #[test]
    fn malformed_search_response_is_a_typed_error() {
        let adapter = adapter();
        let error = adapter
            .result_entries(&serde_json::json!({ "version": "1.0", "hitCount": 3 }))
            .unwrap_err();
        assert!(matches!(error, SourceError::MalformedResponse { .. }));
        let error = adapter
            .result_entries(&serde_json::json!({ "resultList": {} }))
            .unwrap_err();
        assert!(matches!(error, SourceError::MalformedResponse { .. }));
    }

    #[test]
    fn self_test_is_offline_and_reports_pdf_caveat() {
        let report = adapter().self_test();
        assert_eq!(report.source_id, "europepmc");
        assert!(report.configured);
        assert!(report.capabilities.contains(&"discovery".to_owned()));
        assert!(report.notes.iter().any(|note| note.contains("403")));
    }
}
