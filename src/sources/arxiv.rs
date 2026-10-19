//! arXiv adapter (#12 partial).
//!
//! Uses the documented arXiv API (`https://export.arxiv.org/api/query`, Atom XML). Access
//! basis: public API for modest, rate-limited queries; arXiv asks clients to keep at least
//! three seconds between requests, which the rate policy enforces. Bulk harvesting belongs
//! to arXiv's requester-pays S3 buckets, not this adapter.
//!
//! arXiv entries expose direct, openly accessible PDF links; the adapter records them as
//! `oa_pdf_url` after upgrading `http://` to `https://`.

use crate::sources::contract::{
    Capability, DiscoveryPage, DiscoveryQuery, RatePolicy, SelfTestReport, SourceAdapter,
    SourceDescriptor, SourceError, SOURCE_ADAPTER_CONTRACT_VERSION,
};
use crate::sources::http::{SourceHttpClient, SourceHttpClientOptions};
use crate::sources::normalize::{
    normalize_arxiv_id, normalize_doi, normalize_title, normalize_year,
};
use crate::sources::work_record::{IdentifierKind, WorkRecord};
use async_trait::async_trait;
use quick_xml::events::Event;
use quick_xml::Reader;
use std::time::Duration;

pub const ARXIV_DEFAULT_API_BASE: &str = "https://export.arxiv.org";
/// arXiv's documented courtesy interval for API clients.
pub const ARXIV_MIN_REQUEST_INTERVAL: Duration = Duration::from_secs(3);

pub struct ArxivAdapter {
    descriptor: SourceDescriptor,
    client: SourceHttpClient,
    api_base: String,
}

#[derive(Debug, Clone, Default)]
pub struct ArxivConfig {
    pub request_timeout: Option<Duration>,
    pub min_request_interval: Option<Duration>,
    /// Override for tests; defaults to the public API.
    pub api_base: Option<String>,
}

impl ArxivAdapter {
    pub fn new(config: ArxivConfig) -> Result<Self, SourceError> {
        let client = SourceHttpClient::new(SourceHttpClientOptions {
            contact_email: None,
            request_timeout: config.request_timeout.unwrap_or(Duration::from_secs(30)),
            min_request_interval: config
                .min_request_interval
                .unwrap_or(ARXIV_MIN_REQUEST_INTERVAL),
            accept: None,
        })?;
        Ok(Self {
            descriptor: SourceDescriptor {
                id: "arxiv",
                display_name: "arXiv",
                adapter_version: 1,
                contract_version: SOURCE_ADAPTER_CONTRACT_VERSION,
                api_base: "https://export.arxiv.org/api/query",
                access_basis:
                    "public Atom API for modest rate-limited queries; direct OA PDF links",
                capabilities: &[Capability::Discovery, Capability::DirectPdfUrls],
                rate_policy: RatePolicy {
                    min_request_interval: ARXIV_MIN_REQUEST_INTERVAL,
                    default_page_size: 25,
                    max_page_size: 100,
                },
            },
            client,
            api_base: config
                .api_base
                .unwrap_or_else(|| ARXIV_DEFAULT_API_BASE.to_owned()),
        })
    }

    fn query_url(&self, text: &str, start: usize, max_results: u16) -> String {
        // Phrase search keeps results precise; the API otherwise OR-joins bare words.
        format!(
            "{}/api/query?search_query=all%3A%22{}%22&start={start}&max_results={max_results}",
            self.api_base,
            urlencode(text),
        )
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

/// One parsed Atom entry under construction.
#[derive(Default)]
struct EntryDraft {
    id: String,
    title: String,
    summary: String,
    published: String,
    updated: String,
    comment: Option<String>,
    doi: Option<String>,
    authors: Vec<String>,
    categories: Vec<String>,
    pdf_url: Option<String>,
    landing_url: Option<String>,
}

impl EntryDraft {
    fn into_record(self) -> Option<WorkRecord> {
        let arxiv_id = normalize_arxiv_id(&self.id)?;
        let mut record = WorkRecord::new("arxiv", arxiv_id.clone());
        record
            .identifiers
            .push(crate::sources::work_record::WorkIdentifier::new(
                IdentifierKind::Arxiv,
                arxiv_id,
            ));
        record.push_identifier(IdentifierKind::Doi, self.doi.clone());
        record.title = normalize_title(&self.title);
        record.authors = self
            .authors
            .iter()
            .map(|author| normalize_title(author))
            .collect();
        record.publication_year = self
            .published
            .get(..4)
            .and_then(|year| year.parse::<u64>().ok())
            .and_then(normalize_year);
        record.oa_pdf_url = self
            .pdf_url
            .as_deref()
            .map(|url| url.replacen("http://", "https://", 1));
        record.landing_url = self
            .landing_url
            .as_deref()
            .map(|url| url.replacen("http://", "https://", 1));
        record.raw = serde_json::json!({
            "id": self.id,
            "title": self.title,
            "summary": self.summary,
            "published": self.published,
            "updated": self.updated,
            "comment": self.comment,
            "doi": self.doi,
            "authors": self.authors,
            "categories": self.categories,
            "pdf_url": record.oa_pdf_url,
            "landing_url": record.landing_url,
        });
        Some(record)
    }
}

/// Parse an arXiv Atom feed into work records. Entries without a usable arXiv id are
/// skipped; malformed XML returns a typed error instead of panicking.
pub fn parse_arxiv_atom(feed: &str) -> Result<Vec<WorkRecord>, SourceError> {
    let mut reader = Reader::from_str(feed);
    reader.config_mut().trim_text(false);
    let mut records = Vec::new();
    let mut draft: Option<EntryDraft> = None;
    let mut text_target: Option<&'static str> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(start)) => {
                let name = start.name();
                let name = name.as_ref();
                match name {
                    b"entry" => draft = Some(EntryDraft::default()),
                    b"id" if draft.is_some() => text_target = Some("id"),
                    b"title" if draft.is_some() => text_target = Some("title"),
                    b"summary" if draft.is_some() => text_target = Some("summary"),
                    b"published" if draft.is_some() => text_target = Some("published"),
                    b"updated" if draft.is_some() => text_target = Some("updated"),
                    b"name" if draft.is_some() => text_target = Some("name"),
                    b"arxiv:doi" if draft.is_some() => text_target = Some("arxiv:doi"),
                    b"arxiv:comment" if draft.is_some() => text_target = Some("arxiv:comment"),
                    b"category" if draft.is_some() => {
                        capture_category(&start, draft.as_mut().expect("guarded"));
                    }
                    b"link" if draft.is_some() => {
                        capture_link(&start, draft.as_mut().expect("guarded"));
                    }
                    _ => {}
                }
            }
            // Atom feeds serialize <link .../> and <category .../> as self-closing
            // elements, which quick-xml reports as Empty rather than Start.
            Ok(Event::Empty(start)) => {
                if let Some(entry_draft) = draft.as_mut() {
                    let name = start.name();
                    let name = name.as_ref();
                    match name {
                        b"category" => capture_category(&start, entry_draft),
                        b"link" => capture_link(&start, entry_draft),
                        _ => {}
                    }
                }
            }
            Ok(Event::Text(text)) => {
                if let (Some(target), Some(draft)) = (text_target, draft.as_mut()) {
                    let value = text
                        .unescape()
                        .map_err(|error| SourceError::MalformedResponse {
                            detail: format!("undecodable text: {error}"),
                        })?
                        .to_string();
                    match target {
                        "id" => draft.id = value,
                        "title" => draft.title.push_str(&value),
                        "summary" => draft.summary.push_str(&value),
                        "published" => draft.published = value,
                        "updated" => draft.updated = value,
                        "name" => draft.authors.push(value),
                        "arxiv:doi" => draft.doi = normalize_doi(&value),
                        "arxiv:comment" => draft.comment = Some(value),
                        _ => {}
                    }
                }
            }
            Ok(Event::End(end)) => {
                let name = end.name();
                let name = name.as_ref();
                text_target = None;
                if name == b"entry" {
                    if let Some(finished) = draft.take() {
                        if let Some(record) = finished.into_record() {
                            records.push(record);
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => {
                return Err(SourceError::MalformedResponse {
                    detail: format!("invalid Atom XML: {error}"),
                })
            }
        }
    }
    Ok(records)
}

fn attr(start: &quick_xml::events::BytesStart<'_>, key: &[u8]) -> Option<String> {
    start
        .attributes()
        .flatten()
        .find(|attribute| attribute.key.as_ref() == key)
        .and_then(|attribute| attribute.unescape_value().ok())
        .map(|value| value.to_string())
}

fn capture_category(start: &quick_xml::events::BytesStart<'_>, draft: &mut EntryDraft) {
    if let Some(term) = attr(start, b"term") {
        draft.categories.push(term);
    }
}

fn capture_link(start: &quick_xml::events::BytesStart<'_>, draft: &mut EntryDraft) {
    let rel = attr(start, b"rel");
    let link_type = attr(start, b"type");
    let title = attr(start, b"title");
    if let Some(href) = attr(start, b"href") {
        match (rel.as_deref(), link_type.as_deref(), title.as_deref()) {
            (Some("alternate"), _, _) => draft.landing_url = Some(href),
            (Some("related"), Some("application/pdf"), _) | (_, _, Some("pdf")) => {
                draft.pdf_url = Some(href)
            }
            _ => {}
        }
    }
}

#[async_trait]
impl SourceAdapter for ArxivAdapter {
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
                "discovery via /api/query phrase search".to_owned(),
                "direct OA PDF links from entry link[rel=related][type=application/pdf]".to_owned(),
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
        let start: usize = cursor.and_then(|cursor| cursor.parse().ok()).unwrap_or(0);
        let url = self.query_url(&query.text, start, page_size);
        let body = self.client.get_text(&url).await?;
        let records = parse_arxiv_atom(&body)?;
        let next_cursor = if records.len() == page_size as usize {
            Some((start + records.len()).to_string())
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

    fn adapter() -> ArxivAdapter {
        ArxivAdapter::new(ArxivConfig {
            min_request_interval: Some(Duration::from_millis(0)),
            ..ArxivConfig::default()
        })
        .unwrap()
    }

    #[test]
    fn descriptor_declares_direct_pdf_capability() {
        let adapter = adapter();
        let descriptor = adapter.descriptor();
        assert_eq!(descriptor.id, "arxiv");
        assert!(descriptor.supports(Capability::Discovery));
        assert!(descriptor.supports(Capability::DirectPdfUrls));
        assert!(!descriptor.supports(Capability::DoiLookup));
        assert_eq!(
            descriptor.rate_policy.min_request_interval,
            ARXIV_MIN_REQUEST_INTERVAL
        );
    }

    #[test]
    fn parses_recorded_atom_feed() {
        let feed = include_str!("../../tests/fixtures/arxiv_search.xml");
        let records = parse_arxiv_atom(feed).unwrap();
        assert_eq!(records.len(), 2);

        let first = &records[0];
        assert_eq!(first.source_id, "arxiv");
        assert_eq!(first.source_record_id, "2105.02723v1");
        assert_eq!(first.arxiv_id(), Some("2105.02723v1"));
        assert!(first.title.contains("Do You Even Need Attention?"));
        assert_eq!(first.authors, vec!["Luke Melas-Kyriazi".to_owned()]);
        assert_eq!(first.publication_year, Some(2021));
        assert_eq!(
            first.oa_pdf_url.as_deref(),
            Some("https://arxiv.org/pdf/2105.02723v1")
        );
        assert_eq!(
            first.landing_url.as_deref(),
            Some("https://arxiv.org/abs/2105.02723v1")
        );
        assert!(first.raw["summary"]
            .as_str()
            .unwrap()
            .contains("vision transformers"));
        assert_eq!(first.raw["categories"][0], "cs.CV");

        let second = &records[1];
        assert_eq!(second.source_record_id, "2512.19700v1");
        assert_eq!(second.publication_year, Some(2025));
    }

    #[test]
    fn upgrades_http_pdf_links_to_https() {
        let feed = r#"
<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry>
    <id>http://arxiv.org/abs/2401.00001v1</id>
    <title>Test Entry</title>
    <published>2024-01-02T00:00:00Z</published>
    <link href="http://arxiv.org/abs/2401.00001v1" rel="alternate" type="text/html"/>
    <link href="http://arxiv.org/pdf/2401.00001v1" rel="related" type="application/pdf" title="pdf"/>
    <author><name>Some Author</name></author>
  </entry>
</feed>"#;
        let records = parse_arxiv_atom(feed).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].oa_pdf_url.as_deref(),
            Some("https://arxiv.org/pdf/2401.00001v1")
        );
    }

    #[test]
    fn entries_without_valid_ids_are_skipped() {
        let feed = r#"
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry>
    <id>not-an-arxiv-id</id>
    <title>Broken</title>
  </entry>
  <entry>
    <id>http://arxiv.org/abs/2401.00002v1</id>
    <title>Valid</title>
    <published>2024-01-02T00:00:00Z</published>
  </entry>
</feed>"#;
        let records = parse_arxiv_atom(feed).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].source_record_id, "2401.00002v1");
    }

    #[test]
    fn malformed_xml_is_a_typed_error() {
        let error = parse_arxiv_atom("<not-xml").unwrap_err();
        assert!(matches!(error, SourceError::MalformedResponse { .. }));
    }

    #[test]
    fn doi_and_comment_fields_are_captured() {
        let feed = r#"
<feed xmlns="http://www.w3.org/2005/Atom" xmlns:arxiv="http://arxiv.org/schemas/atom">
  <entry>
    <id>http://arxiv.org/abs/2401.00003v2</id>
    <title>With DOI</title>
    <published>2024-01-02T00:00:00Z</published>
    <arxiv:doi>10.1038/s41586-020-2649-2</arxiv:doi>
    <arxiv:comment>12 pages</arxiv:comment>
    <author><name>First Author</name></author>
    <author><name>Second Author</name></author>
  </entry>
</feed>"#;
        let records = parse_arxiv_atom(feed).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].doi(),
            Some("10.1038/s41586-020-2649-2"),
            "arxiv:doi must be normalized through the DOI canonicalizer"
        );
        assert_eq!(records[0].authors.len(), 2);
        assert_eq!(records[0].raw["comment"], "12 pages");
    }
}
