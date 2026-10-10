//! Background jobs and the `!` shell (track B): ListJobs, KillJob, UserShell, and the job events.
//!
//! The jobs themselves live in the tool host (`marlowe-exec/src/jobs/`); this daemon depends on no
//! executor crate, so it talks to them through the process-wide registry `marlowe_os::job::bg_hub`.
//! Nothing here blocks the client reader: a `!` command is started and answered by events.
//!
//! - `ListJobs` answers `Jobs`; `KillJob` kills the tree (the exit is broadcast as `JobExited`).
//! - `UserShell` runs the command as the user (no approval) in the session's workspace; its
//!   output is broadcast as `JobOutput`, and when it ends it becomes context for the session's
//!   next turn without waking the model.
//! - [`start`] subscribes to the registry and broadcasts `JobStarted`, `JobOutput` and
//!   `JobExited` to every client. The daemon calls it at boot; the first job request starts it
//!   too, so a daemon that forgot still reports jobs from then on.
//! - The same thread wakes idle sessions: a job exit or a `job_watch` match that arrives while a
//!   session has no turn running starts a relay turn (not the user's words, so memory does not
//!   ingest it at user trust) carrying the notices. A running turn is not touched: the job
//!   middleware hands the notices over at its next step. A session the daemon does not know (a
//!   crew run's host) keeps its notices.
//! - `Request::DetachCall` (Ctrl+B) is `marlowe_os::job::bg_hub().detach(session, Some(call_id))`.
//! - A session that runs jobs must not be evicted as idle (its tool host would take the jobs
//!   with it): [`keeps_session`] says so, for `Core::evict_if_idle`.

use std::collections::HashMap;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use marlowe_os::job::{bg_hub, BgEvent, BgInfo};
use marlowe_proto::coding::JobInfo;
use marlowe_proto::{Event, Request, SessionId};

use crate::daemon::Core;

/// Marks that this daemon's forwarder is running (kept in `Core::ext`).
struct Forwarder;

pub(crate) fn dispatch(core: &Arc<Core>, client: u64, req: Request) {
    start(core);
    match req {
        Request::ListJobs { session } => {
            let list = bg_hub().list(session.as_ref().map(|s| s.0.as_str())).iter().map(info).collect();
            core.bus.reply(client, Event::Jobs { list });
        }
        Request::KillJob { id } => {
            let known = bg_hub().list(None).into_iter().find(|j| j.id == id);
            match known {
                None => core.bus.reply(client, Event::Error { session: None, message: format!("no job `{id}`") }),
                Some(j) if j.exit_code.is_some() => core.bus.reply(
                    client,
                    Event::Error { session: Some(SessionId(j.session)), message: format!("job `{id}` has already exited") },
                ),
                Some(j) => {
                    bg_hub().kill(&id);
                    // A foreground call's job is not announced; tell the client it is gone anyway.
                    if j.foreground {
                        core.bus.publish(Event::JobExited { session: Some(SessionId(j.session)), id, code: -1 });
                    }
                }
            }
        }
        Request::UserShell { session, command } => {
            if command.trim().is_empty() {
                core.bus.reply(client, Event::Error { session: Some(session), message: "`!` needs a command".into() });
                return;
            }
            if let Err(message) = bg_hub().user_shell(&session.0, command.trim()) {
                core.bus.reply(client, Event::Error { session: Some(session), message });
            }
        }
        other => crate::client::not_yet(core, client, &other, "B"),
    }
}

fn info(j: &BgInfo) -> JobInfo {
    JobInfo {
        id: j.id.clone(),
        session: Some(SessionId(j.session.clone())),
        command: j.command.clone(),
        started_ms: j.started_ms,
        exit_code: j.exit_code,
    }
}

/// Starts broadcasting the registry's job events (once per daemon).
pub(crate) fn start(core: &Arc<Core>) {
    core.ext.get_or_init(|| {
        let events = bg_hub().subscribe();
        let weak: Weak<Core> = Arc::downgrade(core);
        let spawned = std::thread::Builder::new().name("marlowe-job-events".into()).spawn(move || {
            let mut woken: HashMap<SessionId, Instant> = HashMap::new();
            loop {
                let event = match events.recv_timeout(Duration::from_millis(500)) {
                    Ok(event) => Some(event),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                };
                let Some(core) = weak.upgrade() else { break };
                if core.is_shutting_down() {
                    break;
                }
                if let Some(event) = event {
                    core.bus.publish(wire(event));
                }
                wake_idle(&core, &mut woken);
            }
        });
        if let Err(e) = spawned {
            eprintln!("marlowe: the job event forwarder did not start: {e}");
        }
        Forwarder
    });
}

fn wire(event: BgEvent) -> Event {
    match event {
        BgEvent::Started(j) => Event::JobStarted { job: info(&j) },
        BgEvent::Output { session, id, text } => Event::JobOutput { session: Some(SessionId(session)), id, text },
        BgEvent::Exited { session, id, code } => Event::JobExited { session: Some(SessionId(session)), id, code },
    }
}

/// Least time between two wake-ups of one session.
const WAKE_GAP: Duration = Duration::from_secs(1);

/// Starts a relay turn for every idle session with a notice that wakes it.
fn wake_idle(core: &Arc<Core>, woken: &mut HashMap<SessionId, Instant>) {
    woken.retain(|_, at| at.elapsed() < Duration::from_secs(60));
    for id in bg_hub().waking_sessions() {
        let id = SessionId(id);
        if woken.get(&id).is_some_and(|at| at.elapsed() < WAKE_GAP) {
            continue;
        }
        // Not a session of this daemon (or not loadable): its notices stay where they are.
        let Ok(handle) = core.session(&id) else { continue };
        if handle.is_busy() {
            continue;
        }
        let notices = bg_hub().take_notices(&id.0);
        if notices.is_empty() {
            continue;
        }
        let text = notices.iter().map(|n| n.text.as_str()).collect::<Vec<_>>().join("\n\n");
        woken.insert(id.clone(), Instant::now());
        // A session evicted since the lookup: load it again and retry once.
        let delivered = handle.relay(text.clone(), false).is_ok()
            || core.session(&id).is_ok_and(|h| h.relay(text, false).is_ok());
        if !delivered {
            eprintln!("marlowe: a job notice for session {id} could not be delivered");
        }
    }
}

/// Whether `session` still has jobs running or notices waiting, so that the daemon keeps it (and
/// its tool host, which owns the jobs) past the idle timeout.
#[allow(dead_code)]
pub(crate) fn keeps_session(session: &SessionId) -> bool {
    bg_hub().running(&session.0) > 0 || bg_hub().waking_sessions().iter().any(|s| s == &session.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_events_map_onto_wire_events() {
        let j = BgInfo {
            id: "j1".into(),
            session: "s".into(),
            command: "make".into(),
            started_ms: 7,
            exit_code: Some(1),
            foreground: false,
            log: None,
        };
        assert!(matches!(wire(BgEvent::Started(j)), Event::JobStarted { job } if job.id == "j1" && job.exit_code == Some(1)));
        let out = wire(BgEvent::Output { session: "s".into(), id: "j1".into(), text: "hi".into() });
        assert!(matches!(out, Event::JobOutput { session: Some(s), text, .. } if s.0 == "s" && text == "hi"));
        let ex = wire(BgEvent::Exited { session: "s".into(), id: "j1".into(), code: 3 });
        assert!(matches!(ex, Event::JobExited { code: 3, .. }));
    }
}
