//! The conductor's second round (track F3): SetFast, AnswerPlan, SuggestRules.
//!
//! Seam stub: `client.rs` already routes these requests here; until track F3 lands every one is
//! answered with an `Error` naming the track. Replace the body of [`dispatch`]; keep its
//! signature. `stack.rs` calls [`services`] for every session (the plan-file asker, the fast-mode
//! switch), so F3 edits neither `conductor_ops.rs` nor `session.rs` to plug in.

use std::sync::Arc;

use marlowe_agent::{Conversation, Services};
use marlowe_proto::Request;

use crate::daemon::Core;

pub(crate) fn dispatch(core: &Arc<Core>, client: u64, req: Request) {
    crate::client::not_yet(core, client, &req, "F3");
}

/// F3's services for one session.
pub(crate) fn services(_core: &Arc<Core>, _convo: &Conversation, services: Services) -> Services {
    services
}
