//! `paperengine search` — local full-text and metadata search over the library index
//! (#33).
//!
//! Searches the committed Tantivy index (built by `paperengine index`) over title,
//! authors, venue, and extracted body text, with optional filters for source, DOI,
//! arXiv id, publication-year range, and text availability. Results identify the
//! underlying object and its provenance, so every hit can be traced back to a stored,
//! checksummed PDF. Machine-readable output is available with `--json`.

use crate::cli::discover::load_config;
use crate::cli::Flags;
use paperengine::index::{document_count, search_library, SearchFilters};
use paperengine::library::LibraryLayout;
use std::error::Error;

pub fn run(args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let flags = Flags::parse(args)?;
    let config = load_config(&flags)?;
    config.validate()?;

    let library_dir = flags.require("library")?;
    let library = LibraryLayout::new(&library_dir);
    let query = flags.optional("query").unwrap_or_default();
    let limit = flags.parse_optional::<usize>("limit")?.unwrap_or(10);
    if limit == 0 || limit > 1_000 {
        return Err("--limit must be between 1 and 1000".into());
    }

    let filters = SearchFilters {
        source_id: flags.optional("source"),
        doi: flags.optional("doi"),
        arxiv_id: flags.optional("arxiv-id"),
        year_min: flags.parse_optional::<u64>("year-min")?,
        year_max: flags.parse_optional::<u64>("year-max")?,
        text_only: flags.has("text-only"),
    };
    if query.trim().is_empty() && filters == SearchFilters::default() {
        return Err(
            "provide --query or at least one filter (--source, --doi, --arxiv-id, --year-min, --year-max, --text-only)"
                .into(),
        );
    }

    let hits = search_library(&library, &query, &filters, limit)?;

    if flags.has("json") {
        let summary = serde_json::json!({
            "library_root": library_dir,
            "query": query,
            "limit": limit,
            "indexed_documents": document_count(&library)?,
            "hits": hits,
        });
        println!("{}", serde_json::to_string_pretty(&summary)?);
        return Ok(());
    }

    println!(
        "library: {library_dir} ({} indexed documents)",
        document_count(&library)?
    );
    if hits.is_empty() {
        println!("no matches for the given query and filters");
        return Ok(());
    }
    for (position, hit) in hits.iter().enumerate() {
        let year = hit
            .publication_year
            .map(|year| year.to_string())
            .unwrap_or_else(|| "----".to_owned());
        let first_author = hit
            .authors
            .first()
            .map(|author| {
                let mut name = author.clone();
                if let Some(index) = name.find(';') {
                    name.truncate(index);
                }
                name
            })
            .unwrap_or_else(|| "<no authors>".to_owned());
        println!(
            "{:>2}. [{}] {} ({}) — {}",
            position + 1,
            hit.source_id,
            if hit.title.is_empty() {
                "<untitled>"
            } else {
                &hit.title
            },
            year,
            first_author
        );
        let mut provenance = vec![format!(
            "sha256={}",
            &hit.object_sha[..12.min(hit.object_sha.len())]
        )];
        if let Some(doi) = &hit.doi {
            provenance.push(format!("doi:{doi}"));
        }
        if let Some(arxiv) = &hit.arxiv_id {
            provenance.push(format!("arxiv:{arxiv}"));
        }
        provenance.push(format!(
            "pages={}",
            hit.page_count
                .map(|pages| pages.to_string())
                .unwrap_or_else(|| "?".to_owned())
        ));
        provenance.push(format!("bytes={}", hit.byte_length));
        if hit.text_available {
            provenance.push("text=yes".to_owned());
        } else {
            provenance.push("text=no".to_owned());
        }
        println!("    {}", provenance.join(" "));
    }
    println!("{} hit(s) (limit {limit})", hits.len());
    Ok(())
}
