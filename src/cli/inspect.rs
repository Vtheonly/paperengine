//! `paperengine inspect` — list the local library's finalized objects and provenance.

use crate::cli::Flags;
use paperengine::library::LibraryLayout;
use std::error::Error;

pub fn run(args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let flags = Flags::parse(args)?;
    let library_dir = flags.require("library")?;
    let library = LibraryLayout::new(&library_dir);

    let objects = library.list_objects()?;
    let manifests = library.list_manifests()?;

    if flags.has("json") {
        let summary = serde_json::json!({
            "library_root": library_dir,
            "object_count": objects.len(),
            "total_bytes": objects.iter().map(|(_, length)| length).sum::<u64>(),
            "objects": objects
                .iter()
                .map(|(path, length)| serde_json::json!({
                    "path": path.to_string_lossy(),
                    "bytes": length,
                }))
                .collect::<Vec<_>>(),
            "manifests": manifests,
        });
        println!("{}", serde_json::to_string_pretty(&summary)?);
        return Ok(());
    }

    let total_bytes: u64 = objects.iter().map(|(_, length)| length).sum();
    println!("library: {}", library_dir);
    println!("objects: {} ({})", objects.len(), human_bytes(total_bytes));
    if !objects.is_empty() {
        println!();
        for (path, length) in &objects {
            println!("  {:>10}  {}", human_bytes(*length), path.display());
        }
    }
    println!();
    println!("provenance manifests: {}", manifests.len());
    for manifest in &manifests {
        let identifiers: Vec<String> = manifest
            .work
            .identifiers
            .iter()
            .map(|identifier| format!("{}:{}", identifier.kind.as_str(), identifier.value))
            .collect();
        println!(
            "  {} [{}] pages={} sha256={}…",
            if manifest.work.title.is_empty() {
                "<untitled>"
            } else {
                &manifest.work.title
            },
            identifiers.join(" "),
            manifest.content.page_count,
            &manifest.content.sha256_hex[..12.min(manifest.content.sha256_hex.len())]
        );
    }
    Ok(())
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0_usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
