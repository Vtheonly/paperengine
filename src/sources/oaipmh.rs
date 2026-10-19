//! Generic OAI-PMH harvester adapter (#13).
//!
//! Implements the Open Archives Initiative Protocol for Metadata Harvesting 2.0
//! (`ListRecords` with `resumptionToken` continuation, `from`/`until` windows,
//! `oai_dc` Dublin Core payloads). One adapter class serves every repository
//! family that documents an OAI-PMH endpoint; instances are registered per
//! repository: `zenodo-oaipmh` (`https://zenodo.org/oai2d`), `doaj-oaipmh`
//! (`https://doaj.org/oai`), and `pmc-oaipmh`
//! (`https://pmc.ncbi.nlm.nih.gov/api/oai/v1/mh/` — the modern location of the
//! legacy `ncbi.nlm.nih.gov/pmc/oai/oai.cgi`, which 301-redirects there).
//!
//! OAI-PMH is a harvesting protocol, not a search interface: `discover` treats a
//! parseable ISO date in `query.text` as the `from` window and follows opaque
//! resumption tokens through `cursor`. Dublin Core carries no reliable
//! full-text URLs, so these adapters never claim a PDF link; they provide
//! metadata with identifiers (DOI/PMCID) and landing pages.

use crate::sources::contract::{
    Capability, DiscoveryPage, DiscoveryQuery, RatePolicy, SelfTestReport, SourceAdapter,
    SourceDescriptor, SourceError, SOURCE_ADAPTER_CONTRACT_VERSION,
};
use crate::sources::http::{SourceHttpClient, SourceHttpClientOptions};
use crate::sources::normalize::{normalize_doi, normalize_pmcid, normalize_title, normalize_year};
use crate::sources::work_record::{IdentifierKind, WorkRecord};
use async_trait::async_trait;
use quick_xml::events::Event;
use quick_xml::Reader;
use std::time::Duration;

pub const OAIPMH_MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(1_500);

pub struct OaiPmhAdapter {
    descriptor: SourceDescriptor,
    client: SourceHttpClient,
    api_base: String,
    metadata_prefix: String,
    paper_only: bool,
    set: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OaiPmhConfig {
    /// Registry id, e.g. `zenodo-oaipmh`.
    pub source_id: &'static str,
    pub display_name: &'static str,
    pub access_basis: &'static str,
    /// OAI-PMH base URL, e.g. `https://zenodo.org/oai2d`.
    pub api_base: String,
    pub metadata_prefix: String,
    pub request_timeout: Option<Duration>,
    pub min_request_interval: Option<Duration>,
    /// Keep only paper-like dc:type values (default true, #10).
    pub paper_only: bool,
    /// Optional OAI set spec.
    pub set: Option<String>,
}

impl OaiPmhConfig {
    /// Configuration for the Zenodo OAI-PMH instance.
    pub fn zenodo() -> Self {
        Self {
            source_id: "zenodo-oaipmh",
            display_name: "Zenodo (OAI-PMH)",
            access_basis: "public OAI-PMH 2.0 endpoint; Dublin Core metadata harvest",
            api_base: "https://zenodo.org/oai2d".into(),
            metadata_prefix: "oai_dc".into(),
            request_timeout: None,
            min_request_interval: None,
            paper_only: true,
            set: None,
        }
    }

    /// Configuration for the DOAJ OAI-PMH instance.
    pub fn doaj() -> Self {
        Self {
            source_id: "doaj-oaipmh",
            display_name: "DOAJ (OAI-PMH)",
            access_basis: "public OAI-PMH 2.0 endpoint; Dublin Core metadata harvest",
            api_base: "https://doaj.org/oai".into(),
            metadata_prefix: "oai_dc".into(),
            request_timeout: None,
            min_request_interval: None,
            paper_only: true,
            set: None,
        }
    }

    /// Configuration for the PubMed Central OAI-PMH instance. The legacy
    /// `https://www.ncbi.nlm.nih.gov/pmc/oai/oai.cgi` endpoint 301-redirects to
    /// this base; the modern URL is used directly.
    pub fn pmc() -> Self {
        Self {
            source_id: "pmc-oaipmh",
            display_name: "PubMed Central (OAI-PMH)",
            access_basis: "public OAI-PMH 2.0 endpoint; Dublin Core metadata harvest",
            api_base: "https://pmc.ncbi.nlm.nih.gov/api/oai/v1/mh/".into(),
            metadata_prefix: "oai_dc".into(),
            request_timeout: None,
            min_request_interval: None,
            paper_only: true,
            set: None,
        }
    }
}

/// One parsed harvest record: header fields plus Dublin Core elements.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OaiRecord {
    pub identifier: String,
    pub datestamp: String,
    pub deleted: bool,
    pub titles: Vec<String>,
    pub creators: Vec<String>,
    pub dates: Vec<String>,
    pub types: Vec<String>,
    pub publishers: Vec<String>,
    pub sources: Vec<String>,
    pub subjects: Vec<String>,
    pub identifiers: Vec<String>,
    pub relations: Vec<String>,
    pub rights: Vec<String>,
}

/// Parsed ListRecords page.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OaiPage {
    pub records: Vec<OaiRecord>,
    pub resumption_token: Option<String>,
}

impl OaiPmhAdapter {
    pub fn new(config: OaiPmhConfig) -> Result<Self, SourceError> {
        let client = SourceHttpClient::new(SourceHttpClientOptions {
            request_timeout: config.request_timeout.unwrap_or(Duration::from_secs(60)),
            min_request_interval: config
                .min_request_interval
                .unwrap_or(OAIPMH_MIN_REQUEST_INTERVAL),
            ..SourceHttpClientOptions::default()
        })?;
        // Descriptors require a `&'static str` informational base URL. Each
        // adapter is constructed once per process, so the small boxed string is
        // promoted to the process lifetime.
        let api_base: &'static str = Box::leak(config.api_base.clone().into_boxed_str());
        Ok(Self {
            descriptor: SourceDescriptor {
                id: config.source_id,
                display_name: config.display_name,
                adapter_version: 1,
                contract_version: SOURCE_ADAPTER_CONTRACT_VERSION,
                api_base,
                access_basis: config.access_basis,
                capabilities: &[Capability::Discovery],
                rate_policy: RatePolicy {
                    min_request_interval: OAIPMH_MIN_REQUEST_INTERVAL,
                    // OAI-PMH has no page-size parameter; the engine's record
                    // budget still bounds every run.
                    default_page_size: 50,
                    max_page_size: 500,
                },
            },
            client,
            api_base: config.api_base,
            metadata_prefix: config.metadata_prefix,
            paper_only: config.paper_only,
            set: config.set,
        })
    }

    fn list_records_url(&self, from: Option<&str>, token: Option<&str>) -> String {
        let mut url = format!(
            "{base}?{base_query}",
            base = self.api_base,
            base_query = "verb=ListRecords"
        );
        let mut push = |key: &str, value: &str| {
            url.push('&');
            url.push_str(key);
            url.push('=');
            url.push_str(&urlencode(value));
        };
        if let Some(token) = token {
            push("resumptionToken", token);
        } else {
            push("metadataPrefix", &self.metadata_prefix);
            if let Some(from) = from {
                push("from", from);
            }
            if let Some(set) = self.set.as_deref() {
                push("set", set);
            }
        }
        url
    }

    /// Paper-like dc:type test: Dublin Core types are free text across
    /// repositories (`Article`, `Text`, `info:eu-repo/semantics/article`,
    /// `journal`, …). Containers and non-paper objects stay out.
    fn accepts_type(&self, record: &OaiRecord) -> bool {
        if !self.paper_only {
            return true;
        }
        record.types.iter().any(|kind| {
            let kind = kind.to_ascii_lowercase();
            kind.contains("article")
                || kind.contains("text")
                || kind.contains("publication")
                || kind.contains("preprint")
                || kind.contains("thesis")
                || kind.contains("chapter")
                || kind.contains("proceedings")
                || kind.contains("report")
        })
    }

    fn parse_record(&self, harvest: &OaiRecord) -> Option<WorkRecord> {
        if harvest.deleted || harvest.identifier.is_empty() {
            return None;
        }
        if !self.accepts_type(harvest) {
            return None;
        }
        let mut record = WorkRecord::new(self.descriptor.id, harvest.identifier.clone());
        record.title = harvest
            .titles
            .first()
            .map(|title| normalize_title(title))
            .unwrap_or_default();
        record.authors = harvest
            .creators
            .iter()
            .map(|creator| normalize_title(creator))
            .collect();
        record.publication_year = harvest
            .dates
            .iter()
            .filter_map(|date| date.trim().get(..4).and_then(|y| y.parse::<u64>().ok()))
            .find_map(normalize_year);
        record.venue = harvest
            .sources
            .first()
            .or_else(|| harvest.publishers.first())
            .map(|source| source.to_owned());
        record.license = harvest.rights.first().map(|rights| rights.to_owned());
        for value in &harvest.identifiers {
            // DOI forms: bare, `doi:` URN, and https://doi.org/ URLs.
            if let Some(doi) = normalize_doi(value) {
                if record.doi().is_none() {
                    record.push_identifier(IdentifierKind::Doi, Some(doi));
                }
                continue;
            }
            // PMC article URLs carry the accession number.
            if let Some(pmcid) = value
                .split_once("/articles/")
                .and_then(|(_, tail)| tail.split('/').next())
                .and_then(normalize_pmcid)
            {
                record.push_identifier(IdentifierKind::Pmcid, Some(pmcid));
                continue;
            }
            // The first plain https identifier becomes the landing page.
            if record.landing_url.is_none() && value.starts_with("https://") {
                record.landing_url = Some(value.clone());
            }
        }
        if record.landing_url.is_none() {
            if let Some(landing) = harvest
                .relations
                .iter()
                .find(|relation| relation.starts_with("https://"))
            {
                record.landing_url = Some(landing.clone());
            }
        }
        if record.landing_url.is_none() {
            if let Some(doi) = record.doi() {
                record.landing_url = Some(format!("https://doi.org/{doi}"));
            }
        }
        record.raw = serde_json::json!({
            "oai_identifier": harvest.identifier,
            "datestamp": harvest.datestamp,
            "dc": {
                "title": harvest.titles,
                "creator": harvest.creators,
                "date": harvest.dates,
                "type": harvest.types,
                "publisher": harvest.publishers,
                "source": harvest.sources,
                "subject": harvest.subjects,
                "identifier": harvest.identifiers,
                "relation": harvest.relations,
                "rights": harvest.rights,
            },
            "deleted": harvest.deleted,
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

/// Parse one ListRecords document into records plus resumption token.
/// `noRecordsMatch` is a legitimate empty outcome, not an error; other OAI
/// error codes and malformed XML are typed errors.
pub fn parse_list_records(document: &str) -> Result<OaiPage, SourceError> {
    let mut reader = Reader::from_str(document);
    let mut page = OaiPage::default();
    let mut current: Option<OaiRecord> = None;
    // Where the next text event belongs.
    let mut target: Option<&'static str> = None;
    let mut in_header = false;
    let mut in_metadata = false;
    let mut error: Option<(String, String)> = None;
    let mut saw_envelope = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(start)) => {
                let name = start.name();
                let name = name.as_ref();
                match name {
                    b"OAI-PMH" => saw_envelope = true,
                    b"error" => {
                        let code = attr(&start, "code").unwrap_or_default();
                        let text = read_text(&mut reader)?;
                        error = Some((code, text));
                    }
                    b"ListRecords" => {}
                    b"record" => current = Some(OaiRecord::default()),
                    b"header" if current.is_some() => {
                        in_header = true;
                        if let Some(status) = attr(&start, "status") {
                            if status == "deleted" {
                                current.as_mut().expect("guarded").deleted = true;
                            }
                        }
                    }
                    b"identifier" if in_header && current.is_some() => {
                        target = Some("oai_identifier");
                    }
                    b"datestamp" if in_header && current.is_some() => {
                        target = Some("datestamp");
                    }
                    b"metadata" if current.is_some() => in_metadata = true,
                    b"dc:title" if in_metadata && current.is_some() => target = Some("dc:title"),
                    b"dc:creator" if in_metadata && current.is_some() => {
                        target = Some("dc:creator")
                    }
                    b"dc:date" if in_metadata && current.is_some() => target = Some("dc:date"),
                    b"dc:type" if in_metadata && current.is_some() => target = Some("dc:type"),
                    b"dc:publisher" if in_metadata && current.is_some() => {
                        target = Some("dc:publisher")
                    }
                    b"dc:source" if in_metadata && current.is_some() => target = Some("dc:source"),
                    b"dc:subject" if in_metadata && current.is_some() => {
                        target = Some("dc:subject")
                    }
                    b"dc:identifier" if in_metadata && current.is_some() => {
                        target = Some("dc:identifier")
                    }
                    b"dc:relation" if in_metadata && current.is_some() => {
                        target = Some("dc:relation")
                    }
                    b"dc:rights" if in_metadata && current.is_some() => target = Some("dc:rights"),
                    b"resumptionToken" => {
                        let token = read_text(&mut reader)?;
                        if !token.trim().is_empty() {
                            page.resumption_token = Some(token.trim().to_owned());
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(end)) => {
                let name = end.name();
                let name = name.as_ref();
                target = None;
                match name {
                    b"header" => in_header = false,
                    b"metadata" => in_metadata = false,
                    b"record" => {
                        if let Some(finished) = current.take() {
                            page.records.push(finished);
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(text)) => {
                if let Some(kind) = target {
                    let value = text
                        .unescape()
                        .map_err(|error| SourceError::MalformedResponse {
                            detail: format!("undecodable text: {error}"),
                        })?
                        .to_string();
                    let Some(record) = current.as_mut() else {
                        continue;
                    };
                    match kind {
                        "oai_identifier" => record.identifier = value,
                        "datestamp" => record.datestamp = value,
                        "dc:title" => record.titles.push(value),
                        "dc:creator" => record.creators.push(value),
                        "dc:date" => record.dates.push(value),
                        "dc:type" => record.types.push(value),
                        "dc:publisher" => record.publishers.push(value),
                        "dc:source" => record.sources.push(value),
                        "dc:subject" => record.subjects.push(value),
                        "dc:identifier" => record.identifiers.push(value),
                        "dc:relation" => record.relations.push(value),
                        "dc:rights" => record.rights.push(value),
                        _ => {}
                    }
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => {
                return Err(SourceError::MalformedResponse {
                    detail: format!("invalid OAI-PMH XML: {error}"),
                })
            }
        }
    }

    if let Some((code, text)) = error {
        if code == "noRecordsMatch" {
            return Ok(OaiPage::default());
        }
        return Err(SourceError::MalformedResponse {
            detail: format!("OAI-PMH error {code}: {text}"),
        });
    }
    if !saw_envelope {
        return Err(SourceError::MalformedResponse {
            detail: "document is not an OAI-PMH envelope".to_owned(),
        });
    }
    Ok(page)
}

fn attr(start: &quick_xml::events::BytesStart<'_>, name: &str) -> Option<String> {
    let mut value = None;
    for attribute in start.attributes() {
        let Ok(attribute) = attribute else { continue };
        if attribute.key.as_ref() == name.as_bytes() {
            value = attribute
                .unescape_value()
                .ok()
                .map(|value| value.to_string());
        }
    }
    value
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

/// Interpret the discovery query as a `from` window: a bare YYYY-MM-DD or a
/// full UTC ISO datetime. Anything else means "harvest from the beginning".
fn parse_from(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.len() == 10
        && trimmed.as_bytes()[4] == b'-'
        && trimmed.as_bytes()[7] == b'-'
        && trimmed[..4].bytes().all(|b| b.is_ascii_digit())
        && trimmed[5..7].bytes().all(|b| b.is_ascii_digit())
        && trimmed[8..10].bytes().all(|b| b.is_ascii_digit())
    {
        return Some(trimmed.to_owned());
    }
    if trimmed.len() >= 19 && trimmed.ends_with('Z') && trimmed.as_bytes()[10] == b'T' {
        return Some(trimmed.to_owned());
    }
    None
}

#[async_trait]
impl SourceAdapter for OaiPmhAdapter {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    fn self_test(&self) -> SelfTestReport {
        SelfTestReport {
            source_id: self.descriptor.id.to_owned(),
            adapter_version: self.descriptor.adapter_version,
            contract_version: self.descriptor.contract_version,
            capabilities: vec![Capability::Discovery.as_str().to_owned()],
            configured: true,
            notes: vec![
                "harvest via ListRecords; resumptionToken continuation through the cursor"
                    .to_owned(),
                "query.text parsed as the `from` date window when it is a valid ISO date"
                    .to_owned(),
                "Dublin Core carries no reliable full-text URLs; no PDF links are claimed"
                    .to_owned(),
            ],
        }
    }

    async fn discover(
        &self,
        query: &DiscoveryQuery,
        cursor: Option<&str>,
    ) -> Result<DiscoveryPage, SourceError> {
        let token = cursor.filter(|token| !token.is_empty());
        let from = if token.is_none() {
            parse_from(&query.text)
        } else {
            None
        };
        let url = self.list_records_url(from.as_deref(), token);
        let body = self.client.get_text(&url).await?;
        let page = parse_list_records(&body)?;
        let records = page
            .records
            .iter()
            .filter_map(|harvest| self.parse_record(harvest))
            .collect();
        Ok(DiscoveryPage {
            records,
            next_cursor: page.resumption_token,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zenodo_adapter() -> OaiPmhAdapter {
        OaiPmhAdapter::new(OaiPmhConfig::zenodo()).unwrap()
    }

    fn pmc_adapter() -> OaiPmhAdapter {
        OaiPmhAdapter::new(OaiPmhConfig::pmc()).unwrap()
    }

    fn doaj_adapter() -> OaiPmhAdapter {
        OaiPmhAdapter::new(OaiPmhConfig::doaj()).unwrap()
    }

    #[test]
    fn descriptor_declares_harvest_only_capabilities() {
        let adapter = zenodo_adapter();
        let adapter_ref = &adapter;
        let descriptor = adapter_ref.descriptor();
        assert_eq!(descriptor.id, "zenodo-oaipmh");
        assert_eq!(descriptor.contract_version, SOURCE_ADAPTER_CONTRACT_VERSION);
        assert!(descriptor.supports(Capability::Discovery));
        assert!(!descriptor.supports(Capability::DoiLookup));
        assert!(!descriptor.supports(Capability::DirectPdfUrls));
    }

    #[test]
    fn parses_recorded_zenodo_response_with_resumption_token() {
        let adapter = zenodo_adapter();
        let document = include_str!("../../tests/fixtures/zenodo_oai_records.xml");
        let page = parse_list_records(document).unwrap();
        assert_eq!(page.records.len(), 3);
        assert!(page.resumption_token.is_some());
        // All three recorded records are `semantics/other` software/misc uploads:
        // rejected under the default paper-only policy.
        let kept: Vec<_> = page
            .records
            .iter()
            .filter_map(|harvest| adapter.parse_record(harvest))
            .collect();
        assert!(kept.is_empty());

        // In permissive mode they parse with identifiers and a DOI.
        let permissive = OaiPmhAdapter::new(OaiPmhConfig {
            paper_only: false,
            ..OaiPmhConfig::zenodo()
        })
        .unwrap();
        let record = permissive
            .parse_record(&page.records[0])
            .expect("permissive mode keeps semantics/other");
        assert_eq!(record.source_id, "zenodo-oaipmh");
        assert_eq!(record.source_record_id, "oai:zenodo.org:23470914");
        assert_eq!(record.doi(), Some("10.5281/zenodo.23470914"));
        assert_eq!(record.publication_year, Some(2026));
        assert!(record.oa_pdf_url.is_none(), "OAI DC never claims a PDF");
    }

    #[test]
    fn parses_recorded_doaj_response_and_filters_journals() {
        let adapter = doaj_adapter();
        let document = include_str!("../../tests/fixtures/doaj_oai_records.xml");
        let page = parse_list_records(document).unwrap();
        assert_eq!(page.records.len(), 2);
        assert!(page.resumption_token.is_some());
        // Journal records are containers, not papers: rejected by default.
        assert!(page
            .records
            .iter()
            .all(|harvest| adapter.parse_record(harvest).is_none()));

        let permissive = OaiPmhAdapter::new(OaiPmhConfig {
            paper_only: false,
            ..OaiPmhConfig::doaj()
        })
        .unwrap();
        let record = permissive.parse_record(&page.records[0]).unwrap();
        assert!(record.source_record_id.starts_with("oai:doaj.org/journal:"));
        assert!(!record.title.is_empty());
        assert!(record.oa_pdf_url.is_none());
    }

    #[test]
    fn parses_recorded_pmc_response_with_identifiers() {
        let adapter = pmc_adapter();
        let document = include_str!("../../tests/fixtures/pmc_oai_records.xml");
        let page = parse_list_records(document).unwrap();
        assert_eq!(page.records.len(), 3);
        assert!(page.resumption_token.is_some());
        // PMC's dc:type is `Text`: paper-like and accepted by default.
        let record = adapter
            .parse_record(&page.records[0])
            .expect("PMC Text records are paper-like");
        assert_eq!(record.source_id, "pmc-oaipmh");
        assert_eq!(record.source_record_id, "oai:pubmedcentral.nih.gov:6718227");
        assert_eq!(record.identifier(IdentifierKind::Pmcid), Some("PMC6718227"));
        assert_eq!(record.doi(), Some("10.1002/14651858.cd013413"));
        assert!(record.title.contains("tobacco"));
        assert_eq!(record.venue.as_deref(), Some("Cochrane Database Syst Rev"));
        assert!(record.oa_pdf_url.is_none(), "OAI DC never claims a PDF");
    }

    #[test]
    fn no_records_match_is_an_empty_page_not_an_error() {
        let document = include_str!("../../tests/fixtures/doaj_oai_norecords.xml");
        let page = parse_list_records(document).unwrap();
        assert!(page.records.is_empty());
        assert!(page.resumption_token.is_none());
    }

    #[test]
    fn deleted_records_are_skipped() {
        let harvest = OaiRecord {
            identifier: "oai:example.org:1".into(),
            deleted: true,
            types: vec!["Article".into()],
            titles: vec!["Withdrawn".into()],
            ..OaiRecord::default()
        };
        assert!(zenodo_adapter().parse_record(&harvest).is_none());
    }

    #[test]
    fn oai_protocol_errors_are_typed() {
        let document = r#"<?xml version="1.0"?>
<OAI-PMH xmlns="http://www.openarchives.org/OAI/2.0/">
  <error code="badArgument">Missing metadataPrefix</error>
</OAI-PMH>"#;
        let error = parse_list_records(document).unwrap_err();
        assert!(error.to_string().contains("badArgument"));
        assert!(matches!(error, SourceError::MalformedResponse { .. }));

        let error = parse_list_records("not xml").unwrap_err();
        assert!(matches!(error, SourceError::MalformedResponse { .. }));
    }

    #[test]
    fn from_windows_are_parsed_from_query_text() {
        assert_eq!(parse_from("2026-09-01").as_deref(), Some("2026-09-01"));
        assert_eq!(
            parse_from("2026-09-01T10:00:00Z").as_deref(),
            Some("2026-09-01T10:00:00Z")
        );
        assert_eq!(parse_from("attention is all you need"), None);
        assert_eq!(parse_from("2026-9-1"), None);
    }

    #[test]
    fn resumption_token_urls_replace_from_windows() {
        let adapter = zenodo_adapter();
        assert_eq!(
            adapter.list_records_url(Some("2026-10-19"), None),
            "https://zenodo.org/oai2d?verb=ListRecords&metadataPrefix=oai_dc&from=2026-10-19"
        );
        assert_eq!(
            adapter.list_records_url(None, Some("tok en+1")),
            "https://zenodo.org/oai2d?verb=ListRecords&resumptionToken=tok%20en%2B1"
        );
    }
}
