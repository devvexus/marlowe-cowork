//! Imports an old harness profile into a throwaway home and prints the report.
//!
//!   cargo run -p marlowe-mind --example legacy_dry_run -- <old-profile-dir>
//!
//! The old dir is only read (the import copies it to a temp dir first). No models are loaded.

use std::path::PathBuf;

use marlowe_mind::{Mind, MindConfig, RecallQuery};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let old = PathBuf::from(std::env::args().nth(1).ok_or("usage: legacy_dry_run <old-profile-dir>")?);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos();
    let home = std::env::temp_dir().join(format!("marlowe-legacy-dry-run-{nanos}"));
    let cfg = MindConfig { models_dir: Some(home.join("no-models")), ..MindConfig::default() };
    let mind = Mind::open_with(&home, cfg)?;
    let status = mind.legacy_status(Some(&old))?;
    println!("status: {status:?}");
    let report = mind.import_legacy(&old, &mut |p| eprintln!("progress: {} read, {} imported", p.read, p.imported))?;
    println!("{report:#?}");
    let again = mind.import_legacy(&old, &mut |_| {})?;
    println!("second import: imported {} skipped {}", again.imported, again.skipped_duplicates);
    let stats = mind.stats()?;
    println!("beliefs {} episodes {}", stats.beliefs, stats.episodes);
    for q in ["workspace", "equations"] {
        for h in mind.recall(RecallQuery { text: q.into(), scope: None, limit: 3 })? {
            println!("{q}: {} · {} · {:?}", h.kind.label(), h.trust.label(), h.text.chars().take(70).collect::<String>());
        }
    }
    mind.shutdown()?;
    drop(mind);
    std::fs::remove_dir_all(&home)?;
    Ok(())
}
