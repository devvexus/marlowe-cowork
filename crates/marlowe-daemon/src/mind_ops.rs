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
use marlowe_proto::{Event, Request};

use crate::daemon::Core;
use crate::lock;

/// Most hits a `MemorySearch` returns.
const SEARCH_MAX: usize = 200;

/// Session id a `MemoryProbe` injection is journalled under.
const PROBE_SESSION: &str = "probe";

/// `ImportProgress` is broadcast at most this often.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

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

/// Writes an evicted session's episode. Extractive, in the memory engine: the opening request,
/// when and where, the salient lines and keywords of the turns already ingested. No model is
/// called, so nothing loads. Failures are reported on the bus, never fatal.
pub(crate) fn write_episode(core: &Core, mind: &Mind, convo: &Conversation) {
    let session = convo.session.clone();
    // A session that read untrusted content writes an untrusted episode.
    if let Err(e) = mind.write_session_episode(session.as_str(), convo.exposure.tainted) {
        if !matches!(e, marlowe_mind::MindError::Closed) {
            core.bus.publish(Event::Error { session: Some(session), message: format!("episode not written: {e}") });
        }
    }
}
