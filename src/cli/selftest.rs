//! `paperengine self-test` — offline capability report for every registered adapter.

use crate::cli::discover::load_config;
use crate::cli::Flags;
use paperengine::sources::builtin_registry;
use std::error::Error;

pub fn run(args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let flags = Flags::parse(args)?;
    let config = load_config(&flags)?;
    let (registry, notes) = builtin_registry(&config.builtin_sources_config());

    for report in registry.self_tests() {
        println!("{}", serde_json::to_string_pretty(&report)?);
    }
    for note in &notes {
        eprintln!("warning: {note}");
    }
    eprintln!(
        "contract version: {}",
        paperengine::sources::SOURCE_ADAPTER_CONTRACT_VERSION
    );
    Ok(())
}
