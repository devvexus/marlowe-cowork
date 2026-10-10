//! Background jobs and the `!` shell (track B): ListJobs, KillJob, UserShell.
//!
//! Seam stub: `client.rs` already routes these requests here; until track B lands every one is
//! answered with an `Error` naming the track. Jobs run through the tool host's workspace port
//! (`Workspace::spawn`); their events are `JobStarted`, `JobOutput` and `JobExited`. Replace the
//! body of [`dispatch`]; keep its signature.

use std::sync::Arc;

use marlowe_proto::Request;

use crate::daemon::Core;

pub(crate) fn dispatch(core: &Arc<Core>, client: u64, req: Request) {
    crate::client::not_yet(core, client, &req, "B");
}
