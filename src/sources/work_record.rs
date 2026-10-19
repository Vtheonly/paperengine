//! Normalized work records produced by source adapters (#8 partial).
//!
//! A `WorkRecord` is the engine's normalized view of one scholarly work as reported by one
//! source. The raw source payload is retained verbatim under `raw` so later reconciliation can
//! re-derive fields without silent information loss.

use serde::{Deserialize, Serialize};

pub const WORK_RECORD_SCHEMA_VERSION: u16 = 1;

/// Identifier namespaces the normalizer understands today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentifierKind {
    Doi,
    Arxiv,
    Pmid,
    Openalex,
    Crossref,
}

impl IdentifierKind {
    pub fn as_str(self) -> &'static str {
        match self {
            IdentifierKind::Doi => "doi",
            IdentifierKind::Arxiv => "arxiv",
            IdentifierKind::Pmid => "pmid",
            IdentifierKind::Openalex => "openalex",
            IdentifierKind::Crossref => "crossref",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkIdentifier {
    pub kind: IdentifierKind,
    pub value: String,
}

impl WorkIdentifier {
    pub fn new(kind: IdentifierKind, value: impl Into<String>) -> Self {
        Self {
            kind,
            value: value.into(),
        }
    }
}

/// Paper-like work type strings across the built-in sources. Books, datasets, and
/// paratext are excluded by the default filter (#10: paper-only defaults).
pub const PAPER_LIKE_WORK_TYPES: &[&str] = &[
    // OpenAlex
    "article",
    "preprint",
    "book-chapter",
    "report",
    "dissertation",
    "letter",
    "editorial",
    "standard",
    // Crossref
    "journal-article",
    "proceedings-article",
    "posted-content",
    "report-component",
];

/// How adapter results are filtered by work type. The engine default is paper-only
/// (#10); callers may narrow or widen the set explicitly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum WorkTypeFilter {
    /// Paper-like types only (the project default).
    #[default]
    PaperOnly,
    /// Accept every work type.
    All,
    /// Accept exactly these source-reported type strings.
    Only(Vec<String>),
}

impl WorkTypeFilter {
    pub fn accepts(&self, work_type: Option<&str>) -> bool {
        match self {
            WorkTypeFilter::All => true,
            WorkTypeFilter::Only(accepted) => match work_type {
                Some(work_type) => accepted.iter().any(|candidate| candidate == work_type),
                None => false,
            },
            WorkTypeFilter::PaperOnly => match work_type {
                Some(work_type) => PAPER_LIKE_WORK_TYPES.contains(&work_type),
                None => false,
            },
        }
    }
}

/// One scholarly work as reported by one source, with raw metadata retained.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkRecord {
    pub schema_version: u16,
    /// Adapter that produced this record.
    pub source_id: String,
    /// Source's own record identifier (e.g. the OpenAlex `W…` id or the arXiv id).
    pub source_record_id: String,
    /// Normalized identifiers found on the record (DOI, arXiv id, …).
    pub identifiers: Vec<WorkIdentifier>,
    pub title: String,
    pub authors: Vec<String>,
    pub publication_year: Option<u16>,
    pub venue: Option<String>,
    /// License string as reported by the source, when known.
    pub license: Option<String>,
    /// Direct, openly accessible PDF URL when the source itself reports one. `None` means the
    /// source did not expose a permitted direct full-text link — it never means "paywalled
    /// URL we may fetch anyway".
    pub oa_pdf_url: Option<String>,
    /// Landing page for the work.
    pub landing_url: Option<String>,
    /// Verbatim raw metadata from the source (JSON), retained for provenance and replay.
    pub raw: serde_json::Value,
}

impl WorkRecord {
    pub fn new(source_id: impl Into<String>, source_record_id: impl Into<String>) -> Self {
        Self {
            schema_version: WORK_RECORD_SCHEMA_VERSION,
            source_id: source_id.into(),
            source_record_id: source_record_id.into(),
            identifiers: Vec::new(),
            title: String::new(),
            authors: Vec::new(),
            publication_year: None,
            venue: None,
            license: None,
            oa_pdf_url: None,
            landing_url: None,
            raw: serde_json::Value::Null,
        }
    }

    pub fn identifier(&self, kind: IdentifierKind) -> Option<&str> {
        self.identifiers
            .iter()
            .find(|identifier| identifier.kind == kind)
            .map(|identifier| identifier.value.as_str())
    }

    pub fn doi(&self) -> Option<&str> {
        self.identifier(IdentifierKind::Doi)
    }

    pub fn arxiv_id(&self) -> Option<&str> {
        self.identifier(IdentifierKind::Arxiv)
    }

    pub fn push_identifier(&mut self, kind: IdentifierKind, value: Option<String>) {
        if let Some(value) = value {
            if !self.identifiers.iter().any(|id| id.kind == kind) {
                self.identifiers.push(WorkIdentifier::new(kind, value));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_round_trips_through_jsonl() {
        let mut record = WorkRecord::new("openalex", "W123");
        record.title = "A Study".into();
        record.authors = vec!["A. Author".into()];
        record.publication_year = Some(2024);
        record.oa_pdf_url = Some("https://example.org/paper.pdf".into());
        record.raw = serde_json::json!({ "display_name": "A Study" });
        record.push_identifier(IdentifierKind::Doi, Some("10.1000/example".into()));

        let line = serde_json::to_string(&record).unwrap();
        let parsed: WorkRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed.source_id, "openalex");
        assert_eq!(parsed.source_record_id, "W123");
        assert_eq!(parsed.doi(), Some("10.1000/example"));
        assert_eq!(
            parsed.oa_pdf_url.as_deref(),
            Some("https://example.org/paper.pdf")
        );
        assert_eq!(parsed.raw["display_name"], "A Study");
        assert_eq!(parsed.schema_version, WORK_RECORD_SCHEMA_VERSION);
    }

    #[test]
    fn identifier_lookup_ignores_other_kinds() {
        let mut record = WorkRecord::new("arxiv", "2401.00001");
        record.identifiers.push(WorkIdentifier::new(
            IdentifierKind::Arxiv,
            "2401.00001".to_owned(),
        ));
        assert_eq!(record.arxiv_id(), Some("2401.00001"));
        assert_eq!(record.doi(), None);
    }
}
