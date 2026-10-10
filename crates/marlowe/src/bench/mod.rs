//! `marlowe bench` (track L). `marlowe bench <model> [--suite ...] [--tier ...] [--vs ...]`: tool-call and edit
//! reliability per model and edit format; writes `edit_formats` to the model library only with
//! `--apply`; refuses valiance until it is enabled (design 3.10).
//!
//! Seam stub: `main.rs` already dispatches here; replace the body of [`run`], keep its signature.

use std::path::Path;

pub fn run(_home: &Path, _args: &[String]) -> Result<i32, String> {
    eprintln!("marlowe bench is not available yet (M4.6 track L)");
    Ok(1)
}
