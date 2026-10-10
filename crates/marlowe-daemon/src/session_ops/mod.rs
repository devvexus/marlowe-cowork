//! Sessions: rewind, checkpoints, diff, cost, rename, fork, export (track F2): Rewind,
//! ListCheckpoints, SessionDiff, SessionCost, RenameSession, ForkSession, ExportSession.
//!
//! Each request runs on its own short-lived thread (git and the store can take a moment) and
//! answers the asking client; nothing here blocks the client reader.
//!
//! - **Turns** are numbered as the loop numbers them. `/rewind N` goes back to the moment turn `N`
//!   began: the conversation keeps turns before `N`, the files are put back as they were when
//!   `N` started (the earliest checkpoint at or after `N`), and what the session recorded in
//!   memory from turns `N` on is retracted. `RewindScope` picks which of those happen. `/fork`
//!   follows the same convention: `at_turn = N` keeps turns before `N`.
//! - A rewind runs on the session's runner thread while the session is idle (a running turn
//!   refuses it), so the conversation it edits is the one the loop will use next. A code rewind
//!   also refuses while another live session of the same workspace is mid-turn, and puts back
//!   only what this session's turns changed; edits made outside them stay (and are listed).
//! - `/cost` counts rewound turns and calls without a row too.
//! - Rewound conversation rows move to `rewound_turns`, rewound `pre` snapshots to
//!   `refs/marlowe/<session>/rewound/`, and the working tree just before a rewind to
//!   `refs/marlowe/<session>/undo/`: nothing a rewind removes is deleted until the 30-day prune.

mod cost;
mod export;

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::bounded;
use marlowe_agent::checkpoint::ShadowRepo;
use marlowe_agent::{Conversation, Extensions};
use marlowe_mind::Retraction;
use marlowe_model::{ModelRef, Part, Role};
use marlowe_proto::{CheckpointInfo, Event, Request, RewindScope, SessionId, SessionSummary};

use crate::daemon::Core;
use crate::session::{AdminJob, Enqueue};
use crate::store::RowOut;
use crate::now_ms;

/// Longest a rewind may take before the client is told it did not finish (a huge `git restore`).
const REWIND_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub(crate) fn dispatch(core: &Arc<Core>, client: u64, req: Request) {
    let core = core.clone();
    let session = serde_json::to_value(&req)
        .ok()
        .and_then(|v| v.get("session").and_then(|s| s.as_str()).map(|s| SessionId(s.to_string())));
    let c2 = core.clone();
    let s2 = session.clone();
    let spawned = std::thread::Builder::new().name("marlowe-session-op".into()).spawn(move || {
        let result = catch_unwind(AssertUnwindSafe(|| run(&core, client, req)))
            .unwrap_or_else(|_| Err("the request failed unexpectedly".to_string()));
        if let Err(message) = result {
            core.bus.reply(client, Event::Error { session: session.clone(), message });
        }
    });
    if spawned.is_err() {
        c2.bus.reply(client, Event::Error { session: s2, message: "could not start the request".into() });
    }
}

fn run(core: &Arc<Core>, client: u64, req: Request) -> Result<(), String> {
    match req {
        Request::Rewind { session, turn, scope } => rewind(core, client, session, turn, scope),
        Request::ListCheckpoints { session } => {
            let list = list_checkpoints(core, &session)?;
            core.bus.reply(client, Event::Checkpoints { session, list });
            Ok(())
        }
        Request::SessionDiff { session, turn } => {
            let diff = diff(core, &session, turn)?;
            core.bus.reply(client, Event::SessionDiff { session, turn, diff });
            Ok(())
        }
        Request::SessionCost { session } => {
            let rows = core.store.rows(&session)?;
            core.bus.reply(client, Event::SessionCost { report: report(core, &session, &rows), session });
            Ok(())
        }
        Request::RenameSession { session, title } => rename(core, client, session, &title),
        Request::ForkSession { session, at_turn } => fork(core, client, session, at_turn),
        Request::ExportSession { session, format } => {
            let summary = summary_of(core, &session)?;
            let rows = core.store.rows(&session)?;
            let text = export::export(&summary, &rows, &report(core, &session, &rows), format);
            core.bus.reply(client, Event::SessionExported { session, format, text });
            Ok(())
        }
        other => Err(format!("{other:?} is not a session request")),
    }
}

fn summary_of(core: &Arc<Core>, session: &SessionId) -> Result<SessionSummary, String> {
    core.store.get(session)?.ok_or_else(|| format!("unknown session {session}"))
}

/// The session's tokens and cost, priced from the model library. Rewound turns and calls that
/// left no row are in it: the money was spent.
fn report(core: &Arc<Core>, session: &SessionId, rows: &[RowOut]) -> marlowe_proto::CostReport {
    let hidden = core.store.hidden_spend(session).unwrap_or_default();
    cost::report(rows, &hidden, &|m: &ModelRef| match core.brains.model_info(m) {
        Ok(info) => cost::Price { pricing: info.pricing, local: info.local },
        Err(_) => cost::Price { pricing: None, local: m.provider.is_local() },
    })
}

// ---- rewind --------------------------------------------------------------------------------

fn rewind(core: &Arc<Core>, client: u64, session: SessionId, turn: u32, scope: RewindScope) -> Result<(), String> {
    let summary = summary_of(core, &session)?;
    if turn >= summary.turns {
        return Err(format!("turn {turn} has not begun; this session is at turn {}", summary.turns));
    }
    let (tx, rx) = bounded::<Result<(), String>>(1);
    // A runner that idled out between the lookup and the job is replaced once.
    for attempt in 0..2 {
        let handle = core.session(&session).map_err(|(_, m)| m)?;
        let (c, tx) = (core.clone(), tx.clone());
        let job: AdminJob = Box::new(move |convo| {
            let _ = tx.send(apply_rewind(&c, convo, turn, scope));
        });
        match handle.run_admin(job) {
            Ok(()) => break,
            Err(Enqueue::Closed) if attempt == 0 => continue,
            Err(e) => return Err(e.to_string()),
        }
    }
    drop(tx);
    rx.recv_timeout(REWIND_TIMEOUT).map_err(|_| "the rewind did not finish".to_string())??;
    core.bus.publish(Event::Rewound { session: session.clone(), turn, scope });
    if scope != RewindScope::Code {
        // The asking client re-reads the transcript it now has.
        core.open_session(client, &session).map_err(|(_, m)| m)?;
    }
    Ok(())
}

/// Runs on the session's runner thread, with its conversation, while no turn runs.
fn apply_rewind(core: &Arc<Core>, convo: &mut Conversation, turn: u32, scope: RewindScope) -> Result<(), String> {
    let session = convo.session.clone();
    let repo = repo_for(core, convo.workspace.as_deref())?;
    let said = |message: String| {
        core.bus.publish(Event::Error { session: Some(session.clone()), message });
    };

    if matches!(scope, RewindScope::Code | RewindScope::Both) {
        if let Some(repo) = &repo {
            if let Some(other) = busy_sibling(core, &session, convo.workspace.as_deref()) {
                return Err(format!("session {other} is running a turn in this workspace; rewind the files when it is done"));
            }
            // When the turn began, so pre-images saved before it (by a turn number that was
            // dropped and reused) do not count.
            let started = core.store.rows(&session)?.iter().find(|r| r.turn == turn).map(|r| r.at_ms);
            let r = repo.restore_to_turn_at(&session, turn, started).map_err(|e| format!("could not put the files back: {e}"))?;
            // Where the files were before, for the user to take back (there is no request for it
            // yet); said only when the rewind left something unexpected behind.
            let undo = r.undo_ref.as_ref().map(|u| format!("; the files as they were before are kept as {u} in {}", repo.git_dir().display()));
            if !r.verified {
                said(format!(
                    "rewind: the files do not match the checkpoint exactly (a file changed while it was restored){}",
                    undo.clone().unwrap_or_default()
                ));
            }
            if !r.kept.is_empty() {
                let shown: Vec<&str> = r.kept.iter().take(8).map(String::as_str).collect();
                let more = r.kept.len().saturating_sub(shown.len());
                said(format!(
                    "rewind: left {} file(s) as they are, changed outside this session's turns: {}{}{}",
                    r.kept.len(),
                    shown.join(", "),
                    if more > 0 { format!(" and {more} more") } else { String::new() },
                    undo.unwrap_or_default()
                ));
            }
            for path in &r.preimages.skipped {
                said(format!("rewind: {path} was too large to checkpoint and was not restored"));
            }
        }
    }

    if matches!(scope, RewindScope::Conversation | RewindScope::Both) {
        let was_at = convo.next_turn;
        let dropped = core.store.drop_from(&session, turn, now_ms())?;
        if dropped.removed > 0 {
            if let Some(mind) = &core.mind {
                let retraction = Retraction {
                    session: session.to_string(),
                    turns: (turn..was_at).collect(),
                    after_seq: dropped.mem_seq,
                    from_ms: match dropped.mem_seq {
                        Some(_) => None,
                        None => dropped.kept_until_ms.map(|t| t + 1).or(dropped.first_ms),
                    },
                };
                if let Err(e) = mind.retract(retraction) {
                    said(format!("rewind: memory of the dropped turns was not retracted: {e}"));
                }
            }
        }
        if let Some(repo) = &repo {
            // With the files left as they are, the older states stay reachable; with them put
            // back, those states are the present.
            repo.discard_from(&session, turn, scope == RewindScope::Conversation)
                .map_err(|e| format!("checkpoints not updated: {e}"))?;
        }
        let rows = core.store.rows(&session)?;
        convo.messages = rows.iter().map(RowOut::message).collect();
        convo.summary = None;
        convo.next_turn = turn;
        convo.ext = Extensions::new();
        if let Some(r) = crate::checkpoint::recorder_of(core, &session) {
            r.reset(convo);
        }
    }
    Ok(())
}

/// Another live session of the same workspace that is mid-turn, if any: its edits would be
/// reverted under it.
fn busy_sibling(core: &Arc<Core>, session: &SessionId, workspace: Option<&std::path::Path>) -> Option<SessionId> {
    let mine = same_dir_key(workspace?);
    core.store.list(1000).ok()?.into_iter().find_map(|s| {
        let theirs = s.workspace.as_deref().map(std::path::Path::new).map(same_dir_key)?;
        (s.id != *session && theirs == mine && core.is_live(&s.id) && core.session(&s.id).is_ok_and(|h| h.is_busy())).then_some(s.id)
    })
}

/// A directory spelled so that two names for it compare equal (case on Windows, links, `\?\`).
fn same_dir_key(p: &std::path::Path) -> String {
    let c = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let s = c.to_string_lossy().trim_start_matches(r"\\?\").replace('\\', "/");
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

/// The shadow repository for a workspace, when there is a workspace and it has checkpoints.
fn repo_for(core: &Arc<Core>, workspace: Option<&std::path::Path>) -> Result<Option<Arc<ShadowRepo>>, String> {
    match workspace {
        Some(ws) => crate::checkpoint::store(core).existing(ws).map_err(|e| e.to_string()),
        None => Ok(None),
    }
}

// ---- checkpoints and diff ------------------------------------------------------------------

fn session_repo(core: &Arc<Core>, session: &SessionId) -> Result<Option<Arc<ShadowRepo>>, String> {
    let summary = summary_of(core, session)?;
    repo_for(core, summary.workspace.as_deref().map(std::path::Path::new))
}

fn list_checkpoints(core: &Arc<Core>, session: &SessionId) -> Result<Vec<CheckpointInfo>, String> {
    let Some(repo) = session_repo(core, session)? else { return Ok(Vec::new()) };
    let rows = core.store.rows(session)?;
    let cps = repo.checkpoints(session).map_err(|e| e.to_string())?;
    let current = if cps.is_empty() { String::new() } else { repo.current_tree().map_err(|e| e.to_string())? };
    let mut list: Vec<CheckpointInfo> = Vec::new();
    for (i, c) in cps.iter().enumerate() {
        let end = cps.get(i + 1).map(|n| n.tree.as_str()).unwrap_or(&current);
        let files = repo.changed(&c.tree, end).map(|f| f.len() as u32).unwrap_or(0);
        list.push(CheckpointInfo { turn: c.turn, at_ms: c.at_ms, files, summary: request_of(&rows, c.turn) });
    }
    // Turns that only saved pre-images (ignored files, files outside the workspace).
    for turn in repo.preimage_turns(session) {
        if !list.iter().any(|c| c.turn == turn) {
            let at_ms = rows.iter().find(|r| r.turn == turn).map(|r| r.at_ms).unwrap_or(0);
            list.push(CheckpointInfo { turn, at_ms, files: 0, summary: request_of(&rows, turn) });
        }
    }
    list.sort_by_key(|c| c.turn);
    Ok(list)
}

/// The first line of what the user asked in `turn`, short.
fn request_of(rows: &[RowOut], turn: u32) -> String {
    rows.iter()
        .find(|r| r.turn == turn && r.role == Role::User)
        .and_then(|r| {
            r.content.iter().find_map(|p| match p {
                Part::Text { text } => text.lines().map(str::trim).find(|l| !l.is_empty()).map(str::to_string),
                _ => None,
            })
        })
        .map(|l| {
            let mut s: String = l.chars().take(70).collect();
            if l.chars().count() > 70 {
                s.push('…');
            }
            s
        })
        .unwrap_or_default()
}

/// What changed in `turn` (to the start of the next turn that changed files, or to now), or in
/// the whole session. Empty when nothing was checkpointed.
fn diff(core: &Arc<Core>, session: &SessionId, turn: Option<u32>) -> Result<String, String> {
    let Some(repo) = session_repo(core, session)? else { return Ok(String::new()) };
    let cps = repo.checkpoints(session).map_err(|e| e.to_string())?;
    let start = match turn {
        Some(t) => cps.iter().position(|c| c.turn == t),
        None => (!cps.is_empty()).then_some(0),
    };
    let Some(i) = start else { return Ok(String::new()) };
    let end = match (turn, cps.get(i + 1)) {
        (Some(_), Some(next)) => next.tree.clone(),
        _ => repo.current_tree().map_err(|e| e.to_string())?,
    };
    repo.diff(&cps[i].tree, &end).map_err(|e| e.to_string())
}

// ---- rename and fork -----------------------------------------------------------------------

fn rename(core: &Arc<Core>, client: u64, session: SessionId, title: &str) -> Result<(), String> {
    let title = title.trim();
    if title.is_empty() {
        return Err("a session needs a name".into());
    }
    let summary = summary_of(core, &session)?;
    core.store.set_title(&session, title)?;
    if core.is_live(&session) {
        if let Ok(h) = core.session(&session) {
            h.set_title(title);
        }
    }
    let opened = Event::SessionOpened { session, title: title.to_string(), workspace: summary.workspace };
    core.bus.publish(opened.clone());
    core.bus.reply(client, opened);
    Ok(())
}

fn fork(core: &Arc<Core>, client: u64, session: SessionId, at_turn: Option<u32>) -> Result<(), String> {
    let source = summary_of(core, &session)?;
    if let Some(t) = at_turn {
        if t > source.turns {
            return Err(format!("turn {t} has not begun; this session is at turn {}", source.turns));
        }
    }
    let now = now_ms();
    let title = if source.title.trim().is_empty() { "fork".to_string() } else { format!("{} (fork)", source.title.trim()) };
    let id = SessionId::new_random();
    let copy = SessionSummary { id: id.clone(), title, workspace: source.workspace.clone(), created_ms: now, updated_ms: now, turns: 0 };
    let copy = core.store.fork(&session, copy, at_turn)?;
    // The copy owns its own checkpoints: rewinding one session never touches the other's.
    if let Some(repo) = repo_for(core, source.workspace.as_deref().map(std::path::Path::new))? {
        if let Err(e) = repo.fork(&session, &id, copy.turns) {
            core.bus.reply(client, Event::Error { session: Some(id.clone()), message: format!("fork: checkpoints not copied: {e}") });
        }
    }
    let opened = Event::SessionOpened { session: id, title: copy.title, workspace: copy.workspace };
    core.bus.publish(opened.clone());
    core.bus.reply(client, opened);
    Ok(())
}
