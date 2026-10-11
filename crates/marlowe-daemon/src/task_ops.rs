//! Background tasks, cron and wake-ups (track N): ListTasks, StopTask, TaskOutput, CronCreate.
//!
//! Seam stub: `client.rs` already routes these requests here; until track N lands every one is
//! answered with an `Error` naming the track. The registry itself is live (`tasks.rs`): the
//! session service is installed and changes are broadcast as `TaskUpdated`. Replace the body of
//! [`dispatch`]; keep its signature. `stack.rs` calls [`middleware`] and [`services`] for every
//! session and `boot.rs` calls [`on_boot`], so N edits no shared file.

use std::sync::Arc;

use marlowe_agent::{Conversation, Services, Tasks, ToolMiddleware};
use marlowe_proto::Request;

use crate::daemon::Core;

pub(crate) fn dispatch(core: &Arc<Core>, client: u64, req: Request) {
    crate::client::not_yet(core, client, &req, "N");
}

/// N's loop middleware for one session: the injection drain (`after_step`, at most one batched
/// injection per step; job exits, monitor lines, cron fires, workflow results, messages).
pub(crate) fn middleware(_core: &Arc<Core>, _convo: &Conversation) -> Vec<Arc<dyn ToolMiddleware>> {
    Vec::new()
}

/// N's services for one session: the task registry.
pub(crate) fn services(core: &Arc<Core>, _convo: &Conversation, services: Services) -> Services {
    services.with(Tasks(crate::tasks::handle(core)))
}

/// Starts the task broadcast; N re-arms durable crons and monitors here too. Must not block.
pub(crate) fn on_boot(core: &Arc<Core>) {
    crate::tasks::start_broadcast(core);
}
