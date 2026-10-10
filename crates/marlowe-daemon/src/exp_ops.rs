//! Experiments (track J2): ListExperiments.
//!
//! Seam stub: `client.rs` already routes these requests here; until track J2 lands every one is
//! answered with an `Error` naming the track. Reads the experiments table the `train_launch` tool writes. Replace the body of
//! [`dispatch`]; keep its signature.

use std::sync::Arc;

use marlowe_proto::Request;

use crate::daemon::Core;

pub(crate) fn dispatch(core: &Arc<Core>, client: u64, req: Request) {
    crate::client::not_yet(core, client, &req, "J2");
}

/// Re-arms experiment deadlines at daemon start (`boot.rs`). Must not block.
pub(crate) fn on_boot(_core: &Arc<Core>) {}
