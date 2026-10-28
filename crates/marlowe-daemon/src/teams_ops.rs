//! Teams mode: SetTeams / GetTeams and the turn reminder (a per-session standing opt-in to team
//! tasks, remembered across restarts).
//!
//! A session's state is one `daemon_kv` row, `teams.<session>`: `on`, `off`, or `off_pending`
//! (switched off, and the next turn has yet to be told). A session with no row takes the
//! `[teams] default` setting and keeps it from then on, so changing the setting later does not
//! flip a session already seen. The reminders ride on the user turn's tail
//! ([`marlowe_agent::teams`]), never on the system prompt.

use std::path::Path;
use std::sync::Arc;

use marlowe_agent::teams::{is_research_request, looks_substantial, on_reminder_naming, on_reminder_projects, TeamsNote, OFF_NOTICE, RESEARCH_REMINDER, SUGGEST_REMINDER};
use marlowe_agent::{Conversation, Services};
use marlowe_proto::{Event, Request, SessionId};
use marlowe_settings::Settings;

use crate::daemon::Core;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Off,
    OffPending,
    On,
}

fn key(session: &SessionId) -> String {
    format!("teams.{session}")
}

fn parse(v: &str) -> Option<Mode> {
    match v {
        "on" => Some(Mode::On),
        "off" => Some(Mode::Off),
        "off_pending" => Some(Mode::OffPending),
        _ => None,
    }
}

fn write(core: &Core, session: &SessionId, mode: Mode) {
    let v = match mode {
        Mode::On => "on",
        Mode::Off => "off",
        Mode::OffPending => "off_pending",
    };
    let _ = core.store.set_kv(&key(session), v);
}

/// The session's mode, taking (and keeping) the default setting when it has none yet.
fn mode(core: &Core, session: &SessionId, workspace: Option<&Path>) -> Mode {
    if let Some(m) = core.store.kv(&key(session)).ok().flatten().as_deref().and_then(parse) {
        return m;
    }
    let m = if Settings::load(&core.home, workspace, None).teams_default() { Mode::On } else { Mode::Off };
    write(core, session, m);
    m
}

fn size(core: &Core, workspace: Option<&Path>) -> u32 {
    Settings::load(&core.home, workspace, None).team_size()
}

fn changed(core: &Core, session: &SessionId, workspace: Option<&Path>, on: bool) -> Event {
    Event::TeamsChanged { session: session.clone(), on, size: size(core, workspace) }
}

pub(crate) fn dispatch(core: &Arc<Core>, client: u64, req: Request) {
    let fail = |session: Option<SessionId>, message: String| core.bus.reply(client, Event::Error { session, message });
    let (session, set) = match req {
        Request::SetTeams { session, on } => (session, Some(on)),
        Request::GetTeams { session } => (session, None),
        other => return crate::client::not_yet(core, client, &other, "teams"),
    };
    let summary = match core.store.get(&session) {
        Ok(Some(s)) => s,
        Ok(None) => return fail(Some(session.clone()), format!("unknown session {session}")),
        Err(e) => return fail(Some(session), e),
    };
    let ws = summary.workspace.as_deref().map(Path::new);
    let was = mode(core, &session, ws);
    match set {
        None => core.bus.reply(client, changed(core, &session, ws, was == Mode::On)),
        Some(on) => {
            let next = match (on, was) {
                (true, _) => Mode::On,
                (false, Mode::On) => Mode::OffPending,
                (false, other) => other,
            };
            if next != was {
                write(core, &session, next);
            }
            core.bus.publish(changed(core, &session, ws, on));
        }
    }
}

/// The saved team task the user's message names, described for the reminder.
fn named_task(core: &Core, workspace: Option<&Path>, text: &str) -> Option<String> {
    let roots = marlowe_workflow::service::roots(&core.home, workspace, marlowe_agent::commands::plugin::bundle_roots(Some(&core.home), workspace));
    let catalog = roots.catalog();
    marlowe_workflow::saved::named_in(&catalog, text).map(marlowe_workflow::saved::describe)
}

/// The saved team task the message names, described for the reminder, and whether a project
/// kind replaces it.
fn named_task_superseded(core: &Core, workspace: Option<&Path>, text: &str) -> Option<(String, bool)> {
    static KINDS: std::sync::OnceLock<marlowe_project::Registry> = std::sync::OnceLock::new();
    let roots = marlowe_workflow::service::roots(&core.home, workspace, marlowe_agent::commands::plugin::bundle_roots(Some(&core.home), workspace));
    let catalog = roots.catalog();
    let found = marlowe_workflow::saved::named_in(&catalog, text)?;
    let replaced = KINDS.get_or_init(marlowe_project::Registry::builtin).superseded_by(&found.name).is_some();
    Some((marlowe_workflow::saved::describe(found), replaced))
}

/// What a turn is told when the daemon runs projects: Teams on points substantive requests (and
/// every research request) at a project; Teams off starts a research project for a research
/// request (its plan card is the ask) and only offers one for other substantial work.
fn project_reminder(core: &Core, workspace: Option<&Path>, mode: Mode, text: &str) -> Option<String> {
    match mode {
        Mode::On => {
            let (named, superseded) = match named_task_superseded(core, workspace, text) {
                Some((d, s)) => (Some(d), s),
                None => (None, false),
            };
            Some(on_reminder_projects(size(core, workspace), named.as_deref(), superseded))
        }
        _ if is_research_request(text) => Some(RESEARCH_REMINDER.to_string()),
        _ if looks_substantial(text) => Some(SUGGEST_REMINDER.to_string()),
        _ => None,
    }
}

/// A session's turn reminder: the on-reminder while Teams is on, the off notice once after it is
/// switched off. A crew result relayed into the conversation is not a user turn and gets none.
pub(crate) fn services(core: &Arc<Core>, convo: &Conversation, services: Services) -> Services {
    let weak = Arc::downgrade(core);
    let session = convo.session.clone();
    let workspace = convo.workspace.clone();
    let note = TeamsNote(Arc::new(move |text: &str| {
        if text.starts_with(crate::session::RELAY_PREFIX) {
            return None;
        }
        let core = weak.upgrade()?;
        let ws = workspace.as_deref();
        let projects = crate::project_ops::live(&core);
        // What the user chose on a project's card since their last message comes first.
        let running = match (crate::project_ops::take_answers(&core, &session.0), crate::project_ops::running_reminder(&core, &session.0)) {
            (Some(a), Some(r)) => Some(format!("{a}{r}")),
            (a, r) => a.or(r),
        };
        let base = match mode(&core, &session, ws) {
            Mode::On if projects => project_reminder(&core, ws, Mode::On, text),
            Mode::On => Some(on_reminder_naming(size(&core, ws), named_task(&core, ws, text).as_deref())),
            Mode::OffPending => {
                write(&core, &session, Mode::Off);
                // The off notice goes first; a research request in the same message still starts a project.
                match projects.then(|| project_reminder(&core, ws, Mode::Off, text)).flatten() {
                    Some(extra) => Some(format!("{OFF_NOTICE}{extra}")),
                    None => Some(OFF_NOTICE.to_string()),
                }
            }
            Mode::Off if projects => project_reminder(&core, ws, Mode::Off, text),
            Mode::Off => None,
        };
        match (base, running) {
            (Some(b), Some(r)) => Some(format!("{b}{r}")),
            (b, r) => b.or(r),
        }
    }));
    services.with(note)
}
