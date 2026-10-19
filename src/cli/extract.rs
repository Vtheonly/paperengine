//! `paperengine extract` — provenance-aware text and metadata extraction (#31).
//!
//! Extracts text and document metadata from every finalized object in a local library
//! through the real PDF parser, writes content-addressed text artifacts plus extraction
//! provenance records, and appends a JSONL run report under `reports/`. Completed
//! extractions are skipped on re-run unless `--force` is set, so the command is
//! idempotent; parse failures are retried on the next run by design.

use crate::cli::discover::load_config;
use crate::cli::Flags;
use paperengine::extract::{run_extraction, ExtractionReportEntry};
use paperengine::library::LibraryLayout;
use std::error::Error;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn run(args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let flags = Flags::parse(args)?;
    let mut config = load_config(&flags)?;
    if let Some(max_text_bytes) = flags.parse_optional::<u64>("max-text-bytes")? {
        config.extraction.max_text_bytes = max_text_bytes;
    }
    if let Some(max_objects) = flags.parse_optional::<usize>("max-objects")? {
        config.extraction.max_objects = max_objects;
    }
    config.validate()?;
    let extraction_config = config.extraction_config();

    let library_dir = flags.require("library")?;
    let force = flags.has("force");
    let library = LibraryLayout::new(&library_dir);
    library.create()?;

    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let report_path = library.reports_dir().join(format!("{now}-extract.jsonl"));
    let mut report = BufWriter::new(File::create(&report_path)?);
    let json_mode = flags.has("json");

    eprintln!(
        "extracting text from {library_dir} (max {} objects, max {} text bytes per object)…",
        extraction_config.max_objects, extraction_config.max_text_bytes
    );

    let summary = run_extraction(
        &library,
        &extraction_config,
        force,
        now,
        |entry: &ExtractionReportEntry| {
            let line = serde_json::to_string(entry)?;
            if json_mode {
                // Keep stdout machine-readable in --json mode.
                eprintln!("{line}");
            } else {
                println!("{line}");
            }
            report.write_all(line.as_bytes())?;
            report.write_all(b"\n")
        },
    )?;
    report.flush()?;

    if json_mode {
        let summary_json = serde_json::json!({
            "library_root": library_dir,
            "objects_examined": summary.objects_examined,
            "extracted": summary.extracted,
            "empty": summary.empty,
            "failed": summary.failed,
            "skipped": summary.skipped,
            "total_text_bytes": summary.total_text_bytes,
            "report": report_path.to_string_lossy(),
        });
        println!("{}", serde_json::to_string_pretty(&summary_json)?);
        return Ok(());
    }

    eprintln!(
        "summary: examined={} extracted={} empty={} failed={} skipped={} text_bytes={}",
        summary.objects_examined,
        summary.extracted,
        summary.empty,
        summary.failed,
        summary.skipped,
        summary.total_text_bytes
    );
    eprintln!("report: {}", report_path.display());
    if summary.failed > 0 {
        eprintln!("note: failed objects are retried on the next extraction run");
    }
    Ok(())
}
