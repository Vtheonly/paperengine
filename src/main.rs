//! Outer CLI adapter for the in-memory policy kernel.
//!
//! This binary intentionally does not fetch or fabricate source URLs. It accepts normalized
//! candidate facts on stdin, runs the bounded engine, and emits decision events as JSON Lines.
//! Real network adapters remain a separate roadmap slice and must enforce source policy again.

use paperengine::concurrency::ring::{EngineRingBuffer, TaskEnvelope, EVENT_FLAG_FINAL};
use paperengine::runtime::orchestrator::{EngineOrchestrator, OrchestratorCursor};
use paperengine::{normalize_candidate, AcquisitionEngine, CandidateInput, PolicyConfig};
use std::error::Error;
use std::io::{self, BufRead, Write};

fn parse_u32(value: &str) -> Result<u32, Box<dyn Error>> {
    if let Some(hex) = value.strip_prefix("0x") {
        Ok(u32::from_str_radix(hex, 16)?)
    } else {
        Ok(value.parse()?)
    }
}

fn action_name(action: u16) -> &'static str {
    match action {
        0 => "enrich_metadata",
        1 => "resolve_candidate_route",
        2 => "validate_authorized_candidate",
        3 => "acquire_verified_authorized_payload",
        _ => "unknown",
    }
}

fn emit_event(out: &mut impl Write, event: TaskEnvelope) -> io::Result<()> {
    writeln!(
        out,
        "{{\"kind\":\"decision\",\"paper_id\":{},\"action\":\"{}\",\"score\":{},\"source_id\":{},\"policy_version\":{},\"candidate_index\":{},\"final\":{}}}",
        event.paper_id,
        action_name(event.action_type),
        event.score,
        event.source_id,
        event.policy_version,
        event.candidate_index,
        if event.flags & EVENT_FLAG_FINAL != 0 { "true" } else { "false" },
    )
}

fn run_rank(args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let mut reference_year = 2026_u16;
    let mut policy_version = 1_u32;
    let mut args = args;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--year" => reference_year = args.next().ok_or("--year requires a value")?.parse()?,
            "--policy-version" => {
                policy_version = args.next().ok_or("--policy-version requires a value")?.parse()?
            }
            _ => return Err(format!("unknown argument: {arg}").into()),
        }
    }

    let stdin = io::stdin();
    let mut frontier = Vec::new();
    for (line_number, line) in stdin.lock().lines().enumerate() {
        let line = line?;
        let row = line.trim();
        if row.is_empty() || row.starts_with('#') { continue; }

        let fields: Vec<&str> = row.split(',').map(str::trim).collect();
        if fields.len() != 7 {
            return Err(format!(
                "line {}: expected 7 comma-separated numeric fields: paper_id,citations,venue_rank,year,size_kib,source_id,flags",
                line_number + 1
            ).into());
        }

        let input = CandidateInput {
            paper_id: fields[0].parse()?,
            citation_count: fields[1].parse()?,
            venue_rank: fields[2].parse()?,
            year: fields[3].parse()?,
            estimated_size_kib: fields[4].parse()?,
            source_id: fields[5].parse()?,
            flags: parse_u32(fields[6])?,
        };
        frontier.push(normalize_candidate(input, reference_year)?);
    }

    let config = PolicyConfig::new(reference_year, policy_version)?;
    let engine = AcquisitionEngine::new(config)?;
    let loop_engine = EngineOrchestrator::new(256)?;
    let mut cursor = OrchestratorCursor::new(&engine, &frontier);
    let mut ring = EngineRingBuffer::<1024>::new();
    let (mut producer, mut consumer) = ring.split();
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    let mut found_final = false;

    while !cursor.is_complete() {
        let progress = loop_engine.run_turn(&engine, &frontier, &mut cursor, &mut producer)?;
        while let Some(event) = consumer.try_dequeue() {
            found_final |= event.flags & EVENT_FLAG_FINAL != 0;
            emit_event(&mut out, event)?;
        }
        if progress.backpressured {
            // The consumer above frees slots before the next bounded turn.
            continue;
        }
    }

    if !found_final {
        writeln!(out, "{{\"kind\":\"decision\",\"status\":\"no_eligible_candidate\"}}")?;
    }
    out.flush()?;
    Ok(())
}

fn main() {
    let mut args = std::env::args().skip(1);
    let result = match args.next().as_deref() {
        Some("rank") => run_rank(args),
        Some("--version") | Some("-V") => {
            println!("paperengine {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => Err(
            "usage: paperengine rank [--year YEAR] [--policy-version VERSION] < candidates.csv"
                .into(),
        ),
    };

    if let Err(error) = result {
        eprintln!("paperengine: {error}");
        std::process::exit(2);
    }
}
