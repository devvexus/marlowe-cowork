//! Permission modes, rules, hooks and settings (track E): SetMode, ListRules, AddRule, RemoveRule, ListHooks, TrustHooks, GetSettings, SetSetting.
//!
//! Seam stub: `client.rs` already routes these requests here; until track E lands every one is
//! answered with an `Error` naming the track. Rules and settings come from `marlowe-settings`; the adjudication itself is `marlowe-permission` (rules, `shell/`) applied by a `ToolMiddleware` and the approval gate in `marlowe-agent`. Replace the body of
//! [`dispatch`]; keep its signature.

use std::sync::Arc;

use marlowe_agent::{Conversation, Services, ToolMiddleware, TurnObserver};
use marlowe_proto::Request;

use crate::daemon::Core;

pub(crate) fn dispatch(core: &Arc<Core>, client: u64, req: Request) {
    crate::client::not_yet(core, client, &req, "E");
}

/// E's loop middleware for one session (hooks, modes, rules), added by `stack.rs`.
pub(crate) fn middleware(_core: &Arc<Core>, _convo: &Conversation) -> Vec<Arc<dyn ToolMiddleware>> {
    Vec::new()
}

/// E's observers for one session (SessionStart and Notification hooks), added by `stack.rs`.
pub(crate) fn observers(_core: &Arc<Core>, _convo: &Conversation) -> Vec<Arc<dyn TurnObserver>> {
    Vec::new()
}

/// E's services for one session (the hook runner), added by `stack.rs`.
pub(crate) fn services(_core: &Arc<Core>, _convo: &Conversation, services: Services) -> Services {
    services
}
