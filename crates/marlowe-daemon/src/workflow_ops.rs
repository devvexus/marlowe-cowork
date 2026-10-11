//! Workflows and findings (track W): ListWorkflows, WatchWorkflow, RunWorkflow,
//! SkipWorkflowAgent, ListFindings; the `workflow` and `report_findings` tools' daemon side (the
//! run, its journal, `WorkflowUpdated` and `Findings`).
//!
//! Seam stub: `client.rs` already routes these requests here; until track W lands every one is
//! answered with an `Error` naming the track. Replace the body of [`dispatch`]; keep its
//! signature. `stack.rs` calls [`services`] for every session and `boot.rs` calls [`on_boot`].

use std::sync::Arc;

use marlowe_agent::{Conversation, Services};
use marlowe_proto::Request;

use crate::daemon::Core;

pub(crate) fn dispatch(core: &Arc<Core>, client: u64, req: Request) {
    crate::client::not_yet(core, client, &req, "W");
}

/// W's services for one session (the workflow launcher the `workflow` tool calls).
pub(crate) fn services(_core: &Arc<Core>, _convo: &Conversation, services: Services) -> Services {
    services
}

/// Resumes workflow runs a killed daemon left running. Must not block.
pub(crate) fn on_boot(_core: &Arc<Core>) {}
