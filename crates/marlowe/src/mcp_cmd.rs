//! `marlowe mcp` (track K). `marlowe mcp add|list|remove` over `.mcp.json` and the settings layers
//! (design 2.6 X15); the daemon side is `marlowe-daemon/src/mcp_ops.rs`.
//!
//! Seam stub: `main.rs` already dispatches here; replace the body of [`run`], keep its signature.

use std::path::Path;

pub fn run(_home: &Path, _args: &[String]) -> Result<i32, String> {
    eprintln!("marlowe mcp is not available yet (M4.6 track K)");
    Ok(1)
}
