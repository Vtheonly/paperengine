//! Generic OAI-PMH harvester adapter (#13).
//!
//! Implements the Open Archives Initiative Protocol for Metadata Harvesting 2.0
//! (`ListRecords` with `resumptionToken` continuation, `from`/`until` windows,
//! optional `set` specs, `ListSets` with bounded continuation) behind the versioned
//! source contract. Two metadata formats are supported: `oai_dc` (Dublin Core) and
//! the `arXiv` raw format. One adapter class serves every repository family that
//! documents an OAI-PMH endpoint; instances are registered per repository:
//! `zenodo-oaipmh` (`https://zenodo.org/oai2d`), `doaj-oaipmh`
//! (`https://doaj.org/oai`), `pmc-oaipmh`
//! (`https://pmc.ncbi.nlm.nih.gov/api/oai/v1/mh/` — the modern location of the
//! legacy `ncbi.nlm.nih.gov/pmc/oai/oai.cgi`, which 301-redirects), and
//! `arxiv-oaipmh` (`https://oaipmh.arxiv.org/oai` — the redirect target of the
//! legacy `export.arxiv.org/oai2`).
//!
//! OAI-PMH is a harvesting protocol, not a search interface. `discover` accepts a
//! harvest window in either form: an explicit `from_date`/`until_date`/`set` on the
//! query, or a parseable ISO date in `query.text` (the historical form). A
//! non-empty `query.text` that is not a valid ISO date is refused with a typed
//! configuration error — it is never silently ignored. Following a resumption token
//! uses the exclusive `verb`+`resumptionToken` form required by OAI-PMH 2.0 §2.1,
//! with the token percent-encoded.
//!
//! Deleted records (`<header status="deleted">`) are parsed, counted, and excluded
//! from work records — a repository tombstone is never turned into a fabricated
//! work. The per-page deleted count and the raw parsed records are exposed through
//! [`OaiPmhAdapter::harvest_page`] for the future incremental registry ingest
//! (#9/#32); the trait-level `discover` returns live work records only.
//!
//! Neither `oai_dc` nor the `arXiv` raw format carries direct full-text URLs, so
//! these adapters never claim a PDF link: `oa_pdf_url` is never set. `oai_dc`
//! records are filtered by the paper-only policy (`dc:type` substring test,
//! containers like `journal` stay out); the `arXiv` format has no type field —
//! every record is a preprint — so the type policy is not applied to it.

use crate::sources::contract::{
    Capability, DiscoveryPage, DiscoveryQuery, RatePolicy, SelfTestReport, SourceAdapter,
    SourceDescriptor, SourceError, SOURCE_ADAPTER_CONTRACT_VERSION,
};
use crate::sources::http::{SourceHttpClient, SourceHttpClientOptions};
use crate::sources::normalize::{
    normalize_arxiv_id, normalize_doi, normalize_pmcid, normalize_title, normalize_year,
};
use crate::sources::work_record::{IdentifierKind, WorkRecord};
use async_trait::async_trait;
use quick_xml::events::Event;
use quick_xml::Reader;
use std::time::Duration;

/// Dublin Core metadata prefix.
pub const OAI_DC: &str = "oai_dc";
/// arXiv raw metadata format prefix.
pub const ARXIV_RAW: &str = "arXiv";

/// Default minimum spacing between OAI-PMH requests.
pub const OAIPMH_MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(1_500);
/// arXiv's documented courtesy interval, applied to its OAI-PMH interface as well.
pub const ARXIV_OAI_MIN_REQUEST_INTERVAL: Duration = Duration::from_secs(3);

/// Upper bound on `list_sets` continuation requests before the run is stopped with a
/// typed budget error. Set lists are short at every registered interface; the cap only
/// guards against a server that never ends its pagination.
const MAX_LIST_SETS_REQUESTS: usize = 20;

// ---------------------------------------------------------------------------
// URL building and argument encoding
// ---------------------------------------------------------------------------

/// Percent-encode a query value: unreserved characters pass through, everything
/// else is `%XX`-encoded. OAI-PMH resumption tokens are opaque and may contain
/// `=`, `:`, `%`, or `&`, so they must be encoded.
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

/// Build a `ListRecords` URL. When a resumption token is present it is the *only*
/// argument besides `verb` (OAI-PMH 2.0 §2.1: the other arguments must not be
/// included with a resumptionToken); otherwise the selective-harvest arguments are
/// appended. Values are percent-encoded.
pub fn build_list_records_url(
    base: &str,
    metadata_prefix: &str,
    from: Option<&str>,
    until: Option<&str>,
    set: Option<&str>,
    token: Option<&str>,
) -> String {
    match token {
        Some(token) if !token.is_empty() => {
            format!(
                "{base}?verb=ListRecords&resumptionToken={}",
                urlencode(token)
            )
        }
        _ => {
            let mut url = format!(
                "{base}?verb=ListRecords&metadataPrefix={}",
                urlencode(metadata_prefix)
            );
            if let Some(from) = from {
                url.push_str("&from=");
                url.push_str(&urlencode(from));
            }
            if let Some(until) = until {
                url.push_str("&until=");
                url.push_str(&urlencode(until));
            }
            if let Some(set) = set {
                url.push_str("&set=");
                url.push_str(&urlencode(set));
            }
            url
        }
    }
}

/// Build a `ListSets` URL, with the resumption-token continuation form.
fn build_list_sets_url(base: &str, token: Option<&str>) -> String {
    match token {
        Some(token) if !token.is_empty() => {
            format!("{base}?verb=ListSets&resumptionToken={}", urlencode(token))
        }
        _ => format!("{base}?verb=ListSets"),
    }
}

// ---------------------------------------------------------------------------
// Selective-harvest date validation
// ---------------------------------------------------------------------------

fn is_digits(slice: &[u8]) -> bool {
    !slice.is_empty() && slice.iter().all(|byte| byte.is_ascii_digit())
}

fn is_utc_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && is_digits(&bytes[0..4])
        && is_digits(&bytes[5..7])
        && is_digits(&bytes[8..10])
}

fn is_utc_datetime(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 20
        && is_utc_date(&value[..10])
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'Z'
        && is_digits(&bytes[11..13])
        && is_digits(&bytes[14..16])
        && is_digits(&bytes[17..19])
}

/// Validate one `from`/`until` value. Repositories advertise either day or
/// second granularity; both shapes are accepted here and the repository enforces
/// its own granularity server-side (a mismatch surfaces as a typed `badArgument`).
fn validate_harvest_date(value: &str, field: &str) -> Result<(), SourceError> {
    if is_utc_date(value) || is_utc_datetime(value) {
        Ok(())
    } else {
        Err(SourceError::InvalidConfiguration {
            detail: format!(
                "{field} must be `YYYY-MM-DD` or `YYYY-MM-DDThh:mm:ssZ` \
                 (matching the repository's advertised granularity); got `{value}`"
            ),
        })
    }
}

/// Interpret a discovery query text as a `from` window: a bare YYYY-MM-DD or a
/// full UTC ISO datetime. Anything else is not a date and yields `None`.
pub fn parse_from(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if is_utc_date(trimmed) {
        return Some(trimmed.to_owned());
    }
    if is_utc_datetime(trimmed) {
        return Some(trimmed.to_owned());
    }
    None
}

// ---------------------------------------------------------------------------
// Parsed page model (protocol view)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OaiRecordHeader {
    pub identifier: String,
    pub datestamp: String,
    pub set_specs: Vec<String>,
    pub deleted: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OaiArxivAuthor {
    pub keyname: String,
    pub forenames: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DublinCoreRecord {
    pub title: String,
    pub creators: Vec<String>,
    pub subjects: Vec<String>,
    pub description: String,
    pub publishers: Vec<String>,
    pub dates: Vec<String>,
    pub types: Vec<String>,
    pub identifiers: Vec<String>,
    pub relations: Vec<String>,
    pub sources: Vec<String>,
    pub rights: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArxivRawRecord {
    pub id: String,
    pub created: Option<String>,
    pub updated: Option<String>,
    pub authors: Vec<OaiArxivAuthor>,
    pub title: String,
    pub categories: Option<String>,
    pub comments: Option<String>,
    pub license: Option<String>,
    pub abstract_text: String,
    pub doi: Option<String>,
    pub journal_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OaiMetadata {
    DublinCore(DublinCoreRecord),
    ArxivRaw(ArxivRawRecord),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OaiRecord {
    pub header: OaiRecordHeader,
    pub metadata: Option<OaiMetadata>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OaiListRecordsPage {
    /// All parsed records, including deleted ones (marked `header.deleted`).
    pub records: Vec<OaiRecord>,
    pub deleted_count: usize,
    pub resumption_token: Option<String>,
    pub complete_list_size: Option<u64>,
}

impl OaiListRecordsPage {
    pub fn empty() -> Self {
        Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OaiSet {
    pub spec: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OaiListSetsPage {
    pub sets: Vec<OaiSet>,
    pub resumption_token: Option<String>,
}

// ---------------------------------------------------------------------------
// XML parsing
// ---------------------------------------------------------------------------

/// Local (prefix-stripped) tag name: `dc:title` -> `title`, `header` -> `header`.
/// Repositories vary in their choice of namespace prefixes; matching local names
/// keeps the parser independent of that choice.
fn local_name(name: &[u8]) -> &[u8] {
    match name.iter().position(|byte| *byte == b':') {
        Some(index) => &name[index + 1..],
        None => name,
    }
}

/// True when the element carries `status="deleted"` among its attributes, at any
/// position (recorded DOAJ headers place it after several `xmlns` attributes).
fn has_deleted_status(start: &quick_xml::events::BytesStart<'_>) -> bool {
    start.attributes().flatten().any(|attribute| {
        attribute.key.as_ref() == b"status"
            && attribute
                .unescape_value()
                .map(|value| value.as_ref() == "deleted")
                .unwrap_or(false)
    })
}

/// Read the `completeListSize` resumption-token attribute when present.
fn complete_list_size(start: &quick_xml::events::BytesStart<'_>) -> Option<u64> {
    start
        .attributes()
        .flatten()
        .find(|attribute| attribute.key.as_ref() == b"completeListSize")
        .and_then(|attribute| attribute.unescape_value().ok())
        .and_then(|value| value.parse::<u64>().ok())
}

/// Map a top-level OAI-PMH `<error>` to the typed error taxonomy. `noRecordsMatch`
/// is a legitimate empty result, not a failure, and is handled by the callers.
fn oai_error(code: &str, message: &str) -> SourceError {
    SourceError::MalformedResponse {
        detail: format!("OAI-PMH error {code}: {message}"),
    }
}

/// Parse a `ListRecords` response for the given expected metadata prefix. Deleted
/// records are returned in `records` with `header.deleted = true` and no metadata;
/// `deleted_count` counts them. An OAI error element maps to a typed error, except
/// `noRecordsMatch`, which yields an empty page. A document without an OAI-PMH
/// envelope or without a `ListRecords` container is a typed malformed response.
pub fn parse_oai_list_records(
    body: &str,
    expected_prefix: &str,
) -> Result<OaiListRecordsPage, SourceError> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);

    let mut page = OaiListRecordsPage::default();
    let mut saw_envelope = false;
    let mut saw_listrecords = false;
    let mut error: Option<(String, String)> = None;
    let mut text_target: Option<&'static str> = None;

    let mut header: Option<OaiRecordHeader> = None;
    let mut metadata: Option<OaiMetadata> = None;
    let mut inside_metadata_element = false;
    let mut in_header_element = false;
    let mut in_metadata_format = false;
    let mut current_author: Option<OaiArxivAuthor> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(start)) => {
                let name = local_name(start.name().as_ref()).to_vec();
                match name.as_slice() {
                    b"OAI-PMH" => saw_envelope = true,
                    b"ListRecords" => saw_listrecords = true,
                    b"record" => {
                        header = Some(OaiRecordHeader::default());
                        metadata = None;
                        current_author = None;
                    }
                    b"header" => {
                        if let Some(header) = header.as_mut() {
                            header.deleted = has_deleted_status(&start);
                        }
                        in_header_element = true;
                    }
                    b"identifier" if in_header_element && header.is_some() => {
                        text_target = Some("header_identifier")
                    }
                    b"datestamp" if in_header_element && header.is_some() => {
                        text_target = Some("header_datestamp")
                    }
                    b"setSpec" if in_header_element && header.is_some() => {
                        text_target = Some("header_setspec")
                    }
                    b"metadata" if header.is_some() => inside_metadata_element = true,
                    b"dc" if inside_metadata_element && expected_prefix == OAI_DC => {
                        metadata = Some(OaiMetadata::DublinCore(DublinCoreRecord::default()));
                        in_metadata_format = true;
                    }
                    b"arXiv" if inside_metadata_element && expected_prefix == ARXIV_RAW => {
                        metadata = Some(OaiMetadata::ArxivRaw(ArxivRawRecord::default()));
                        in_metadata_format = true;
                    }

                    // Dublin Core fields (inside <oai_dc:dc>).
                    b"title" if in_metadata_format && expected_prefix == OAI_DC => {
                        text_target = Some("dc_title")
                    }
                    b"creator" if in_metadata_format && expected_prefix == OAI_DC => {
                        text_target = Some("dc_creator")
                    }
                    b"subject" if in_metadata_format && expected_prefix == OAI_DC => {
                        text_target = Some("dc_subject")
                    }
                    b"description" if in_metadata_format && expected_prefix == OAI_DC => {
                        text_target = Some("dc_description")
                    }
                    b"publisher" if in_metadata_format && expected_prefix == OAI_DC => {
                        text_target = Some("dc_publisher")
                    }
                    b"date" if in_metadata_format && expected_prefix == OAI_DC => {
                        text_target = Some("dc_date")
                    }
                    b"type" if in_metadata_format && expected_prefix == OAI_DC => {
                        text_target = Some("dc_type")
                    }
                    b"identifier" if in_metadata_format && expected_prefix == OAI_DC => {
                        text_target = Some("dc_identifier")
                    }
                    b"relation" if in_metadata_format && expected_prefix == OAI_DC => {
                        text_target = Some("dc_relation")
                    }
                    b"source" if in_metadata_format && expected_prefix == OAI_DC => {
                        text_target = Some("dc_source")
                    }
                    b"rights" if in_metadata_format && expected_prefix == OAI_DC => {
                        text_target = Some("dc_rights")
                    }

                    // arXiv raw-format fields (inside <arXiv>).
                    b"id" if in_metadata_format && expected_prefix == ARXIV_RAW => {
                        text_target = Some("arxiv_id")
                    }
                    b"title" if in_metadata_format && expected_prefix == ARXIV_RAW => {
                        text_target = Some("arxiv_title")
                    }
                    b"created" if in_metadata_format && expected_prefix == ARXIV_RAW => {
                        text_target = Some("arxiv_created")
                    }
                    b"updated" if in_metadata_format && expected_prefix == ARXIV_RAW => {
                        text_target = Some("arxiv_updated")
                    }
                    b"categories" if in_metadata_format && expected_prefix == ARXIV_RAW => {
                        text_target = Some("arxiv_categories")
                    }
                    b"comments" if in_metadata_format && expected_prefix == ARXIV_RAW => {
                        text_target = Some("arxiv_comments")
                    }
                    b"license" if in_metadata_format && expected_prefix == ARXIV_RAW => {
                        text_target = Some("arxiv_license")
                    }
                    b"abstract" if in_metadata_format && expected_prefix == ARXIV_RAW => {
                        text_target = Some("arxiv_abstract")
                    }
                    b"doi" if in_metadata_format && expected_prefix == ARXIV_RAW => {
                        text_target = Some("arxiv_doi")
                    }
                    b"journal-ref" if in_metadata_format && expected_prefix == ARXIV_RAW => {
                        text_target = Some("arxiv_journal_ref")
                    }
                    b"author" if in_metadata_format && expected_prefix == ARXIV_RAW => {
                        current_author = Some(OaiArxivAuthor::default());
                    }
                    b"keyname" if current_author.is_some() => text_target = Some("arxiv_keyname"),
                    b"forenames" if current_author.is_some() => {
                        text_target = Some("arxiv_forenames")
                    }

                    b"resumptionToken" if header.is_none() => {
                        page.complete_list_size = complete_list_size(&start);
                        text_target = Some("resumption_token");
                    }
                    b"error" if header.is_none() => {
                        let code = start
                            .attributes()
                            .flatten()
                            .find(|attribute| attribute.key.as_ref() == b"code")
                            .and_then(|attribute| attribute.unescape_value().ok())
                            .map(|value| value.to_string());
                        error = Some((code.unwrap_or_default(), String::new()));
                        text_target = Some("error_message");
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(start)) => {
                // `<resumptionToken ... />` with no text: the spec's end-of-list marker.
                if local_name(start.name().as_ref()) == b"resumptionToken" && header.is_none() {
                    page.complete_list_size = complete_list_size(&start);
                }
            }
            Ok(Event::Text(text)) => {
                let value = match text.unescape() {
                    Ok(value) => value.trim().to_string(),
                    Err(error) => {
                        return Err(SourceError::MalformedResponse {
                            detail: format!("undecodable text: {error}"),
                        })
                    }
                };
                if value.is_empty() {
                    continue;
                }
                match text_target {
                    Some("header_identifier") => {
                        if let Some(header) = header.as_mut() {
                            header.identifier = value;
                        }
                    }
                    Some("header_datestamp") => {
                        if let Some(header) = header.as_mut() {
                            header.datestamp = value;
                        }
                    }
                    Some("header_setspec") => {
                        if let Some(header) = header.as_mut() {
                            header.set_specs.push(value);
                        }
                    }
                    Some("resumption_token") => page.resumption_token = Some(value),
                    Some("error_message") => {
                        if let Some((_, message)) = error.as_mut() {
                            message.push_str(&value);
                        }
                    }
                    Some("dc_title") => {
                        if let Some(OaiMetadata::DublinCore(dc)) = metadata.as_mut() {
                            dc.title = value;
                        }
                    }
                    Some("dc_creator") => {
                        if let Some(OaiMetadata::DublinCore(dc)) = metadata.as_mut() {
                            dc.creators.push(value);
                        }
                    }
                    Some("dc_subject") => {
                        if let Some(OaiMetadata::DublinCore(dc)) = metadata.as_mut() {
                            dc.subjects.push(value);
                        }
                    }
                    Some("dc_description") => {
                        if let Some(OaiMetadata::DublinCore(dc)) = metadata.as_mut() {
                            dc.description = value;
                        }
                    }
                    Some("dc_publisher") => {
                        if let Some(OaiMetadata::DublinCore(dc)) = metadata.as_mut() {
                            dc.publishers.push(value);
                        }
                    }
                    Some("dc_date") => {
                        if let Some(OaiMetadata::DublinCore(dc)) = metadata.as_mut() {
                            dc.dates.push(value);
                        }
                    }
                    Some("dc_type") => {
                        if let Some(OaiMetadata::DublinCore(dc)) = metadata.as_mut() {
                            dc.types.push(value);
                        }
                    }
                    Some("dc_identifier") => {
                        if let Some(OaiMetadata::DublinCore(dc)) = metadata.as_mut() {
                            dc.identifiers.push(value);
                        }
                    }
                    Some("dc_relation") => {
                        if let Some(OaiMetadata::DublinCore(dc)) = metadata.as_mut() {
                            dc.relations.push(value);
                        }
                    }
                    Some("dc_source") => {
                        if let Some(OaiMetadata::DublinCore(dc)) = metadata.as_mut() {
                            dc.sources.push(value);
                        }
                    }
                    Some("dc_rights") => {
                        if let Some(OaiMetadata::DublinCore(dc)) = metadata.as_mut() {
                            dc.rights.push(value);
                        }
                    }
                    Some("arxiv_id") => {
                        if let Some(OaiMetadata::ArxivRaw(arxiv)) = metadata.as_mut() {
                            arxiv.id = value;
                        }
                    }
                    Some("arxiv_created") => {
                        if let Some(OaiMetadata::ArxivRaw(arxiv)) = metadata.as_mut() {
                            arxiv.created = Some(value);
                        }
                    }
                    Some("arxiv_updated") => {
                        if let Some(OaiMetadata::ArxivRaw(arxiv)) = metadata.as_mut() {
                            arxiv.updated = Some(value);
                        }
                    }
                    Some("arxiv_title") => {
                        if let Some(OaiMetadata::ArxivRaw(arxiv)) = metadata.as_mut() {
                            arxiv.title = value;
                        }
                    }
                    Some("arxiv_categories") => {
                        if let Some(OaiMetadata::ArxivRaw(arxiv)) = metadata.as_mut() {
                            arxiv.categories = Some(value);
                        }
                    }
                    Some("arxiv_comments") => {
                        if let Some(OaiMetadata::ArxivRaw(arxiv)) = metadata.as_mut() {
                            arxiv.comments = Some(value);
                        }
                    }
                    Some("arxiv_license") => {
                        if let Some(OaiMetadata::ArxivRaw(arxiv)) = metadata.as_mut() {
                            arxiv.license = Some(value);
                        }
                    }
                    Some("arxiv_abstract") => {
                        if let Some(OaiMetadata::ArxivRaw(arxiv)) = metadata.as_mut() {
                            arxiv.abstract_text = value;
                        }
                    }
                    Some("arxiv_doi") => {
                        if let Some(OaiMetadata::ArxivRaw(arxiv)) = metadata.as_mut() {
                            arxiv.doi = Some(value);
                        }
                    }
                    Some("arxiv_journal_ref") => {
                        if let Some(OaiMetadata::ArxivRaw(arxiv)) = metadata.as_mut() {
                            arxiv.journal_ref = Some(value);
                        }
                    }
                    Some("arxiv_keyname") => {
                        if let Some(author) = current_author.as_mut() {
                            author.keyname = value;
                        }
                    }
                    Some("arxiv_forenames") => {
                        if let Some(author) = current_author.as_mut() {
                            author.forenames = value;
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(end)) => {
                let name = local_name(end.name().as_ref()).to_vec();
                text_target = None;
                match name.as_slice() {
                    b"record" => {
                        if let Some(finished_header) = header.take() {
                            if finished_header.deleted {
                                page.deleted_count += 1;
                            }
                            page.records.push(OaiRecord {
                                header: finished_header,
                                metadata: metadata.take(),
                            });
                        }
                    }
                    b"author" => {
                        if let Some(finished_author) = current_author.take() {
                            if let Some(OaiMetadata::ArxivRaw(arxiv)) = metadata.as_mut() {
                                arxiv.authors.push(finished_author);
                            }
                        }
                    }
                    b"header" => in_header_element = false,
                    b"metadata" => inside_metadata_element = false,
                    b"dc" | b"arXiv" => in_metadata_format = false,
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(parse_error) => {
                return Err(SourceError::MalformedResponse {
                    detail: format!("invalid OAI-PMH XML: {parse_error}"),
                })
            }
        }
    }

    if let Some((code, message)) = error {
        if code == "noRecordsMatch" {
            return Ok(OaiListRecordsPage::empty());
        }
        return Err(oai_error(&code, &message));
    }
    if header.is_some() {
        return Err(SourceError::MalformedResponse {
            detail: "unterminated <record> element".to_owned(),
        });
    }
    if !saw_envelope {
        return Err(SourceError::MalformedResponse {
            detail: "document is not an OAI-PMH envelope".to_owned(),
        });
    }
    if !saw_listrecords {
        return Err(SourceError::MalformedResponse {
            detail: "response contains neither a ListRecords container nor an OAI-PMH error"
                .to_owned(),
        });
    }
    Ok(page)
}

/// Parse a `ListSets` response, following the same error mapping as
/// [`parse_oai_list_records`].
pub fn parse_oai_list_sets(body: &str) -> Result<OaiListSetsPage, SourceError> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);

    let mut page = OaiListSetsPage::default();
    let mut saw_envelope = false;
    let mut saw_listsets = false;
    let mut error: Option<(String, String)> = None;
    let mut text_target: Option<&'static str> = None;
    let mut current_set: Option<OaiSet> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(start)) => {
                let name = local_name(start.name().as_ref()).to_vec();
                match name.as_slice() {
                    b"OAI-PMH" => saw_envelope = true,
                    b"ListSets" => saw_listsets = true,
                    b"set" => {
                        current_set = Some(OaiSet {
                            spec: String::new(),
                            name: None,
                        })
                    }
                    b"setSpec" if current_set.is_some() => text_target = Some("set_spec"),
                    b"setName" if current_set.is_some() => text_target = Some("set_name"),
                    b"resumptionToken" => text_target = Some("sets_resumption_token"),
                    b"error" => {
                        let code = start
                            .attributes()
                            .flatten()
                            .find(|attribute| attribute.key.as_ref() == b"code")
                            .and_then(|attribute| attribute.unescape_value().ok())
                            .map(|value| value.to_string());
                        error = Some((code.unwrap_or_default(), String::new()));
                        text_target = Some("error_message");
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(text)) => {
                let value = match text.unescape() {
                    Ok(value) => value.trim().to_string(),
                    Err(error) => {
                        return Err(SourceError::MalformedResponse {
                            detail: format!("undecodable text: {error}"),
                        })
                    }
                };
                if value.is_empty() {
                    continue;
                }
                match text_target {
                    Some("set_spec") => {
                        if let Some(set) = current_set.as_mut() {
                            set.spec = value;
                        }
                    }
                    Some("set_name") => {
                        if let Some(set) = current_set.as_mut() {
                            set.name = Some(value);
                        }
                    }
                    Some("sets_resumption_token") => page.resumption_token = Some(value),
                    Some("error_message") => {
                        if let Some((_, message)) = error.as_mut() {
                            message.push_str(&value);
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(end)) => {
                let name = local_name(end.name().as_ref()).to_vec();
                text_target = None;
                if name == b"set" {
                    if let Some(finished_set) = current_set.take() {
                        if !finished_set.spec.is_empty() {
                            page.sets.push(finished_set);
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(parse_error) => {
                return Err(SourceError::MalformedResponse {
                    detail: format!("invalid OAI-PMH XML: {parse_error}"),
                })
            }
        }
    }

    if let Some((code, message)) = error {
        if code == "noRecordsMatch" {
            return Ok(OaiListSetsPage::default());
        }
        return Err(oai_error(&code, &message));
    }
    if !saw_envelope {
        return Err(SourceError::MalformedResponse {
            detail: "document is not an OAI-PMH envelope".to_owned(),
        });
    }
    if !saw_listsets {
        return Err(SourceError::MalformedResponse {
            detail: "response contains neither a ListSets container nor an OAI-PMH error"
                .to_owned(),
        });
    }
    Ok(page)
}

// ---------------------------------------------------------------------------
// Identifier extraction helpers
// ---------------------------------------------------------------------------

/// Extract a normalized PMCID from article-URL identifier values
/// (`…/articles/PMC12345/…` on any host). Other values return `None`.
pub fn pmcid_from_article_url(value: &str) -> Option<String> {
    value
        .split_once("/articles/")
        .and_then(|(_, tail)| tail.split('/').next())
        .and_then(normalize_pmcid)
}

// ---------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct OaiPmhConfig {
    /// Registry id, e.g. `zenodo-oaipmh`.
    pub source_id: &'static str,
    pub display_name: &'static str,
    pub access_basis: &'static str,
    /// OAI-PMH base URL, e.g. `https://zenodo.org/oai2d`.
    pub api_base: String,
    /// Metadata prefix this adapter harvests: `oai_dc` or `arXiv`.
    pub metadata_prefix: String,
    pub request_timeout: Option<Duration>,
    pub min_request_interval: Option<Duration>,
    /// Keep only paper-like dc:type values (default true, #10). Not applied to the
    /// `arXiv` format, which has no type field — every record is a preprint.
    pub paper_only: bool,
    /// Optional OAI set spec used when the query does not carry one.
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
            metadata_prefix: OAI_DC.into(),
            request_timeout: None,
            min_request_interval: None,
            paper_only: true,
            set: None,
        }
    }

    /// Configuration for the DOAJ OAI-PMH instance. DOAJ's interface disseminates
    /// journal records only (verified live); the paper-only policy therefore maps
    /// its harvests to zero work records — recorded honestly in the registry.
    pub fn doaj() -> Self {
        Self {
            source_id: "doaj-oaipmh",
            display_name: "DOAJ (OAI-PMH)",
            access_basis: "public OAI-PMH 2.0 endpoint; disseminates journal records only",
            api_base: "https://doaj.org/oai".into(),
            metadata_prefix: OAI_DC.into(),
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
            access_basis: "public OAI-PMH 2.0 endpoint over the PMC open-access subset",
            api_base: "https://pmc.ncbi.nlm.nih.gov/api/oai/v1/mh/".into(),
            metadata_prefix: OAI_DC.into(),
            request_timeout: None,
            min_request_interval: None,
            paper_only: true,
            set: None,
        }
    }

    /// Configuration for the arXiv OAI-PMH instance, harvesting the `arXiv` raw
    /// metadata format. The legacy `export.arxiv.org/oai2` endpoint 301-redirects
    /// to this base; the modern URL is used directly. arXiv's 3 s courtesy
    /// interval is applied. The raw format carries no links of any kind: this
    /// instance is metadata-only and the Atom API adapter remains the arXiv
    /// acquisition route.
    pub fn arxiv() -> Self {
        Self {
            source_id: "arxiv-oaipmh",
            display_name: "arXiv (OAI-PMH)",
            access_basis: "public OAI-PMH 2.0 endpoint; arXiv raw metadata format (preprints)",
            api_base: "https://oaipmh.arxiv.org/oai".into(),
            metadata_prefix: ARXIV_RAW.into(),
            request_timeout: None,
            min_request_interval: Some(ARXIV_OAI_MIN_REQUEST_INTERVAL),
            paper_only: true,
            set: None,
        }
    }
}

#[derive(Debug)]
pub struct OaiPmhAdapter {
    descriptor: SourceDescriptor,
    client: SourceHttpClient,
    api_base: String,
    metadata_prefix: String,
    paper_only: bool,
    set: Option<String>,
}

impl OaiPmhAdapter {
    pub fn new(config: OaiPmhConfig) -> Result<Self, SourceError> {
        if config.metadata_prefix != OAI_DC && config.metadata_prefix != ARXIV_RAW {
            return Err(SourceError::InvalidConfiguration {
                detail: format!(
                    "metadataPrefix must be `{OAI_DC}` or `{ARXIV_RAW}`; got `{}`",
                    config.metadata_prefix
                ),
            });
        }
        let interval = config
            .min_request_interval
            .unwrap_or(OAIPMH_MIN_REQUEST_INTERVAL);
        let client = SourceHttpClient::new(SourceHttpClientOptions {
            request_timeout: config.request_timeout.unwrap_or(Duration::from_secs(60)),
            min_request_interval: interval,
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
                    min_request_interval: interval,
                    // OAI-PMH has no client page-size parameter; the engine's
                    // record budget still bounds every run.
                    default_page_size: 0,
                    max_page_size: 0,
                },
            },
            client,
            api_base: config.api_base,
            metadata_prefix: config.metadata_prefix,
            paper_only: config.paper_only,
            set: config.set,
        })
    }

    fn list_records_url(
        &self,
        from: Option<&str>,
        until: Option<&str>,
        set: Option<&str>,
        token: Option<&str>,
    ) -> String {
        build_list_records_url(
            &self.api_base,
            &self.metadata_prefix,
            from,
            until,
            set,
            token,
        )
    }

    /// Paper-like dc:type test: Dublin Core types are free text across
    /// repositories (`Article`, `Text`, `info:eu-repo/semantics/article`,
    /// `journal`, …). Containers and non-paper objects stay out.
    fn accepts_type(&self, types: &[String]) -> bool {
        if !self.paper_only {
            return true;
        }
        types.iter().any(|kind| {
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

    /// Map one parsed record to a work record. Deleted records, empty identifiers,
    /// and (under the default policy) non-paper types return `None`.
    fn parse_record(&self, parsed: &OaiRecord) -> Option<WorkRecord> {
        if parsed.header.deleted || parsed.header.identifier.is_empty() {
            return None;
        }
        match parsed.metadata.as_ref()? {
            OaiMetadata::DublinCore(dc) => {
                if !self.accepts_type(&dc.types) {
                    return None;
                }
                let mut record =
                    WorkRecord::new(self.descriptor.id, parsed.header.identifier.clone());
                record.title = normalize_title(&dc.title);
                record.authors = dc
                    .creators
                    .iter()
                    .map(|creator| normalize_title(creator))
                    .collect();
                record.publication_year = dc
                    .dates
                    .iter()
                    .filter_map(|date| date.trim().get(..4).and_then(|y| y.parse::<u64>().ok()))
                    .find_map(normalize_year);
                record.venue = dc
                    .sources
                    .first()
                    .or_else(|| dc.publishers.first())
                    .map(|source| source.to_owned());
                record.license = dc.rights.first().map(|rights| rights.to_owned());
                for value in &dc.identifiers {
                    // DOI forms: bare, `doi:` URN, and https://doi.org/ URLs.
                    if let Some(doi) = normalize_doi(value) {
                        if record.doi().is_none() {
                            record.push_identifier(IdentifierKind::Doi, Some(doi));
                        }
                        continue;
                    }
                    // PMC article URLs carry the accession number.
                    if let Some(pmcid) = pmcid_from_article_url(value) {
                        record.push_identifier(IdentifierKind::Pmcid, Some(pmcid));
                        continue;
                    }
                    // The first plain https identifier becomes the landing page.
                    if record.landing_url.is_none() && value.starts_with("https://") {
                        record.landing_url = Some(value.clone());
                    }
                }
                if record.landing_url.is_none() {
                    if let Some(landing) = dc
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
                // Dublin Core carries no reliable full-text URLs: never set
                // oa_pdf_url.
                record.oa_pdf_url = None;
                record.raw = serde_json::json!({
                    "oai_identifier": parsed.header.identifier,
                    "datestamp": parsed.header.datestamp,
                    "set_specs": parsed.header.set_specs,
                    "deleted": parsed.header.deleted,
                    "dc": {
                        "title": dc.title,
                        "creator": dc.creators,
                        "date": dc.dates,
                        "type": dc.types,
                        "publisher": dc.publishers,
                        "source": dc.sources,
                        "subject": dc.subjects,
                        "identifier": dc.identifiers,
                        "relation": dc.relations,
                        "rights": dc.rights,
                        "description": dc.description,
                    },
                });
                Some(record)
            }
            OaiMetadata::ArxivRaw(arxiv) => {
                let arxiv_id = normalize_arxiv_id(&arxiv.id)?;
                let mut record =
                    WorkRecord::new(self.descriptor.id, parsed.header.identifier.clone());
                record.push_identifier(IdentifierKind::Arxiv, Some(arxiv_id));
                record.push_identifier(IdentifierKind::Doi, arxiv.doi.clone());
                record.title = normalize_title(&arxiv.title);
                record.authors = arxiv
                    .authors
                    .iter()
                    .map(|author| {
                        normalize_title(format!("{} {}", author.forenames, author.keyname).trim())
                    })
                    .collect();
                record.publication_year = arxiv
                    .created
                    .as_deref()
                    .and_then(|created| created.get(..4))
                    .and_then(|year| year.parse::<u64>().ok())
                    .and_then(normalize_year);
                record.license = arxiv.license.clone();
                // The arXiv raw format carries no links of any kind: neither
                // landing nor PDF.
                record.oa_pdf_url = None;
                record.landing_url = None;
                record.raw = serde_json::json!({
                    "oai_identifier": parsed.header.identifier,
                    "datestamp": parsed.header.datestamp,
                    "set_specs": parsed.header.set_specs,
                    "deleted": parsed.header.deleted,
                    "arxiv": {
                        "id": arxiv.id,
                        "created": arxiv.created,
                        "updated": arxiv.updated,
                        "authors": arxiv.authors,
                        "title": arxiv.title,
                        "categories": arxiv.categories,
                        "comments": arxiv.comments,
                        "license": arxiv.license,
                        "abstract": arxiv.abstract_text,
                        "doi": arxiv.doi,
                        "journal_ref": arxiv.journal_ref,
                    },
                });
                Some(record)
            }
        }
    }

    /// Protocol-level view of one harvest page: raw parsed records (including
    /// deleted ones), the deleted count, and the resumption token. This is the
    /// surface a future incremental registry ingest (#9/#32) consumes.
    pub async fn harvest_page(
        &self,
        from: Option<&str>,
        until: Option<&str>,
        set: Option<&str>,
        token: Option<&str>,
    ) -> Result<OaiListRecordsPage, SourceError> {
        let url = self.list_records_url(from, until, set, token);
        let body = self.client.get_text(&url).await?;
        parse_oai_list_records(&body, &self.metadata_prefix)
    }

    /// Enumerate the repository's sets, following resumption tokens until the list
    /// ends. Bounded by [`MAX_LIST_SETS_REQUESTS`]; exceeding it stops the run with a
    /// typed budget error rather than an endless crawl.
    pub async fn list_sets(&self) -> Result<Vec<OaiSet>, SourceError> {
        let mut sets = Vec::new();
        let mut token: Option<String> = None;
        let mut requests = 0_usize;
        loop {
            if requests >= MAX_LIST_SETS_REQUESTS {
                return Err(SourceError::BudgetExhausted {
                    detail: format!(
                        "ListSets for `{}` did not complete within {MAX_LIST_SETS_REQUESTS} requests",
                        self.descriptor.id
                    ),
                });
            }
            let url = build_list_sets_url(&self.api_base, token.as_deref());
            let body = self.client.get_text(&url).await?;
            requests += 1;
            let page = parse_oai_list_sets(&body)?;
            sets.extend(page.sets);
            token = page
                .resumption_token
                .filter(|value| !value.trim().is_empty());
            if token.is_none() {
                return Ok(sets);
            }
        }
    }
}

#[async_trait]
impl SourceAdapter for OaiPmhAdapter {
    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    fn self_test(&self) -> SelfTestReport {
        let mut notes = vec![
            "harvest via ListRecords; resumptionToken continuation through the cursor"
                .to_owned(),
            "harvest window accepted as explicit from/until/set fields or an ISO date in query text; a non-date query text is refused with a typed error"
                .to_owned(),
            "deleted records are parsed, counted, and excluded from work records".to_owned(),
            format!(
                "metadata-only: the `{}` format carries no direct PDF links; oa_pdf_url is never set",
                self.metadata_prefix
            ),
        ];
        if self.metadata_prefix == ARXIV_RAW {
            notes.push(
                "work-type policy not applied: the arXiv format has no type field (every record is a preprint)"
                    .to_owned(),
            );
        } else {
            notes.push(format!(
                "paper-only dc:type filter: {}",
                if self.paper_only { "on" } else { "off" }
            ));
        }
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
            notes,
        }
    }

    async fn discover(
        &self,
        query: &DiscoveryQuery,
        cursor: Option<&str>,
    ) -> Result<DiscoveryPage, SourceError> {
        let token = cursor.filter(|token| !token.is_empty());
        let (from, until, set) = if let Some(_token) = token {
            // The resumption form is exclusive: no other arguments.
            (None, None, None)
        } else {
            let text_from = parse_from(&query.text);
            if text_from.is_none()
                && query.from_date.is_none()
                && query.until_date.is_none()
                && !query.text.trim().is_empty()
            {
                return Err(SourceError::InvalidConfiguration {
                    detail: format!(
                        "`{}` is an OAI-PMH harvest source: OAI-PMH has no free-text \
                         search. Pass a harvest window instead: an ISO date as the query \
                         text, or explicit from/until dates and an optional set, and bound \
                         the run with page/record budgets",
                        self.descriptor.id
                    ),
                });
            }
            let from = query.from_date.clone().or(text_from);
            let until = query.until_date.clone();
            if let Some(from) = &from {
                validate_harvest_date(from, "from_date")?;
            }
            if let Some(until) = &until {
                validate_harvest_date(until, "until_date")?;
            }
            if let (Some(from), Some(until)) = (&from, &until) {
                if from > until {
                    return Err(SourceError::InvalidConfiguration {
                        detail: format!(
                            "from_date ({from}) must not be later than until_date ({until})"
                        ),
                    });
                }
            }
            let set = query.set.clone().or_else(|| self.set.clone());
            (from, until, set)
        };
        let url = self.list_records_url(from.as_deref(), until.as_deref(), set.as_deref(), token);
        let body = self.client.get_text(&url).await?;
        let page = parse_oai_list_records(&body, &self.metadata_prefix)?;
        let records = page
            .records
            .iter()
            .filter_map(|parsed| self.parse_record(parsed))
            .collect();
        Ok(DiscoveryPage {
            records,
            next_cursor: page
                .resumption_token
                .filter(|token| !token.trim().is_empty()),
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

    fn arxiv_adapter() -> OaiPmhAdapter {
        OaiPmhAdapter::new(OaiPmhConfig::arxiv()).unwrap()
    }

    fn discover_error(adapter: &OaiPmhAdapter, query: &DiscoveryQuery) -> SourceError {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(adapter.discover(query, None)).unwrap_err()
    }

    // -------------------------------------------------------------------
    // Descriptors and construction
    // -------------------------------------------------------------------

    #[test]
    fn descriptor_declares_harvest_only_capabilities() {
        let adapter = zenodo_adapter();
        let descriptor = adapter.descriptor();
        assert_eq!(descriptor.id, "zenodo-oaipmh");
        assert_eq!(descriptor.api_base, "https://zenodo.org/oai2d");
        assert_eq!(descriptor.contract_version, SOURCE_ADAPTER_CONTRACT_VERSION);
        assert!(descriptor.supports(Capability::Discovery));
        assert!(!descriptor.supports(Capability::DoiLookup));
        assert!(!descriptor.supports(Capability::DirectPdfUrls));
    }

    #[test]
    fn arxiv_descriptor_declares_the_raw_format_and_courtesy_interval() {
        let adapter = arxiv_adapter();
        let descriptor = adapter.descriptor();
        assert_eq!(descriptor.id, "arxiv-oaipmh");
        assert_eq!(descriptor.api_base, "https://oaipmh.arxiv.org/oai");
        assert!(descriptor.supports(Capability::Discovery));
        assert!(!descriptor.supports(Capability::DirectPdfUrls));
        assert_eq!(
            descriptor.rate_policy.min_request_interval,
            ARXIV_OAI_MIN_REQUEST_INTERVAL
        );
    }

    #[test]
    fn unsupported_metadata_prefix_is_refused_at_construction() {
        let error = OaiPmhAdapter::new(OaiPmhConfig {
            metadata_prefix: "marc21".into(),
            ..OaiPmhConfig::zenodo()
        })
        .unwrap_err();
        assert!(matches!(error, SourceError::InvalidConfiguration { .. }));
    }

    // -------------------------------------------------------------------
    // URL building and date validation
    // -------------------------------------------------------------------

    #[test]
    fn list_records_url_encodes_selective_harvest_arguments() {
        let url = build_list_records_url(
            "https://example.org/oai",
            "oai_dc",
            Some("2026-01-01"),
            Some("2026-02-01T00:00:00Z"),
            Some("LCC:1789-"),
            None,
        );
        assert_eq!(
            url,
            "https://example.org/oai?verb=ListRecords&metadataPrefix=oai_dc\
             &from=2026-01-01&until=2026-02-01T00%3A00%3A00Z&set=LCC%3A1789-"
        );
    }

    #[test]
    fn resumption_token_is_exclusive() {
        let url = build_list_records_url(
            "https://example.org/oai",
            "oai_dc",
            Some("2026-01-01"),
            Some("2026-02-01"),
            Some("set"),
            Some("next/page==token"),
        );
        assert_eq!(
            url,
            "https://example.org/oai?verb=ListRecords&resumptionToken=next%2Fpage%3D%3Dtoken"
        );
    }

    #[test]
    fn harvest_dates_accept_day_and_second_granularity() {
        assert!(is_utc_date("2026-10-19"));
        assert!(is_utc_datetime("2026-10-19T11:08:33Z"));
        assert!(!is_utc_date("2026/10/19"));
        assert!(!is_utc_date("2026-1-9"));
        assert!(!is_utc_datetime("2026-10-19 11:08:33"));
    }

    #[test]
    fn parse_from_accepts_iso_dates_only() {
        assert_eq!(parse_from("2026-10-19").as_deref(), Some("2026-10-19"));
        assert_eq!(
            parse_from("  2026-10-19T11:08:33Z ").as_deref(),
            Some("2026-10-19T11:08:33Z")
        );
        assert_eq!(parse_from("attention is all you need"), None);
        assert_eq!(parse_from(""), None);
    }

    // -------------------------------------------------------------------
    // Recorded Zenodo page (their fixture)
    // -------------------------------------------------------------------

    #[test]
    fn parses_recorded_zenodo_response_with_resumption_token() {
        let adapter = zenodo_adapter();
        let document = include_str!("../../tests/fixtures/zenodo_oai_records.xml");
        let page = parse_oai_list_records(document, OAI_DC).unwrap();
        assert_eq!(page.records.len(), 3);
        assert!(page.resumption_token.is_some());
        // All three recorded records are `semantics/other` software/misc uploads:
        // rejected under the default paper-only policy.
        let kept: Vec<_> = page
            .records
            .iter()
            .filter_map(|parsed| adapter.parse_record(parsed))
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

    // -------------------------------------------------------------------
    // Recorded DOAJ pages (both fixture sets)
    // -------------------------------------------------------------------

    #[test]
    fn parses_recorded_doaj_response_and_filters_journals() {
        let adapter = doaj_adapter();
        let document = include_str!("../../tests/fixtures/doaj_oai_records.xml");
        let page = parse_oai_list_records(document, OAI_DC).unwrap();
        assert_eq!(page.records.len(), 2);
        assert!(page.resumption_token.is_some());
        // Journal records are containers, not papers: rejected by default.
        assert!(page
            .records
            .iter()
            .all(|parsed| adapter.parse_record(parsed).is_none()));

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
    fn parses_recorded_doaj_page_with_deletions_and_token() {
        let body = include_str!("../../tests/fixtures/doaj_oai_list_records.xml");
        let page = parse_oai_list_records(body, OAI_DC).unwrap();
        assert_eq!(page.records.len(), 2);
        assert_eq!(page.deleted_count, 1);
        assert_eq!(page.complete_list_size, Some(37100));
        let token = page
            .resumption_token
            .as_deref()
            .expect("recorded page carries a resumption token");
        assert!(token.starts_with("eyJtIjogIm9haV9kYyIs"), "token: {token}");

        let live = &page.records[0];
        assert!(!live.header.deleted);
        assert_eq!(
            live.header.identifier,
            "oai:doaj.org/journal:7af11a5feb5e40958b516fad5f98aa46"
        );
        let OaiMetadata::DublinCore(dc) = live.metadata.as_ref().unwrap() else {
            panic!("expected dublin core metadata");
        };
        assert_eq!(
            dc.title,
            "JASF (Journal of Accounting and Strategic Finance)"
        );
        assert_eq!(dc.types, vec!["journal".to_owned()]);
        assert_eq!(
            dc.identifiers,
            vec![
                "2614-6649".to_owned(),
                "https://doaj.org/toc/2614-6649".to_owned(),
            ]
        );
        assert_eq!(dc.rights.first().map(String::as_str), Some("CC BY"));

        let deleted = &page.records[1];
        assert!(deleted.header.deleted);
        assert_eq!(
            deleted.header.identifier,
            "oai:doaj.org/journal:ab959d158c564526bc247b1c58d5429d"
        );
        assert!(
            deleted.metadata.is_none(),
            "deleted records carry no metadata"
        );
    }

    #[test]
    fn permissive_mode_admits_doaj_journal_records_with_landing_pages() {
        let body = include_str!("../../tests/fixtures/doaj_oai_list_records.xml");
        let page = parse_oai_list_records(body, OAI_DC).unwrap();
        let adapter = OaiPmhAdapter::new(OaiPmhConfig {
            paper_only: false,
            ..OaiPmhConfig::doaj()
        })
        .unwrap();
        let works: Vec<_> = page
            .records
            .iter()
            .filter_map(|parsed| adapter.parse_record(parsed))
            .collect();
        assert_eq!(works.len(), 1);
        let work = &works[0];
        assert_eq!(work.source_id, "doaj-oaipmh");
        assert_eq!(
            work.title,
            "JASF (Journal of Accounting and Strategic Finance)"
        );
        assert_eq!(
            work.landing_url.as_deref(),
            Some("https://doaj.org/toc/2614-6649")
        );
        assert_eq!(work.license.as_deref(), Some("CC BY"));
        assert_eq!(work.publication_year, Some(2026));
        assert!(
            work.oa_pdf_url.is_none(),
            "oai_dc never carries direct PDF links"
        );
        assert!(work.doi().is_none(), "an ISSN is not a DOI");
        assert_eq!(
            work.raw["set_specs"][0].as_str().unwrap(),
            "TENDOkFjY291bnRpbmcuIEJvb2trZWVwaW5n"
        );
    }

    // -------------------------------------------------------------------
    // Recorded PMC pages (both fixture sets)
    // -------------------------------------------------------------------

    #[test]
    fn parses_recorded_pmc_response_with_identifiers() {
        let adapter = pmc_adapter();
        let document = include_str!("../../tests/fixtures/pmc_oai_records.xml");
        let page = parse_oai_list_records(document, OAI_DC).unwrap();
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
    fn parses_recorded_pmc_page_and_maps_article_metadata() {
        let body = include_str!("../../tests/fixtures/pmc_oai_list_records.xml");
        let page = parse_oai_list_records(body, OAI_DC).unwrap();
        assert_eq!(page.records.len(), 3);
        assert_eq!(page.deleted_count, 0);
        assert!(page.resumption_token.is_some());

        let first = &page.records[0];
        assert_eq!(first.header.identifier, "oai:pubmedcentral.nih.gov:6718227");
        let OaiMetadata::DublinCore(dc) = first.metadata.as_ref().unwrap() else {
            panic!("expected dublin core metadata");
        };
        assert_eq!(
            dc.title,
            "Interventions to reduce tobacco use in people experiencing homelessness"
        );
        assert_eq!(dc.types, vec!["Text".to_owned()]);
        assert_eq!(
            dc.identifiers,
            vec![
                "https://pmc.ncbi.nlm.nih.gov/articles/PMC6718227/".to_owned(),
                "https://doi.org/10.1002/14651858.cd013413".to_owned(),
            ]
        );

        // Adapter-level mapping: identifiers, landing page, no PDF claims.
        let adapter = pmc_adapter();
        let work = adapter
            .parse_record(first)
            .expect("Text type is paper-like");
        assert_eq!(work.source_record_id, "oai:pubmedcentral.nih.gov:6718227");
        assert_eq!(work.doi(), Some("10.1002/14651858.cd013413"));
        assert_eq!(work.identifier(IdentifierKind::Pmcid), Some("PMC6718227"));
        // The PMC article URL is consumed as the PMCID identifier; the landing
        // page falls back through relations to the DOI resolver.
        assert_eq!(
            work.landing_url.as_deref(),
            Some("https://doi.org/10.1002/14651858.cd013413")
        );
        assert_eq!(work.publication_year, Some(2019));
        assert_eq!(work.venue.as_deref(), Some("Cochrane Database Syst Rev"));
        assert_eq!(work.authors.len(), 3);
        assert_eq!(work.authors[0], "Vijayaraghavan, Maya");
        assert!(work.oa_pdf_url.is_none());
        assert!(work.raw["dc"]["type"][0].as_str().unwrap() == "Text");
    }

    // -------------------------------------------------------------------
    // Recorded arXiv page (arXiv raw format)
    // -------------------------------------------------------------------

    #[test]
    fn parses_recorded_arxiv_page_and_maps_preprints() {
        let body = include_str!("../../tests/fixtures/arxiv_oai_list_records.xml");
        let page = parse_oai_list_records(body, ARXIV_RAW).unwrap();
        assert_eq!(page.records.len(), 2);
        assert_eq!(page.deleted_count, 0);
        let token = page
            .resumption_token
            .as_deref()
            .expect("recorded arXiv page carries a resumption token");
        assert!(token.starts_with("verb%3DListRecords%26metadataPrefix%3DarXiv"));

        let first = &page.records[0];
        assert_eq!(first.header.identifier, "oai:arXiv.org:1509.00958");
        let OaiMetadata::ArxivRaw(arxiv) = first.metadata.as_ref().unwrap() else {
            panic!("expected arXiv raw metadata");
        };
        assert_eq!(arxiv.id, "1509.00958");
        assert_eq!(
            arxiv.title,
            "Improvements in the contemporary implementation of photoemission spectroscopy"
        );
        assert_eq!(arxiv.authors.len(), 1);
        assert_eq!(arxiv.authors[0].keyname, "Patil");
        assert_eq!(arxiv.authors[0].forenames, "Swapnil");
        assert!(arxiv
            .abstract_text
            .starts_with("In this short communication"));
        assert_eq!(
            arxiv.license.as_deref(),
            Some("http://arxiv.org/licenses/nonexclusive-distrib/1.0/")
        );

        // Adapter-level mapping through the arXiv built-in.
        let adapter = arxiv_adapter();
        let work = adapter
            .parse_record(first)
            .expect("preprints are paper-like");
        assert_eq!(work.source_id, "arxiv-oaipmh");
        assert_eq!(work.arxiv_id(), Some("1509.00958"));
        assert_eq!(work.authors, vec!["Swapnil Patil".to_owned()]);
        assert_eq!(work.publication_year, Some(2026));
        assert!(work.oa_pdf_url.is_none());
        assert!(work.landing_url.is_none());
        assert!(work.raw["arxiv"]["id"].as_str().unwrap() == "1509.00958");
    }

    // -------------------------------------------------------------------
    // noRecordsMatch (both fixture sets)
    // -------------------------------------------------------------------

    #[test]
    fn no_records_match_is_an_empty_page_not_an_error() {
        let document = include_str!("../../tests/fixtures/doaj_oai_norecords.xml");
        let page = parse_oai_list_records(document, OAI_DC).unwrap();
        assert!(page.records.is_empty());
        assert!(page.resumption_token.is_none());
    }

    #[test]
    fn no_records_match_maps_to_an_empty_page() {
        let body = include_str!("../../tests/fixtures/doaj_oai_no_records.xml");
        let page = parse_oai_list_records(body, OAI_DC).unwrap();
        assert!(page.records.is_empty());
        assert_eq!(page.deleted_count, 0);
        assert!(page.resumption_token.is_none());
    }

    // -------------------------------------------------------------------
    // Error mapping and deleted handling
    // -------------------------------------------------------------------

    #[test]
    fn deleted_records_are_skipped() {
        let parsed = OaiRecord {
            header: OaiRecordHeader {
                identifier: "oai:example.org:1".into(),
                deleted: true,
                ..OaiRecordHeader::default()
            },
            metadata: None,
        };
        assert!(zenodo_adapter().parse_record(&parsed).is_none());
    }

    #[test]
    fn oai_protocol_errors_are_typed() {
        let document = r#"<?xml version="1.0"?>
<OAI-PMH xmlns="http://www.openarchives.org/OAI/2.0/">
  <error code="badArgument">Missing metadataPrefix</error>
</OAI-PMH>"#;
        let error = parse_oai_list_records(document, OAI_DC).unwrap_err();
        assert!(error.to_string().contains("badArgument"));
        assert!(matches!(error, SourceError::MalformedResponse { .. }));

        let error = parse_oai_list_records("not xml", OAI_DC).unwrap_err();
        assert!(matches!(error, SourceError::MalformedResponse { .. }));

        let body = r#"<?xml version="1.0" encoding="UTF-8"?>
<OAI-PMH xmlns="http://www.openarchives.org/OAI/2.0/">
  <responseDate>2026-10-19T11:08:25Z</responseDate>
</OAI-PMH>"#;
        assert!(matches!(
            parse_oai_list_records(body, OAI_DC),
            Err(SourceError::MalformedResponse { .. })
        ));
    }

    // -------------------------------------------------------------------
    // Identifier extraction
    // -------------------------------------------------------------------

    #[test]
    fn pmcid_is_extracted_from_article_url_shapes() {
        assert_eq!(
            pmcid_from_article_url("https://pmc.ncbi.nlm.nih.gov/articles/PMC6718227/"),
            Some("PMC6718227".to_owned())
        );
        assert_eq!(
            pmcid_from_article_url("https://www.ncbi.nlm.nih.gov/pmc/articles/PMC4302049"),
            Some("PMC4302049".to_owned())
        );
        assert_eq!(
            pmcid_from_article_url("https://example.org/articles/PMC12345/"),
            Some("PMC12345".to_owned())
        );
        assert_eq!(pmcid_from_article_url("https://example.org/PMC123"), None);
        assert_eq!(pmcid_from_article_url("2614-6649"), None);
    }

    // -------------------------------------------------------------------
    // Recorded DOAJ ListSets
    // -------------------------------------------------------------------

    #[test]
    fn parses_recorded_doaj_list_sets() {
        let body = include_str!("../../tests/fixtures/doaj_oai_list_sets.xml");
        let page = parse_oai_list_sets(body).unwrap();
        assert_eq!(page.sets.len(), 6);
        assert_eq!(page.sets[0].spec, "TENDOjE3ODkt");
        assert_eq!(page.sets[0].name.as_deref(), Some("LCC:1789-"));
        assert_eq!(
            page.sets[1].name.as_deref(),
            Some("LCC:Academies and learned societies")
        );
        assert!(page.resumption_token.is_none());
    }

    // -------------------------------------------------------------------
    // discover() harvest-window semantics
    // -------------------------------------------------------------------

    #[test]
    fn discover_refuses_non_date_query_text() {
        let adapter = doaj_adapter();
        match discover_error(&adapter, &DiscoveryQuery::new("some search text")) {
            SourceError::InvalidConfiguration { detail } => {
                assert!(detail.contains("no free-text search"), "detail: {detail}");
            }
            other => panic!("expected InvalidConfiguration, got {other:?}"),
        }
    }

    #[test]
    fn discover_rejects_malformed_and_inconsistent_date_ranges() {
        let adapter = doaj_adapter();
        for (from, until) in [
            (Some("2026/10/19"), None),
            (None, Some("not a date")),
            (Some("2026-10-19"), Some("2026-10-18")),
        ] {
            let query = DiscoveryQuery {
                from_date: from.map(|value| value.to_owned()),
                until_date: until.map(|value| value.to_owned()),
                ..DiscoveryQuery::new("")
            };
            assert!(
                matches!(
                    discover_error(&adapter, &query),
                    SourceError::InvalidConfiguration { .. }
                ),
                "expected InvalidConfiguration for from={from:?} until={until:?}"
            );
        }
    }

    #[test]
    fn self_test_reports_harvest_semantics() {
        let report = doaj_adapter().self_test();
        assert_eq!(report.source_id, "doaj-oaipmh");
        assert!(report.configured);
        assert!(report
            .notes
            .iter()
            .any(|note| note.contains("no direct PDF links")));
        assert!(report
            .notes
            .iter()
            .any(|note| note.contains("deleted records are parsed, counted")));
        let arxiv_report = arxiv_adapter().self_test();
        assert!(arxiv_report
            .notes
            .iter()
            .any(|note| note.contains("no type field")));
    }
}
