//! Pull requests (track J): OpenPr.
//!
//! Seam stub: `client.rs` already routes these requests here; until track J lands every one is
//! answered with an `Error` naming the track. Push and `gh pr create` run here after one approval, never from a crew shell; `GH_TOKEN` reaches the `gh` child only through `ChildSpec::env_allow`. Replace the body of
//! [`dispatch`]; keep its signature.

use std::sync::Arc;

use marlowe_proto::Request;

use crate::daemon::Core;

pub(crate) fn dispatch(core: &Arc<Core>, client: u64, req: Request) {
    crate::client::not_yet(core, client, &req, "J");
}
