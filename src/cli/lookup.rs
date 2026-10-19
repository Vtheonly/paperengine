//! `paperengine lookup` — resolve one DOI through a source adapter (no downloads).

use crate::cli::discover::load_config;
use crate::cli::Flags;
use paperengine::sources::builtin_registry;
use std::error::Error;
use std::io::Write;

pub fn run(args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let flags = Flags::parse(args)?;
    let config = load_config(&flags)?;
    let source = flags.require("source")?;
    let doi = flags.require("doi")?;

    let (registry, notes) = builtin_registry(&config.builtin_sources_config());
    for note in &notes {
        eprintln!("warning: {note}");
    }
    let adapter = registry.get(&source).ok_or_else(|| {
        format!(
            "unknown source `{source}`; built-ins: {}",
            registry.source_ids().join(", ")
        )
    })?;

    eprintln!("resolving DOI {doi} through `{source}`…");
    let record = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(adapter.lookup_doi(&doi))?;

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    match record {
        Some(record) => {
            serde_json::to_writer(&mut out, &record)?;
            out.write_all(b"\n")?;
            eprintln!(
                "resolved: {} (pdf: {})",
                record.title,
                record.oa_pdf_url.as_deref().unwrap_or("<none reported>")
            );
        }
        None => {
            eprintln!("source reports no record for this DOI");
        }
    }
    out.flush()?;
    Ok(())
}
