//! `paperengine acquire` — the end-to-end acquisition workflow.
//!
//! Reads a discovery JSONL file, re-checks URL policy immediately before every request,
//! downloads permitted OA PDFs through the bounded resumable downloader, structurally
//! validates them, quarantines invalid payloads, and finalizes valid objects into
//! content-addressed storage with provenance manifests and a run report.

use crate::cli::discover::load_config;
use crate::cli::Flags;
use paperengine::library::{
    acquisition_downloader, run_acquisition, AcquisitionReportEntry, LibraryLayout,
};
use paperengine::sources::work_record::WorkRecord;
use std::error::Error;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn run(args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let flags = Flags::parse(args)?;
    let mut config = load_config(&flags)?;
    if let Some(max_downloads) = flags.parse_optional::<usize>("max-downloads")? {
        config.acquisition.max_downloads = max_downloads;
    }
    if flags.has("allow-http") {
        config.acquisition.allow_http = true;
    }
    config.validate()?;

    let input = flags.require("from")?;
    let library_dir = flags
        .optional("library")
        .unwrap_or_else(|| "paperengine-library".to_owned());
    let library = LibraryLayout::new(&library_dir);

    let input_file = File::open(&input)?;
    let reader = BufReader::new(input_file);
    let mut records = Vec::new();
    for (line_number, line) in reader.lines().enumerate() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let record: WorkRecord = serde_json::from_str(trimmed).map_err(|error| {
            format!(
                "{}:{}: invalid work record: {error}",
                input,
                line_number + 1
            )
        })?;
        records.push(record);
    }

    let acquisition_config = config.acquisition_config();
    let downloader = acquisition_downloader(&acquisition_config)?;

    library.create()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let report_path = library.reports_dir().join(format!("{now}-acquire.jsonl"));
    let mut report = BufWriter::new(File::create(&report_path)?);

    eprintln!(
        "acquiring from {input} into {} (max {} downloads, max {} bytes per file)…",
        library_dir, acquisition_config.max_downloads, acquisition_config.max_bytes_per_file
    );

    let summary = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(run_acquisition(
            records.iter(),
            &library,
            &acquisition_config,
            &downloader,
            now,
            |entry: &AcquisitionReportEntry| {
                let line = serde_json::to_string(entry)?;
                println!("{line}");
                report.write_all(line.as_bytes())?;
                report.write_all(b"\n")
            },
        ))?;
    report.flush()?;

    eprintln!(
        "summary: examined={} acquired={} (deduplicated {}) quarantined={} failed={} skipped={} bytes={}",
        summary.records_examined,
        summary.acquired,
        summary.deduplicated,
        summary.quarantined,
        summary.failed,
        summary.skipped,
        summary.total_bytes
    );
    eprintln!("report: {}", report_path.display());
    if summary.download_budget_exhausted {
        eprintln!("note: run budget exhausted; re-run to continue with remaining records");
    }
    Ok(())
}
