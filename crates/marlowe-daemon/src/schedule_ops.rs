//! `/loop` and scheduled tasks (track F): ListSchedules, AddSchedule, RemoveSchedule.
//!
//! Seam stub: `client.rs` already routes these requests here; until track F lands every one is
//! answered with an `Error` naming the track. A `schedules` table; firing starts a non-blocking turn like an `Ask`. Replace the body of
//! [`dispatch`]; keep its signature.

use std::sync::Arc;

use marlowe_proto::Request;

use crate::daemon::Core;

pub(crate) fn dispatch(core: &Arc<Core>, client: u64, req: Request) {
    crate::client::not_yet(core, client, &req, "F");
}

/// Re-arms stored schedules at daemon start (`boot.rs`). Must not block.
pub(crate) fn on_boot(_core: &Arc<Core>) {}
