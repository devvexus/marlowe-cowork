//! Daemon start hooks: one per owning track, each declared in that track's own file. They run
//! once, after the core, the Sentinel and the crew are up and before the first client is
//! accepted. A hook must not block: slow work (connecting MCP servers) goes on its own thread.
//!
//! | Hook | Track |
//! |---|---|
//! | `schedule_ops::on_boot` | F: re-arm `/loop` schedules |
//! | `exp_ops::on_boot` | J2: re-arm experiment deadlines |
//! | `mcp_ops::on_boot` | K: connect configured MCP servers |

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use crate::daemon::Core;

/// The boot hooks, in order.
const HOOKS: &[fn(&Arc<Core>)] = &[crate::schedule_ops::on_boot, crate::exp_ops::on_boot, crate::mcp_ops::on_boot];

/// Runs every boot hook. A panicking hook is reported in the daemon log and the daemon starts
/// anyway.
pub(crate) fn run(core: &Arc<Core>) {
    for hook in HOOKS {
        if catch_unwind(AssertUnwindSafe(|| hook(core))).is_err() {
            eprintln!("marlowe: a start-up hook failed; continuing without it");
        }
    }
}
