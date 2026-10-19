//! `paperengine discover` — bounded, download-free metadata discovery (#9 partial).

use crate::cli::Flags;
use paperengine::config::PaperEngineConfig;
use paperengine::sources::work_record::WorkRecord;
use paperengine::sources::{builtin_registry, run_discovery, DiscoveryBudget, DiscoveryQuery};
use std::error::Error;
use std::fs::File;
use std::io::{BufWriter, Write};

pub fn run(args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let flags = Flags::parse(args)?;
    let config = load_config(&flags)?;
    let source = flags.require("source")?;
    let query_text = flags.require("query")?;

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
    let query = DiscoveryQuery {
        text: query_text,
        page_size,
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

    eprintln!(
        "discovering from `{}` (max {} records, max {} pages)…",
        source, budget.max_records, budget.max_pages
    );
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
