//! Sessions: rewind, checkpoints, diff, cost, rename, fork, export (track F2): Rewind, ListCheckpoints, SessionDiff, SessionCost, RenameSession, ForkSession, ExportSession.
//!
//! Seam stub: `client.rs` already routes these requests here; until track F2 lands every one is
//! answered with an `Error` naming the track. Checkpoints are the shadow repo in `checkpoint.rs`; per-call rows come from a `TurnObserver`. Replace the body of
//! [`dispatch`]; keep its signature.

use std::sync::Arc;

use marlowe_proto::Request;

use crate::daemon::Core;

pub(crate) fn dispatch(core: &Arc<Core>, client: u64, req: Request) {
    crate::client::not_yet(core, client, &req, "F2");
}
