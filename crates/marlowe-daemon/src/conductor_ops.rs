//! Conductor controls (track F): Compact, AddDir, SwitchModel, ListCommands, ListAgentTypes, AnswerQuestion, ReloadInstructions, DetachCall.
//!
//! Seam stub: `client.rs` already routes these requests here; until track F lands every one is
//! answered with an `Error` naming the track. Each acts on the session runner (`session.rs`) at its next step boundary; nothing here blocks the client reader thread. `DetachCall` hands a running foreground shell to B's job table. Replace the body of
//! [`dispatch`]; keep its signature.

use std::sync::Arc;

use marlowe_agent::{Conversation, Services, ToolMiddleware};
use marlowe_proto::Request;

use crate::daemon::Core;

pub(crate) fn dispatch(core: &Arc<Core>, client: u64, req: Request) {
    crate::client::not_yet(core, client, &req, "F");
}

/// F's loop middleware for one session (todo and verify reminders), added by `stack.rs`.
pub(crate) fn middleware(_core: &Arc<Core>, _convo: &Conversation) -> Vec<Arc<dyn ToolMiddleware>> {
    Vec::new()
}

/// F's services for one session (the `ask_user` channel, the subagent runner, the
/// `WorktreeProvider`), added by `stack.rs`. The agent group's tools read them from
/// `CallCtx::scope`.
pub(crate) fn services(_core: &Arc<Core>, _convo: &Conversation, services: Services) -> Services {
    services
}
