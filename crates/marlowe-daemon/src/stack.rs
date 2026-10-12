//! What every session's conductor is built with beyond the agent itself: the tool host's wrappers,
//! loop middleware, observers and services, one constructor per owning track, each declared in
//! that track's own file. This file only concatenates them, so no track edits `session.rs` or
//! another track's file to plug in.
//!
//! | Hook | Track constructors |
//! |---|---|
//! | [`tools`] | `mcp_ops::compose` (K: the MCP host beside the built-ins) |
//! | [`Stack`] middleware | `mode_ops::middleware` (E: hooks, modes, rules), `conductor_ops::middleware` (F: todo and verify reminders), `checkpoint::middleware` (F2: snapshots in `pre_mutation`), `task_ops::middleware` (N: background news at a step boundary), `f3_ops::middleware` (F3: `ultrathink` and `+500k`, the task-list mirror) |
//! | [`Stack`] observers | `mode_ops::observers` (E: SessionStart and Notification hooks), `checkpoint::observers` (F2: per-call and per-batch turn rows) |
//! | [`Stack`] services | `mode_ops::services` (E: the hook runner), `conductor_ops::services` (F: `ask_user`, subagents, worktrees), `task_ops::services` (N: the task registry), `notify_ops::services` (N), `workflow_ops::services` (W), `f3_ops::services` (F3), `web_ops::services` (T20: the `web_fetch` prompt digest) |
//!
//! The exec-side middleware (A's `FileState`, B's job notices, C's diagnostics) comes from the
//! tool host itself (`ToolHost::middleware`, seeded by `Agent::new`), built next to its state.
//! The stack is built once per session runner, so middleware may keep state across turns.

use std::sync::Arc;

use marlowe_agent::{Conversation, Services, ToolHost, ToolMiddleware, TurnObserver};

use crate::daemon::Core;

/// Wraps a session's tool host, before the memory and crew wrappers go on.
pub(crate) fn tools(core: &Arc<Core>, convo: &Conversation, host: Arc<dyn ToolHost>) -> Arc<dyn ToolHost> {
    crate::mcp_ops::compose(core, convo, host)
}

/// One session's middleware, observers and services.
#[derive(Clone, Default)]
pub(crate) struct Stack {
    pub middleware: Vec<Arc<dyn ToolMiddleware>>,
    pub observers: Vec<Arc<dyn TurnObserver>>,
    pub services: Services,
}

impl Stack {
    /// Built when a session's runner starts.
    pub fn build(core: &Arc<Core>, convo: &Conversation) -> Self {
        let middleware = [
            crate::mode_ops::middleware(core, convo),
            crate::conductor_ops::middleware(core, convo),
            crate::checkpoint::middleware(core, convo),
            crate::task_ops::middleware(core, convo),
            crate::f3_ops::middleware(core, convo),
        ]
        .concat();
        let observers = [crate::mode_ops::observers(core, convo), crate::checkpoint::observers(core, convo)].concat();
        let adders: [fn(&Arc<Core>, &Conversation, Services) -> Services; 7] = [
            crate::mode_ops::services,
            crate::conductor_ops::services,
            crate::task_ops::services,
            crate::notify_ops::services,
            crate::workflow_ops::services,
            crate::f3_ops::services,
            crate::web_ops::services,
        ];
        let services = adders.iter().fold(Services::new(), |s, add| add(core, convo, s));
        Self { middleware, observers, services }
    }
}
