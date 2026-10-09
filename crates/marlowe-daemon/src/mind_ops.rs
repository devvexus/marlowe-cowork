//! Memory requests from clients, the first-run legacy offer, and session episodes.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use marlowe_agent::Conversation;
use marlowe_mind::{
    default_legacy_profile, InjectQuery, LegacyStatus, MemoryChange, Mind, ProfileQuery, Provenance, RecallQuery,
    MAX_INJECT_TOKENS,
};
use marlowe_model::{CancelToken, ChatRequest, Message, Part, Role, SystemBlock};
use marlowe_proto::{Event, Request, Slot};

use crate::daemon::Core;
use crate::{lock, now_ms};

/// Most hits a `MemorySearch` returns.
const SEARCH_MAX: usize = 200;

/// Session id a `MemoryProbe` injection is journalled under.
const PROBE_SESSION: &str = "probe";

/// `ImportProgress` is broadcast at most this often.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

/// Transcript tail shown to the episode summariser, in characters.
const EPISODE_TRANSCRIPT_CHARS: usize = 24_000;

/// Longest tool result shown to the summariser, in characters.
const EPISODE_RESULT_CHARS: usize = 400;

/// Output cap for an episode summary.
const EPISODE_MAX_TOKENS: u32 = 700;

/// Instruction for the episode summariser (a harness call, not the persona).
const EPISODE_PROMPT: &str = "Summarise this work session as a durable episode for later recall: \
what the user wanted, what was decided and why, what was found or produced, and what was left \
open. Name files, commands, numbers and dates exactly. Plain prose, at most 200 words. Do not \
address the user.";

/// Daemon-side memory state.
#[derive(Default)]
pub(crate) struct MemoryState {
    /// Cached `legacy_status(None)`; cleared after an import.
    legacy: Mutex<Option<Option<LegacyStatus>>>,
    importing: AtomicBool,
}

/// Handles a memory request for `client` on a worker thread. Never panics the client thread.
pub(crate) fn dispatch(core: &Arc<Core>, client: u64, req: Request) {
    let Some(mind) = core.mind.clone() else {
        core.bus.reply(client, error("memory is not available in this daemon".into()));
        return;
    };
    let c = core.clone();
    let spawned = std::thread::Builder::new().name("marlowe-memory".into()).spawn(move || {
        if catch_unwind(AssertUnwindSafe(|| handle(&c, &mind, client, req))).is_err() {
            c.bus.reply(client, error("the memory request failed".into()));
        }
    });
    if spawned.is_err() {
        core.bus.reply(client, error("the daemon is out of threads".into()));
    }
}

fn error(message: String) -> Event {
    Event::Error { session: None, message }
}

fn handle(core: &Arc<Core>, mind: &Mind, client: u64, req: Request) {
    let reply = |ev: Event| core.bus.reply(client, ev);
    match req {
        Request::MemorySearch { query, scope, limit } => {
            let limit = (limit as usize).clamp(1, SEARCH_MAX);
            match mind.recall(RecallQuery { text: query, scope, limit }) {
                Ok(hits) => reply(Event::MemoryResults { hits }),
                Err(e) => reply(error(format!("memory search failed: {e}"))),
            }
        }
        // A client is the user: the Memory view or the CLI.
        Request::MemoryForget { id } => match mind.forget(&id, Provenance::User { session: None }) {
            Ok(()) => {
                core.bus.publish(Event::MemoryChanged { id, change: MemoryChange::Forgotten });
            }
            Err(e) => reply(error(format!("not forgotten: {e}"))),
        },
        // A client is the user: the Memory view or the CLI.
        Request::MemoryCorrect { id, text } => match mind.correct(&id, &text, Provenance::User { session: None }) {
            Ok(r) => {
                core.bus.publish(Event::MemoryChanged { id, change: MemoryChange::Corrected { by: r.id } });
            }
            Err(e) => reply(error(format!("not corrected: {e}"))),
        },
        Request::MemoryStats => match mind.stats() {
            Ok(s) => reply(Event::MemoryStats(s)),
            Err(e) => reply(error(format!("memory stats failed: {e}"))),
        },
        Request::LegacyStatus => match legacy_status(core, mind) {
            Ok(Some(s)) => reply(available(&s)),
            Ok(None) => reply(error("no old harness profile was found".into())),
            Err(e) => reply(error(format!("could not read the old profile: {e}"))),
        },
        Request::ImportLegacy { path } => import(core, mind, client, path),
        // Retrieval alone: the profile is not excluded, so `memory bench` measures the pipeline.
        Request::MemoryProbe { query, workspace } => {
            let i = mind.inject(InjectQuery {
                session: PROBE_SESSION.into(),
                workspace,
                text: query,
                budget_tokens: MAX_INJECT_TOKENS,
                exclude: Vec::new(),
            });
            reply(Event::MemoryProbed { recalled: i.recalled, abstained: i.abstained, latency_ms: i.latency_ms });
        }
        Request::MemoryProfile { workspace } => match mind.profile(ProfileQuery { workspace, ..ProfileQuery::default() }) {
            Ok(p) => {
                let hits = p.ids.iter().filter_map(|id| mind.get(id).ok().flatten()).collect();
                reply(Event::MemoryProfile { hits, tokens: p.tokens, omitted: p.omitted });
            }
            Err(e) => reply(error(format!("memory profile failed: {e}"))),
        },
        _ => reply(error("not a memory request".into())),
    }
}

fn available(s: &LegacyStatus) -> Event {
    Event::LegacyAvailable {
        path: s.path.display().to_string(),
        approx_entries: s.approx_entries,
        already_imported: s.already_imported,
    }
}

/// The default old profile's status, cached after the first look.
fn legacy_status(core: &Core, mind: &Mind) -> Result<Option<LegacyStatus>, marlowe_mind::MindError> {
    if let Some(cached) = lock(&core.memory.legacy).clone() {
        return Ok(cached);
    }
    let s = mind.legacy_status(None)?;
    *lock(&core.memory.legacy) = Some(s.clone());
    Ok(s)
}

fn import(core: &Arc<Core>, mind: &Mind, client: u64, path: Option<String>) {
    let dir = match path.map(PathBuf::from).or_else(default_legacy_profile) {
        Some(d) => d,
        None => return core.bus.reply(client, error("no old harness profile location is known".into())),
    };
    if core.memory.importing.swap(true, Ordering::SeqCst) {
        return core.bus.reply(client, error("an import is already running".into()));
    }
    let mut last = Instant::now().checked_sub(PROGRESS_EVERY).unwrap_or_else(Instant::now);
    let mut progress = |p: marlowe_mind::ImportProgress| {
        if last.elapsed() >= PROGRESS_EVERY {
            last = Instant::now();
            core.bus.publish(Event::ImportProgress { read: p.read, imported: p.imported });
        }
    };
    let outcome = mind.import_legacy(&dir, &mut progress);
    *lock(&core.memory.legacy) = None;
    core.memory.importing.store(false, Ordering::SeqCst);
    match outcome {
        Ok(report) => {
            core.bus.publish(Event::ImportProgress { read: report.read, imported: report.imported });
            core.bus.publish(Event::ImportDone { report });
        }
        Err(e) => core.bus.reply(client, error(format!("import failed: {e}"))),
    }
}

/// After `Welcome`: offers the old harness profile to `client` when one exists and nothing was
/// imported from it yet. Runs on its own thread; silent when there is nothing to offer.
pub(crate) fn offer_legacy(core: &Arc<Core>, client: u64) {
    let Some(mind) = core.mind.clone() else { return };
    if !core.cfg.legacy_offer {
        return;
    }
    let c = core.clone();
    let _ = std::thread::Builder::new().name("marlowe-legacy-offer".into()).spawn(move || {
        if let Ok(Ok(Some(s))) = catch_unwind(AssertUnwindSafe(|| legacy_status(&c, &mind))) {
            if !s.already_imported && s.approx_entries > 0 {
                c.bus.reply(client, available(&s));
            }
        }
    });
}

/// Summarises an evicted session into an Episode memory via the Reflection slot (falling back to
/// the Conductor). Failures are reported on the bus, never fatal.
pub(crate) fn write_episode(core: &Core, mind: &Mind, convo: &Conversation) {
    let session = convo.session.clone();
    let summary = match summarise(core, convo) {
        Ok(s) if !s.trim().is_empty() => s,
        Ok(_) => return,
        Err(e) => {
            core.bus.publish(Event::Error { session: Some(session), message: format!("episode not written: {e}") });
            return;
        }
    };
    // The transcript includes tool output, so a tainted session's summary is untrusted.
    if let Err(e) = mind.write_episode(session.as_str(), summary.trim(), now_ms(), convo.exposure.tainted) {
        if !matches!(e, marlowe_mind::MindError::Closed) {
            core.bus.publish(Event::Error { session: Some(session), message: format!("episode not written: {e}") });
        }
    }
}

fn summarise(core: &Core, convo: &Conversation) -> Result<String, String> {
    let parts = catch_unwind(AssertUnwindSafe(|| {
        core.brains.agent_parts(&Slot::Reflection).or_else(|_| core.brains.agent_parts(&Slot::Conductor))
    }))
    .map_err(|_| "the model library failed".to_string())?;
    let (provider, info, _effort) = parts.map_err(|e| e.to_string())?;
    let mut req = ChatRequest::new(info.id.clone(), vec![Message::user(transcript(convo))]);
    req.effort = marlowe_model::Effort::Off;
    req.max_output_tokens = Some(EPISODE_MAX_TOKENS);
    req.system = vec![SystemBlock { text: EPISODE_PROMPT.into(), cache: false }];
    let cancel = CancelToken::new();
    let done = catch_unwind(AssertUnwindSafe(|| provider.stream(&req, &cancel, &mut |_| {})))
        .map_err(|_| "the summariser failed".to_string())?
        .map_err(|e| e.to_string())?;
    Ok(done.text)
}

/// The conversation as plain labelled text, keeping the most recent part when it is long.
pub(crate) fn transcript(convo: &Conversation) -> String {
    let mut out = String::new();
    if let Some(s) = &convo.summary {
        out.push_str("Earlier (condensed): ");
        out.push_str(s.trim());
        out.push_str("\n\n");
    }
    for m in &convo.messages {
        let who = match m.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::Tool => "Tool",
            Role::System => continue,
        };
        for p in &m.content {
            match p {
                Part::Text { text } if !text.trim().is_empty() => out.push_str(&format!("{who}: {}\n", text.trim())),
                Part::ToolCall(c) => out.push_str(&format!("{who} called {} {}\n", c.name, c.args)),
                Part::ToolResult(r) => {
                    let body: String = r.content.chars().take(EPISODE_RESULT_CHARS).collect();
                    out.push_str(&format!("Result: {}\n", body.trim()));
                }
                _ => {}
            }
        }
    }
    let n = out.chars().count();
    if n > EPISODE_TRANSCRIPT_CHARS {
        let tail: String = out.chars().skip(n - EPISODE_TRANSCRIPT_CHARS).collect();
        return format!("[earlier turns omitted]\n{tail}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use marlowe_proto::SessionId;

    #[test]
    fn the_transcript_labels_turns_and_keeps_the_tail() {
        let mut c = Conversation::new(SessionId("s".into()), None);
        c.push(Message::user("first"));
        c.push(Message::assistant("reply"));
        assert_eq!(transcript(&c), "User: first\nAssistant: reply\n");
        c.push(Message::user("y".repeat(EPISODE_TRANSCRIPT_CHARS)));
        let t = transcript(&c);
        assert!(t.starts_with("[earlier turns omitted]\n") && !t.contains("first"));
    }
}
