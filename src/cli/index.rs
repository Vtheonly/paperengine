//! `paperengine index` — incremental metadata + extracted-text indexing (#32).
//!
//! Indexes every provenance manifest and its extraction record into the library's
//! Tantivy index. Incremental: unchanged documents are skipped, changed documents are
//! updated, vanished objects are deleted; everything commits once at the end of the run.
//! The index is derived state — `--rebuild` drops and recreates it, and canonical
//! metadata is never modified.

use crate::cli::discover::load_config;
use crate::cli::Flags;
use paperengine::index::{run_indexing, IndexingReportEntry};
use paperengine::library::LibraryLayout;
use std::error::Error;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn run(args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let flags = Flags::parse(args)?;
    let config = load_config(&flags)?;
    config.validate()?;
    let indexing_config = config.indexing_config();

    let library_dir = flags.require("library")?;
    let rebuild = flags.has("rebuild");
    let library = LibraryLayout::new(&library_dir);
    library.create()?;

    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let report_path = library.reports_dir().join(format!("{now}-index.jsonl"));
    let mut report = BufWriter::new(File::create(&report_path)?);
    let json_mode = flags.has("json");

    eprintln!(
        "indexing {library_dir} into {} ({} rebuild)…",
        library.index_dir().display(),
        if rebuild { "with" } else { "without" }
    );

    let summary = run_indexing(
        &library,
        &indexing_config,
        rebuild,
        |entry: &IndexingReportEntry| {
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
            "manifests_examined": summary.manifests_examined,
            "added": summary.added,
            "updated": summary.updated,
            "skipped": summary.skipped,
            "deleted": summary.deleted,
            "failed": summary.failed,
            "report": report_path.to_string_lossy(),
        });
        println!("{}", serde_json::to_string_pretty(&summary_json)?);
        return Ok(());
    }

    eprintln!(
        "summary: manifests={} added={} updated={} skipped={} deleted={} failed={}",
        summary.manifests_examined,
        summary.added,
        summary.updated,
        summary.skipped,
        summary.deleted,
        summary.failed
    );
    eprintln!("report: {}", report_path.display());
    eprintln!("next: `paperengine search --library {library_dir} --query …`");
    if summary.failed > 0 {
        eprintln!("note: failed manifests are retried on the next indexing run");
    }
    Ok(())
}
