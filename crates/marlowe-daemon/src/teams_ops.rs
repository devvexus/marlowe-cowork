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

use marlowe_agent::teams::{on_reminder, TeamsNote, OFF_NOTICE};
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
        match mode(&core, &session, ws) {
            Mode::On => Some(on_reminder(size(&core, ws))),
            Mode::OffPending => {
                write(&core, &session, Mode::Off);
                Some(OFF_NOTICE.to_string())
            }
            Mode::Off => None,
        }
    }));
    services.with(note)
}
