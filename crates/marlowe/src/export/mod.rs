//! `marlowe export` (track M). `marlowe export trajectories`: opt-in per project; keep rule (crew merged /
//! conductor `/good`); HF JSONL + step DPO; layered scrub, dry-run and manifest (design 3.10).
//!
//! Seam stub: `main.rs` already dispatches here; replace the body of [`run`], keep its signature.

use std::path::Path;

pub fn run(_home: &Path, _args: &[String]) -> Result<i32, String> {
    eprintln!("marlowe export is not available yet (M4.6 track M)");
    Ok(1)
}
