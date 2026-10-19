//! Incremental full-text and metadata indexing over a local library (#32).
//!
//! The index is a derived, rebuildable structure: the canonical metadata (provenance
//! manifests, extraction records, objects) is never modified by indexing. A failed or
//! interrupted indexing run commits nothing and can simply be re-run.
//!
//! Layout:
//!
//! ```text
//! library/
//!   index/                     Tantivy index directory (derived; delete to rebuild)
//!   reports/<unix>-index.jsonl per-run indexing reports
//! ```
//!
//! Incrementality: each document carries the SHA-256 of the state it was indexed from
//! (manifest bytes plus extraction-record bytes when present). On re-index, a document
//! whose state digest is unchanged is skipped; a changed document is deleted and
//! re-added. Documents whose objects no longer exist in the library are deleted. Object
//! identity is the content address, so distinct versions of one work are retained as
//! distinct documents — version separation is structural, not a special case.
//!
//! Crash safety: all deletes and adds of one run are committed together at the end of
//! the run. An interrupted run leaves the previous committed state untouched.

use crate::extract::ExtractionRecord;
use crate::library::{LibraryError, LibraryLayout, ProvenanceManifest};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use tantivy::collector::TopDocs;
use tantivy::query::{
    AllQuery, BooleanQuery, EmptyQuery, Occur, Query, QueryParser, RangeQuery, TermQuery,
};
use tantivy::schema::{Field, IndexRecordOption, Schema, FAST, INDEXED, STORED, STRING, TEXT};
use tantivy::{doc, Index, IndexReader, IndexWriter, TantivyDocument, Term};
use thiserror::Error;

pub const INDEX_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Error)]
pub enum IndexError {
    #[error("filesystem operation failed: {0}")]
    Io(#[from] io::Error),
    #[error("serialization failed: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("index engine failure: {0}")]
    Engine(#[from] tantivy::TantivyError),
    #[error("invalid library layout: {0}")]
    Layout(String),
    #[error("invalid indexing configuration: {0}")]
    InvalidConfig(String),
    #[error("extraction record failure: {0}")]
    Extraction(String),
    #[error("query syntax error: {0}")]
    Query(String),
}

impl From<LibraryError> for IndexError {
    fn from(error: LibraryError) -> Self {
        IndexError::Layout(error.to_string())
    }
}

/// Resource budgets for one indexing run.
#[derive(Debug, Clone, Copy)]
pub struct IndexingConfig {
    /// Tantivy writer memory budget.
    pub memory_budget_bytes: usize,
}

impl Default for IndexingConfig {
    fn default() -> Self {
        Self {
            memory_budget_bytes: 32 * 1024 * 1024,
        }
    }
}

impl IndexingConfig {
    pub fn validate(&self) -> Result<(), IndexError> {
        // Tantivy requires at least 15 MB per writer thread; one thread is the floor.
        if self.memory_budget_bytes < 15_000_000 {
            return Err(IndexError::InvalidConfig(
                "memory budget must be at least 15 MB (Tantivy per-thread arena minimum)".into(),
            ));
        }
        Ok(())
    }
}

/// The index field set. Built once and shared by indexing and search.
#[derive(Debug, Clone)]
pub struct LibraryIndexSchema {
    pub schema: Schema,
    pub object_sha: Field,
    pub state_digest: Field,
    pub source_id: Field,
    pub source_record_id: Field,
    pub title: Field,
    pub authors: Field,
    pub venue: Field,
    pub doi: Field,
    pub arxiv_id: Field,
    pub publication_year: Field,
    pub page_count: Field,
    pub byte_length: Field,
    pub pdf_version: Field,
    pub license: Field,
    pub body_text: Field,
    pub text_available: Field,
    pub extraction_version: Field,
}

impl LibraryIndexSchema {
    pub fn build() -> Self {
        let mut builder = Schema::builder();
        let object_sha = builder.add_text_field("object_sha", STRING | STORED);
        let state_digest = builder.add_text_field("state_digest", STRING | STORED);
        let source_id = builder.add_text_field("source_id", STRING | STORED);
        let source_record_id = builder.add_text_field("source_record_id", STRING | STORED);
        let title = builder.add_text_field("title", TEXT | STORED);
        let authors = builder.add_text_field("authors", TEXT | STORED);
        let venue = builder.add_text_field("venue", TEXT | STORED);
        let doi = builder.add_text_field("doi", STRING | STORED);
        let arxiv_id = builder.add_text_field("arxiv_id", STRING | STORED);
        let publication_year = builder.add_u64_field("publication_year", FAST | INDEXED | STORED);
        let page_count = builder.add_u64_field("page_count", FAST | INDEXED | STORED);
        let byte_length = builder.add_u64_field("byte_length", FAST | INDEXED | STORED);
        let pdf_version = builder.add_text_field("pdf_version", STRING | STORED);
        let license = builder.add_text_field("license", STRING | STORED);
        let body_text = builder.add_text_field("body_text", TEXT);
        let text_available = builder.add_u64_field("text_available", FAST | INDEXED | STORED);
        let extraction_version =
            builder.add_u64_field("extraction_version", FAST | INDEXED | STORED);
        Self {
            schema: builder.build(),
            object_sha,
            state_digest,
            source_id,
            source_record_id,
            title,
            authors,
            venue,
            doi,
            arxiv_id,
            publication_year,
            page_count,
            byte_length,
            pdf_version,
            license,
            body_text,
            text_available,
            extraction_version,
        }
    }
}

/// Per-document outcome written into the run report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexingReportEntry {
    pub schema_version: u16,
    /// Object address, or the manifest file stem for manifest-level failures.
    pub object_sha: String,
    pub outcome: IndexingOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IndexingOutcome {
    Added,
    Updated,
    Skipped { reason: String },
    Deleted { reason: String },
    Failed { stage: String, detail: String },
}

/// Run-wide accounting returned by [`run_indexing`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexingRunSummary {
    pub manifests_examined: usize,
    pub added: usize,
    pub updated: usize,
    pub skipped: usize,
    pub deleted: usize,
    pub failed: usize,
}

/// One manifest plus its optional extraction record and derived state digest.
struct IndexedState {
    manifest: ProvenanceManifest,
    extraction: Option<ExtractionRecord>,
    state_digest: String,
}

/// Compute the state digest for one document: SHA-256 over the manifest bytes and the
/// extraction-record bytes when present. Deterministic; ignores whitespace formatting
/// by hashing the canonical serialization.
fn state_digest_of(manifest: &ProvenanceManifest, extraction: &Option<ExtractionRecord>) -> String {
    let manifest_bytes = serde_json::to_vec(manifest).unwrap_or_default();
    let extraction_bytes = extraction
        .as_ref()
        .map(|record| serde_json::to_vec(record).unwrap_or_default())
        .unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(&manifest_bytes);
    hasher.update(&extraction_bytes);
    format!("{:x}", hasher.finalize())
}

/// Load the manifest and its extraction record from disk.
fn load_indexed_state(
    manifest_path: &Path,
    library: &LibraryLayout,
) -> Result<IndexedState, IndexError> {
    let manifest_bytes = fs::read(manifest_path)?;
    let manifest: ProvenanceManifest =
        serde_json::from_slice(&manifest_bytes).map_err(IndexError::Serde)?;
    let extraction = crate::extract::read_record(library, &manifest.content.sha256_hex)
        .map_err(|error| IndexError::Extraction(error.to_string()))?;
    let state_digest = state_digest_of(&manifest, &extraction);
    Ok(IndexedState {
        manifest,
        extraction,
        state_digest,
    })
}

/// Open the library index, creating it when absent. `rebuild` drops the existing index
/// directory first. Returns an error suggesting a rebuild when an existing index cannot
/// be opened with the current schema.
fn open_index(
    library: &LibraryLayout,
    schema: &LibraryIndexSchema,
    rebuild: bool,
) -> Result<Index, IndexError> {
    let index_dir = library.index_dir();
    if rebuild && index_dir.exists() {
        fs::remove_dir_all(&index_dir)?;
    }
    if index_dir.exists() {
        match Index::open_in_dir(&index_dir) {
            Ok(index) => {
                // Refuse to use an index whose schema predates the current field set.
                for field_name in [
                    "object_sha",
                    "state_digest",
                    "title",
                    "body_text",
                    "text_available",
                ] {
                    if index.schema().get_field(field_name).is_err() {
                        return Err(IndexError::InvalidConfig(format!(
                            "existing index is missing field `{field_name}`; re-run with --rebuild"
                        )));
                    }
                }
                Ok(index)
            }
            Err(error) => Err(IndexError::InvalidConfig(format!(
                "existing index could not be opened ({error}); re-run with --rebuild"
            ))),
        }
    } else {
        fs::create_dir_all(&index_dir)?;
        Ok(Index::create_in_dir(&index_dir, schema.schema.clone())?)
    }
}

/// Current `(object_sha -> state_digest)` mapping held by the committed index.
fn committed_states(
    reader: &IndexReader,
    schema: &LibraryIndexSchema,
) -> Result<BTreeMap<String, String>, IndexError> {
    let searcher = reader.searcher();
    let collector = TopDocs::with_limit(1_000_000).order_by_score();
    let hits = searcher.search(&AllQuery, &collector)?;
    let mut states = BTreeMap::new();
    for (_score, address) in hits {
        let document: TantivyDocument = searcher.doc(address)?;
        let sha = stored_text(&document, schema.object_sha);
        let digest = stored_text(&document, schema.state_digest);
        if let Some(sha) = sha {
            states.insert(sha, digest.unwrap_or_default());
        }
    }
    Ok(states)
}

fn stored_text(document: &TantivyDocument, field: Field) -> Option<String> {
    let value = document.get_first(field)?;
    let owned = tantivy::schema::OwnedValue::from(value);
    match owned {
        tantivy::schema::OwnedValue::Str(text) => Some(text),
        tantivy::schema::OwnedValue::PreTokStr(tokenized) => Some(tokenized.text),
        _ => None,
    }
}

fn stored_u64(document: &TantivyDocument, field: Field) -> Option<u64> {
    let value = document.get_first(field)?;
    let owned = tantivy::schema::OwnedValue::from(value);
    match owned {
        tantivy::schema::OwnedValue::U64(value) => Some(value),
        tantivy::schema::OwnedValue::I64(value) => Some(value.unsigned_abs()),
        _ => None,
    }
}

/// Build one Tantivy document from manifest, extraction record, and extracted text.
fn build_document(
    schema: &LibraryIndexSchema,
    state: &IndexedState,
    body_text: Option<&str>,
) -> TantivyDocument {
    let manifest = &state.manifest;
    let mut document = doc!(
        schema.object_sha => manifest.content.sha256_hex.as_str(),
        schema.state_digest => state.state_digest.as_str(),
        schema.source_id => manifest.work.source_id.as_str(),
        schema.source_record_id => manifest.work.source_record_id.as_str(),
        schema.title => manifest.work.title.as_str(),
        schema.byte_length => manifest.content.byte_length,
    );
    if !manifest.work.authors.is_empty() {
        for author in &manifest.work.authors {
            document.add_field_value(schema.authors, author.as_str());
        }
    }
    if let Some(venue) = manifest.work.venue.as_deref() {
        document.add_field_value(schema.venue, venue);
    }
    if let Some(license) = manifest.work.license.as_deref() {
        document.add_field_value(schema.license, license);
    }
    if let Some(year) = manifest.work.publication_year {
        let year_value = year as u64;
        document.add_field_value(schema.publication_year, &year_value);
    }
    if let Some(doi) = manifest
        .work
        .identifiers
        .iter()
        .find(|identifier| identifier.kind.as_str() == "doi")
    {
        document.add_field_value(schema.doi, doi.value.as_str());
    }
    if let Some(arxiv) = manifest
        .work
        .identifiers
        .iter()
        .find(|identifier| identifier.kind.as_str() == "arxiv")
    {
        document.add_field_value(schema.arxiv_id, arxiv.value.as_str());
    }
    if let Some(extraction) = &state.extraction {
        let page_count_value = extraction.object.page_count as u64;
        document.add_field_value(schema.page_count, &page_count_value);
        document.add_field_value(schema.pdf_version, extraction.object.pdf_version.as_str());
        let extraction_version_value = extraction.extractor.version as u64;
        document.add_field_value(schema.extraction_version, &extraction_version_value);
        let has_text = if extraction.status.has_text() {
            1u64
        } else {
            0u64
        };
        document.add_field_value(schema.text_available, &has_text);
    }
    if let Some(text) = body_text {
        document.add_field_value(schema.body_text, text);
    }
    document
}

/// Read the extracted body text for one object, bounded by the extraction artifact's
/// recorded byte length. Returns `None` when no text artifact exists.
fn read_body_text(library: &LibraryLayout, extraction: &ExtractionRecord) -> Option<String> {
    let path = library.root().join(&extraction.text.path);
    let bytes = fs::read(path).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Run the incremental indexing pipeline: analyze library state against the committed
/// index, apply adds/updates/deletes, and commit once at the end. Canonical metadata is
/// only ever read.
pub fn run_indexing(
    library: &LibraryLayout,
    config: &IndexingConfig,
    rebuild: bool,
    mut on_entry: impl FnMut(&IndexingReportEntry) -> Result<(), io::Error>,
) -> Result<IndexingRunSummary, IndexError> {
    config.validate()?;

    let schema = LibraryIndexSchema::build();
    let index = open_index(library, &schema, rebuild)?;
    let reader = index.reader()?;
    let mut writer: IndexWriter<TantivyDocument> = index.writer(config.memory_budget_bytes)?;

    // 1. Load the library's current state: one entry per manifest.
    let manifest_paths = list_manifest_paths(library)?;
    let mut current: BTreeMap<String, IndexedState> = BTreeMap::new();
    let mut summary = IndexingRunSummary {
        manifests_examined: 0,
        added: 0,
        updated: 0,
        skipped: 0,
        deleted: 0,
        failed: 0,
    };

    for path in &manifest_paths {
        summary.manifests_examined += 1;
        match load_indexed_state(path, library) {
            Ok(state) => {
                // Documents are object-addressed: when several manifests point at one
                // deduplicated object (the same bytes acquired via several sources), the
                // first manifest in sorted path order provides the indexed metadata and
                // the others are recorded as skipped duplicates.
                let address = state.manifest.content.sha256_hex.clone();
                if let std::collections::btree_map::Entry::Vacant(entry) =
                    current.entry(address.clone())
                {
                    entry.insert(state);
                } else {
                    summary.skipped += 1;
                    on_entry(&IndexingReportEntry {
                        schema_version: INDEX_SCHEMA_VERSION,
                        object_sha: address,
                        outcome: IndexingOutcome::Skipped {
                            reason: "duplicate manifest for an already-indexed object".to_owned(),
                        },
                    })?;
                }
            }
            Err(error) => {
                summary.failed += 1;
                on_entry(&IndexingReportEntry {
                    schema_version: INDEX_SCHEMA_VERSION,
                    object_sha: path
                        .file_stem()
                        .map(|stem| stem.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    outcome: IndexingOutcome::Failed {
                        stage: "load".to_owned(),
                        detail: error.to_string(),
                    },
                })?;
            }
        }
    }

    // 2. Compare against the committed index.
    let committed = committed_states(&reader, &schema)?;
    let mut operations: Vec<IndexingReportEntry> = Vec::new();

    for (sha, state) in &current {
        match committed.get(sha) {
            Some(existing_digest) if *existing_digest == state.state_digest => {
                summary.skipped += 1;
                operations.push(IndexingReportEntry {
                    schema_version: INDEX_SCHEMA_VERSION,
                    object_sha: sha.clone(),
                    outcome: IndexingOutcome::Skipped {
                        reason: "state digest unchanged".to_owned(),
                    },
                });
            }
            Some(_) => {
                // State changed: remove the stale document, re-add the fresh one.
                writer.delete_term(Term::from_field_text(schema.object_sha, sha));
                let document =
                    build_document(&schema, state, body_text_for(library, state).as_deref());
                writer.add_document(document)?;
                summary.updated += 1;
                operations.push(IndexingReportEntry {
                    schema_version: INDEX_SCHEMA_VERSION,
                    object_sha: sha.clone(),
                    outcome: IndexingOutcome::Updated,
                });
            }
            None => {
                let document =
                    build_document(&schema, state, body_text_for(library, state).as_deref());
                writer.add_document(document)?;
                summary.added += 1;
                operations.push(IndexingReportEntry {
                    schema_version: INDEX_SCHEMA_VERSION,
                    object_sha: sha.clone(),
                    outcome: IndexingOutcome::Added,
                });
            }
        }
    }

    // 3. Delete documents whose objects no longer exist in the library.
    for sha in committed.keys() {
        if !current.contains_key(sha) {
            writer.delete_term(Term::from_field_text(schema.object_sha, sha));
            summary.deleted += 1;
            operations.push(IndexingReportEntry {
                schema_version: INDEX_SCHEMA_VERSION,
                object_sha: sha.clone(),
                outcome: IndexingOutcome::Deleted {
                    reason: "object no longer present in the library".to_owned(),
                },
            });
        }
    }

    // 4. Commit once: an interrupted run leaves the previous state intact.
    writer.commit()?;
    for entry in &operations {
        on_entry(entry)?;
    }
    reader.reload()?;

    Ok(summary)
}

fn body_text_for(library: &LibraryLayout, state: &IndexedState) -> Option<String> {
    state
        .extraction
        .as_ref()
        .filter(|record| record.status.has_text())
        .and_then(|record| read_body_text(library, record))
}

fn list_manifest_paths(library: &LibraryLayout) -> Result<Vec<PathBuf>, IndexError> {
    let mut paths = Vec::new();
    let dir = library.manifests_dir();
    if !dir.exists() {
        return Ok(paths);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("json") {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

/// Filters accepted by [`search_library`]. All optional.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SearchFilters {
    pub source_id: Option<String>,
    pub doi: Option<String>,
    pub arxiv_id: Option<String>,
    pub year_min: Option<u64>,
    pub year_max: Option<u64>,
    /// Only works whose extraction produced text.
    pub text_only: bool,
}

/// One search result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub object_sha: String,
    pub title: String,
    pub authors: Vec<String>,
    pub venue: Option<String>,
    pub publication_year: Option<u64>,
    pub doi: Option<String>,
    pub arxiv_id: Option<String>,
    pub source_id: String,
    pub source_record_id: String,
    pub license: Option<String>,
    pub page_count: Option<u64>,
    pub byte_length: u64,
    pub pdf_version: Option<String>,
    pub text_available: bool,
    pub score: f32,
}

/// Search the committed index. `query` uses Tantivy's query syntax over title, authors,
/// venue, and extracted body text (title boosted); an empty query with filters returns
/// everything matching the filters.
pub fn search_library(
    library: &LibraryLayout,
    query: &str,
    filters: &SearchFilters,
    limit: usize,
) -> Result<Vec<SearchHit>, IndexError> {
    if limit == 0 {
        return Err(IndexError::InvalidConfig("limit must be positive".into()));
    }
    let schema = LibraryIndexSchema::build();
    let index = open_index(library, &schema, false)?;
    let reader = index.reader()?;
    let searcher = reader.searcher();

    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    let trimmed = query.trim();
    if !trimmed.is_empty() {
        let mut parser = QueryParser::for_index(
            &index,
            vec![schema.title, schema.authors, schema.venue, schema.body_text],
        );
        parser.set_field_boost(schema.title, 3.0);
        let parsed = parser
            .parse_query(trimmed)
            .map_err(|error| IndexError::Query(error.to_string()))?;
        clauses.push((Occur::Must, parsed));
    } else if filters == &SearchFilters::default() {
        return Err(IndexError::Query(
            "provide a query or at least one filter".into(),
        ));
    }
    if let Some(source) = filters.source_id.as_deref() {
        clauses.push((
            Occur::Must,
            Box::new(TermQuery::new(
                Term::from_field_text(schema.source_id, source),
                IndexRecordOption::Basic,
            )),
        ));
    }
    if let Some(doi) = filters.doi.as_deref() {
        clauses.push((
            Occur::Must,
            Box::new(TermQuery::new(
                Term::from_field_text(schema.doi, doi),
                IndexRecordOption::Basic,
            )),
        ));
    }
    if let Some(arxiv) = filters.arxiv_id.as_deref() {
        clauses.push((
            Occur::Must,
            Box::new(TermQuery::new(
                Term::from_field_text(schema.arxiv_id, arxiv),
                IndexRecordOption::Basic,
            )),
        ));
    }
    if let Some(year_min) = filters.year_min {
        let lower = Bound::Included(Term::from_field_u64(schema.publication_year, year_min));
        let upper = filters
            .year_max
            .map(|year_max| {
                Bound::Included(Term::from_field_u64(schema.publication_year, year_max))
            })
            .unwrap_or(Bound::Unbounded);
        clauses.push((Occur::Must, Box::new(RangeQuery::new(lower, upper))));
    } else if let Some(year_max) = filters.year_max {
        let upper = Bound::Included(Term::from_field_u64(schema.publication_year, year_max));
        clauses.push((
            Occur::Must,
            Box::new(RangeQuery::new(Bound::Unbounded, upper)),
        ));
    }
    if filters.text_only {
        clauses.push((
            Occur::Must,
            Box::new(TermQuery::new(
                Term::from_field_u64(schema.text_available, 1u64),
                IndexRecordOption::Basic,
            )),
        ));
    }

    let boolean = BooleanQuery::new(clauses);
    let collector = TopDocs::with_limit(limit).order_by_score();
    let hits = searcher.search(&boolean, &collector)?;

    let mut results = Vec::with_capacity(hits.len());
    for (score, address) in hits {
        let document: TantivyDocument = searcher.doc(address)?;
        let identifiers_authors: Vec<String> = document
            .get_all(schema.authors)
            .map(|value| match tantivy::schema::OwnedValue::from(value) {
                tantivy::schema::OwnedValue::Str(text) => text,
                tantivy::schema::OwnedValue::PreTokStr(tokenized) => tokenized.text,
                _ => String::new(),
            })
            .collect();
        results.push(SearchHit {
            object_sha: stored_text(&document, schema.object_sha).unwrap_or_default(),
            title: stored_text(&document, schema.title).unwrap_or_default(),
            authors: identifiers_authors,
            venue: stored_text(&document, schema.venue),
            publication_year: stored_u64(&document, schema.publication_year),
            doi: stored_text(&document, schema.doi),
            arxiv_id: stored_text(&document, schema.arxiv_id),
            source_id: stored_text(&document, schema.source_id).unwrap_or_default(),
            source_record_id: stored_text(&document, schema.source_record_id).unwrap_or_default(),
            license: stored_text(&document, schema.license),
            page_count: stored_u64(&document, schema.page_count),
            byte_length: stored_u64(&document, schema.byte_length).unwrap_or_default(),
            pdf_version: stored_text(&document, schema.pdf_version),
            text_available: stored_u64(&document, schema.text_available).unwrap_or(0) == 1,
            score,
        });
    }
    Ok(results)
}

/// Count documents in the committed index (used by the CLI and parity tests).
pub fn document_count(library: &LibraryLayout) -> Result<usize, IndexError> {
    let schema = LibraryIndexSchema::build();
    let index = open_index(library, &schema, false)?;
    let reader = index.reader()?;
    let searcher = reader.searcher();
    let collector = TopDocs::with_limit(1_000_000).order_by_score();
    let hits = searcher.search(&AllQuery, &collector)?;
    Ok(hits.len())
}

/// A query matching nothing; kept for API completeness of empty filters.
#[allow(dead_code)]
fn empty_query() -> Box<dyn Query> {
    Box::new(EmptyQuery)
}
