//! Ingests user turns into a cowork home through `Mind::ingest_turn`, the path the daemon's
//! `before_turn` uses, so `marlowe memory probe` can be tried on a small store without a model.
//!
//!   cargo run -p marlowe-mind --example seed_turns -- <home> [--workspace DIR] <text>...
//!
//! Each text is one user turn in session `seed`. Stop the daemon for that home first: the
//! profile lock admits one opener.

use std::path::PathBuf;

use marlowe_mind::{Mind, TurnIn, TurnRole};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let home = PathBuf::from(args.next().ok_or("usage: seed_turns <home> [--workspace DIR] <text>...")?);
    let (mut workspace, mut texts) = (None, Vec::new());
    while let Some(a) = args.next() {
        if a == "--workspace" {
            workspace = Some(args.next().ok_or("--workspace needs a value")?);
        } else {
            texts.push(a);
        }
    }
    let mind = Mind::open(&home)?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis() as i64;
    for (i, text) in texts.iter().enumerate() {
        mind.ingest_turn(TurnIn {
            session: "seed".into(),
            workspace: workspace.clone(),
            role: TurnRole::User,
            text: text.clone(),
            at_ms: now + i as i64,
        })?;
        println!("ingested: {text}");
    }
    let stats = mind.stats()?;
    println!("beliefs {} episodes {}", stats.beliefs, stats.episodes);
    mind.shutdown()?;
    Ok(())
}
