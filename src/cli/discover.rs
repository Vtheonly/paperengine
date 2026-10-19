//! `paperengine discover` — bounded, download-free metadata discovery (#9 partial).
//!
//! Search-based sources take `--query`. Harvest-based OAI-PMH sources (ids ending in
//! `-oai`, #13) have no free-text search: they take an empty query and are bounded by
//! `--from-date`/`--until-date`, an optional `--set`, and the page/record budgets
//! instead. The adapter refuses a non-empty query on a harvest source with a typed
//! configuration error rather than silently ignoring it.

use crate::cli::Flags;
use paperengine::config::PaperEngineConfig;
use paperengine::sources::work_record::WorkRecord;
use paperengine::sources::{builtin_registry, run_discovery, DiscoveryBudget, DiscoveryQuery};
use std::error::Error;
use std::fs::File;
use std::io::{BufWriter, Write};

/// Harvest-based source ids registered by the OAI-PMH adapters (#13).
const HARVEST_SUFFIX: &str = "-oaipmh";

fn is_harvest_source(source_id: &str) -> bool {
    source_id.ends_with(HARVEST_SUFFIX)
}

pub fn run(args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let flags = Flags::parse(args)?;
    let config = load_config(&flags)?;
    let source = flags.require("source")?;
    let harvest_source = is_harvest_source(&source);

    // Query text: required and non-empty for search sources; optional for harvest
    // sources, where a non-empty value is refused by the adapter with a typed error.
    let query_text = match flags.optional("query") {
        Some(text) if !text.trim().is_empty() => text,
        Some(_) | None if harvest_source => String::new(),
        Some(_) | None => {
            return Err(format!(
                "--query is required for search source `{source}`; harvest sources \
                 (`-oaipmh` suffix) take a date window instead: an ISO date as --query, \
                 or --from-date/--until-date (plus an optional --set)"
            )
            .into());
        }
    };

    let sources = config.builtin_sources_config();
    let (registry, notes) = builtin_registry(&sources);
    for note in &notes {
        eprintln!("warning: {note}");
    }
    let adapter = registry.get(&source).ok_or_else(|| {
        format!(
            "unknown source `{source}`; built-ins: {}",
            registry.source_ids().join(", ")
        )
    })?;

    let budget = DiscoveryBudget {
        max_records: flags
            .parse_optional::<usize>("max-records")?
            .unwrap_or(config.discovery.max_records),
        max_pages: flags
            .parse_optional::<u32>("max-pages")?
            .unwrap_or(config.discovery.max_pages),
    };
    let page_size = flags
        .parse_optional::<u16>("page-size")?
        .unwrap_or(config.discovery.default_page_size);
    let from_date = flags.optional("from-date");
    let until_date = flags.optional("until-date");
    let set = flags.optional("set");
    let query = DiscoveryQuery {
        text: query_text,
        page_size,
        from_date,
        until_date,
        set,
    };

    // Stream records as JSONL either to a file (flushed per page, so an interrupted run
    // keeps completed pages) or to stdout.
    let out_path = flags.optional("out");
    let stdout_writer;
    let file_writer;
    let mut writer: Box<dyn Write> = match &out_path {
        Some(path) => {
            if let Some(parent) = std::path::Path::new(path).parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            file_writer = BufWriter::new(File::create(path)?);
            Box::new(file_writer)
        }
        None => {
            stdout_writer = BufWriter::new(std::io::stdout().lock());
            Box::new(stdout_writer)
        }
    };

    if harvest_source {
        eprintln!(
            "harvesting from `{}` (max {} records, max {} pages{}{}{})…",
            source,
            budget.max_records,
            budget.max_pages,
            query
                .from_date
                .as_deref()
                .map(|value| format!(", from {value}"))
                .unwrap_or_default(),
            query
                .until_date
                .as_deref()
                .map(|value| format!(", until {value}"))
                .unwrap_or_default(),
            query
                .set
                .as_deref()
                .map(|value| format!(", set {value}"))
                .unwrap_or_default(),
        );
    } else {
        eprintln!(
            "discovering from `{}` (max {} records, max {} pages)…",
            source, budget.max_records, budget.max_pages
        );
    }
    let summary = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(run_discovery(
            adapter,
            &query,
            &budget,
            |page: &[WorkRecord]| {
                for record in page {
                    serde_json::to_writer(&mut writer, record)?;
                    writer.write_all(b"\n")?;
                }
                writer.flush()
            },
        ))?;

    writer.flush()?;
    if let Some(path) = &out_path {
        eprintln!("wrote {} records to {path}", summary.records_returned);
    }
    eprintln!(
        "summary: source={} records={} pages={} truncated={}",
        summary.source_id, summary.records_returned, summary.pages_fetched, summary.truncated
    );
    Ok(())
}

pub(crate) fn load_config(flags: &Flags) -> Result<PaperEngineConfig, Box<dyn Error>> {
    let path = flags
        .optional("config")
        .unwrap_or_else(|| paperengine::config::DEFAULT_CONFIG_FILE.to_owned());
    let mut config = PaperEngineConfig::load_optional(std::path::Path::new(&path))?;
    if let Some(email) = flags.optional("email") {
        config.contact_email = Some(email);
        config.validate()?;
    }
    Ok(config)
}
