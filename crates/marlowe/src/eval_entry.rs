//! `marlowe eval` (track L0). `marlowe eval ...`: the coding-eval entry (own + polyglot runners, baseline
//! runs) that `eval/coding/` drives (design section 6). The LongMemEval adapter stays at
//! `--eval-adapter` (`eval/`).
//!
//! Seam stub: `main.rs` already dispatches here; replace the body of [`run`], keep its signature.

use std::path::Path;

pub fn run(_home: &Path, _args: &[String]) -> Result<i32, String> {
    eprintln!("marlowe eval is not available yet (M4.6 track L0)");
    Ok(1)
}
