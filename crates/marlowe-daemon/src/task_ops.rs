//! Background tasks, cron and wake-ups (track N): ListTasks, StopTask, TaskOutput, CronCreate.
//!
//! The `/tasks` panel's requests and the daemon side of the `task_*` tools. The registry itself,
//! its sources and the news drain are `tasks.rs`; schedules are `schedule_ops.rs`.
//! `stack.rs` calls [`middleware`] and [`services`] for every session and `boot.rs` calls
//! [`on_boot`], so N edits no shared file.

use std::sync::Arc;

use marlowe_agent::{Background, Conversation, Services, TaskRegistry, Tasks, ToolMiddleware};
use marlowe_proto::{Event, Request};

use crate::bg_ops::DaemonBackground;
use crate::daemon::Core;

pub(crate) fn dispatch(core: &Arc<Core>, client: u64, req: Request) {
    let reply = |event: Event| core.bus.reply(client, event);
    match req {
        Request::ListTasks { session } => {
            let list = crate::tasks::registry(core).list(session.as_ref());
            reply(Event::Tasks { list });
        }
        Request::StopTask { id } => {
            // The change arrives as `TaskUpdated`; a refusal is an error to this client.
            if let Err(message) = crate::tasks::registry(core).stop(&id) {
                reply(Event::Error { session: None, message });
            }
        }
        Request::TaskOutput { id, since, filter } => match crate::tasks::registry(core).output(&id, since, filter.as_deref()) {
            Ok(chunk) => reply(Event::TaskOutput { id, text: chunk.text, next_since: chunk.next_since, done: chunk.done }),
            Err(message) => reply(Event::Error { session: None, message }),
        },
        cron @ Request::CronCreate { .. } => crate::schedule_ops::dispatch(core, client, cron),
        other => crate::client::not_yet(core, client, &other, "N"),
    }
}

/// N's loop middleware for one session: the injection drain (`after_step` and `before_final`, one
/// batched message per step: monitor lines, cron fires, wake-ups, workflow results, messages).
pub(crate) fn middleware(core: &Arc<Core>, convo: &Conversation) -> Vec<Arc<dyn ToolMiddleware>> {
    let drain = crate::tasks::DrainMiddleware { registry: crate::tasks::handle(core), session: convo.session.clone() };
    vec![Arc::new(drain)]
}

/// N's services for one session: the task registry and the background operations.
pub(crate) fn services(core: &Arc<Core>, convo: &Conversation, services: Services) -> Services {
    if let Some(subs) = services.get::<marlowe_agent::subagent::Subagents>() {
        crate::tasks::note_subagents(core, &convo.session, subs.clone());
    }
    let ops = DaemonBackground {
        core: core.clone(),
        hooks: services.get::<Arc<marlowe_agent::hooks::HookRunner>>().cloned(),
        questions: services.get::<marlowe_agent::todo::Questions>().cloned(),
    };
    services.with(Tasks(crate::tasks::handle(core))).with(Background(Arc::new(ops)))
}

/// Installs the registry's sources and threads; re-arms nothing else here (durable schedules are
/// `schedule_ops::on_boot`, and a monitor dies with the session host that owns its process).
/// Must not block.
pub(crate) fn on_boot(core: &Arc<Core>) {
    crate::tasks::install(core);
}

/// The registry as a trait object, for tests through `Daemon::tasks`.
#[allow(dead_code)]
pub(crate) fn registry(core: &Core) -> Arc<dyn TaskRegistry> {
    crate::tasks::handle(core)
}
