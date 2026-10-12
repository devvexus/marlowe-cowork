//! Background tasks, cron and wake-ups (track N): ListTasks, StopTask, TaskOutput, CronCreate.
//!
//! The `/tasks` panel's requests and the daemon side of the `task_*` tools. The registry itself,
//! its sources and the news drain are `tasks.rs`; schedules are `schedule_ops.rs`.
//! `stack.rs` calls [`middleware`] and [`services`] for every session and `boot.rs` calls
//! [`on_boot`], so N edits no shared file.

use std::sync::Arc;

use marlowe_agent::{Background, Conversation, Services, TaskRegistry, Tasks, ToolMiddleware, TurnFlow};
use marlowe_model::{Part, Role};
use marlowe_proto::{Event, Request, TaskInfo, TaskKind, TaskStatus};

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
/// batched message per step: monitor lines, cron fires, wake-ups, workflow results, messages) and
/// the background-work reminder.
pub(crate) fn middleware(core: &Arc<Core>, convo: &Conversation) -> Vec<Arc<dyn ToolMiddleware>> {
    let registry = crate::tasks::handle(core);
    let drain = crate::tasks::DrainMiddleware { registry: registry.clone(), session: convo.session.clone() };
    vec![Arc::new(drain), Arc::new(BackgroundNotice { registry })]
}

/// How the reminder starts; the newest one in the history is what was last reported.
const NOTICE_PREFIX: &str = "<system-reminder>\nBackground work:";
/// Tasks listed in one reminder; the rest are counted.
const NOTICE_MAX: usize = 5;

/// Tells the conductor, at the start of a turn, what runs in the background anywhere (a crew run
/// or workflow another conversation started, a shell job, a monitor): it cannot stop what it does
/// not know exists. One
/// `<system-reminder>` in the turn's tail (never the system prompt, so the cached prefix stays),
/// sent when the active set or a status differs from the newest reminder in this conversation's own
/// history, the way the editor note is, so a rewind or a compaction sends it again.
struct BackgroundNotice {
    registry: Arc<dyn TaskRegistry>,
}

/// Work worth reporting: alive, and not a schedule (a cron always waits for its next fire).
fn is_active(t: &TaskInfo) -> bool {
    !t.status.is_terminal() && t.kind != TaskKind::Cron
}

/// `running`, `queued`, `paused`, `needs you`: what a person would say about it.
fn status_word(t: &TaskInfo) -> String {
    if t.status != TaskStatus::Waiting {
        return t.status.as_str().to_string();
    }
    match (t.kind, t.detail.as_deref()) {
        (TaskKind::Agent | TaskKind::Workflow, Some(d)) => d.split(':').next().unwrap_or("").trim().to_string(),
        _ => String::new(),
    }
}

/// The reminder for the active tasks, newest first, or `None` when nothing is active.
fn notice_for(tasks: &[TaskInfo], me: &marlowe_proto::SessionId) -> Option<String> {
    let active: Vec<&TaskInfo> = tasks.iter().filter(|t| is_active(t)).collect();
    if active.is_empty() {
        return None;
    }
    let mut out = String::from(NOTICE_PREFIX);
    for t in active.iter().take(NOTICE_MAX) {
        let status = Some(status_word(t)).filter(|s| !s.is_empty()).unwrap_or_else(|| "waiting".into());
        out.push_str(&format!("\n{} · {} · {} · {}", t.id, t.kind.as_str(), crate::tasks::clip_line(&t.title, 48), status));
        if t.session.as_ref().is_some_and(|s| s != me) {
            out.push_str(" · (another conversation)");
        }
    }
    if active.len() > NOTICE_MAX {
        out.push_str(&format!("\n+{} more", active.len() - NOTICE_MAX));
    }
    out.push_str("\ntask_stop <id> stops one (a workflow stops as a whole); task_list shows them.\n</system-reminder>");
    Some(out)
}

/// The newest background reminder already in the history, if any.
fn last_notice(convo: &Conversation) -> Option<&str> {
    convo.messages.iter().rev().filter(|m| m.role == Role::User).find_map(|m| {
        m.content.iter().rev().find_map(|p| match p {
            Part::Text { text } if text.starts_with(NOTICE_PREFIX) => Some(text.as_str()),
            _ => None,
        })
    })
}

impl ToolMiddleware for BackgroundNotice {
    fn name(&self) -> &str {
        "background-notice"
    }

    fn before_turn(&self, convo: &mut Conversation, _text: &mut String) -> TurnFlow {
        let Some(note) = notice_for(&self.registry.list(None), &convo.session) else { return TurnFlow::Continue };
        if last_notice(convo) != Some(note.as_str()) && !convo.pending_context.contains(&note) {
            convo.pending_context.push(note);
        }
        TurnFlow::Continue
    }
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

#[cfg(test)]
mod tests {
    use marlowe_agent::{MemoryTaskRegistry, NewTask};
    use marlowe_model::Message;
    use marlowe_proto::SessionId;

    use super::*;

    fn rig() -> (Arc<MemoryTaskRegistry>, BackgroundNotice, Conversation) {
        let reg = Arc::new(MemoryTaskRegistry::new());
        let mw = BackgroundNotice { registry: reg.clone() };
        (reg, mw, Conversation::new(SessionId("mine".into()), None))
    }

    /// One turn as the loop runs it: the middleware, then the user's message is appended.
    fn turn(mw: &BackgroundNotice, c: &mut Conversation, said: &str) -> Vec<String> {
        let mut text = said.to_string();
        mw.before_turn(c, &mut text);
        assert_eq!(text, said, "the user's words are untouched");
        let pending = c.pending_context.clone();
        c.push(Message::user(text));
        pending
    }

    fn run_of_another_conversation(reg: &MemoryTaskRegistry) -> String {
        reg.register(
            NewTask::new(TaskKind::Agent, "analyst: For every crate, list the public items nobody calls")
                .raw_id("b8ee7e91")
                .session(Some(SessionId("other".into()))),
        )
    }

    #[test]
    fn the_first_turn_hears_of_a_run_another_conversation_started() {
        let (reg, mw, mut c) = rig();
        let id = run_of_another_conversation(&reg);
        let first = turn(&mw, &mut c, "Cancel the workflow");
        assert_eq!(first.len(), 1);
        let note = &first[0];
        assert!(note.starts_with("<system-reminder>") && note.ends_with("</system-reminder>"), "{note}");
        assert!(note.contains(&id) && note.contains("agent") && note.contains("running") && note.contains("(another conversation)"), "{note}");
        assert!(note.contains("task_stop <id> stops one") && note.contains("task_list"), "{note}");
        assert!(note.contains("For every crate, list the public"), "the title is clipped, not dropped: {note}");
        assert!(!note.contains("nobody calls"), "{note}");
        assert!(c.messages[0].content.iter().any(|p| matches!(p, Part::Text { text } if text == note)), "in the message tail");
        assert!(matches!(&c.messages[0].content[0], Part::Text { text } if text == "Cancel the workflow"), "after the user's words");
    }

    #[test]
    fn an_unchanged_list_is_not_repeated_and_a_status_change_is() {
        let (reg, mw, mut c) = rig();
        let id = run_of_another_conversation(&reg);
        assert_eq!(turn(&mw, &mut c, "a").len(), 1);
        assert!(turn(&mw, &mut c, "b").is_empty(), "same set, same status");
        reg.set_status(&id, TaskStatus::Waiting, Some("paused".into()));
        let again = turn(&mw, &mut c, "c");
        assert_eq!(again.len(), 1);
        assert!(again[0].contains("paused"), "{again:?}");
        assert!(turn(&mw, &mut c, "d").is_empty());
        let job = reg.register(NewTask::new(TaskKind::Job, "cargo test").raw_id("j1").session(Some(c.session.clone())));
        let more = turn(&mw, &mut c, "e");
        assert!(more.len() == 1 && more[0].contains(&job), "{more:?}");
        assert!(more[0].contains(&id), "the whole active set is restated: {more:?}");
    }

    #[test]
    fn nothing_active_means_no_reminder_and_a_new_conversation_gets_it_again() {
        let (reg, mw, mut c) = rig();
        assert!(turn(&mw, &mut c, "hello").is_empty());
        // A schedule waits for its next fire: not work in progress.
        reg.register(NewTask::new(TaskKind::Cron, "check CI").session(Some(c.session.clone())));
        let done = reg.register(NewTask::new(TaskKind::Job, "old").session(Some(c.session.clone())));
        reg.set_status(&done, TaskStatus::Done, None);
        assert!(turn(&mw, &mut c, "still nothing").is_empty());
        run_of_another_conversation(&reg);
        assert_eq!(turn(&mw, &mut c, "now").len(), 1);
        let mut fresh = Conversation::new(SessionId("fresh".into()), None);
        assert_eq!(turn(&mw, &mut fresh, "first turn").len(), 1, "the first turn of a conversation always gets it");
    }

    #[test]
    fn a_long_list_is_clipped_to_five_and_counted() {
        let (reg, mw, mut c) = rig();
        for i in 0..8 {
            reg.register(NewTask::new(TaskKind::Job, format!("job {i}")).raw_id(format!("j{i}")).session(Some(c.session.clone())));
        }
        let note = turn(&mw, &mut c, "go").remove(0);
        assert_eq!(note.matches(" · job · ").count(), 5, "{note}");
        assert!(note.contains("+3 more"), "{note}");
    }
}
