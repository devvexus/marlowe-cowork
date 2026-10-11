//! The loop's write path to the journal.
//!
//! ARCHITECTURE §7: *"Loop → Journal: **Nothing directly.** Requests only, via the harness."*
//! and *"Harness → Journal: `JournalEvent` (typed, signed)"*. The loop is harness code, so it
//! may append with `Actor::Harness`; what it may never do is hand the model a path to this.
//! There is no `ModelStep` variant that names an event kind, and there must not be one.
//!
//! A port rather than a concrete `Journal` for one reason that is not testing convenience: the
//! loop must be constructible in a context that has no profile on disk (the first frame, before
//! a daemon exists), and the alternative to a port there is an `Option<Journal>` with a silent
//! no-op branch — a write path that sometimes does not write, which is the shape that makes an
//! audit trail unfalsifiable.

use marlowe_contract::Clock;
use marlowe_journal::{Actor, AppendRequest, EventKind, Journal, Seq};

use crate::run::{RunId, SessionId};

pub trait Recorder {
    fn append(
        &mut self,
        clock: Clock,
        kind: EventKind,
        run: RunId,
        session: SessionId,
        payload: serde_json::Value,
    ) -> Result<Seq, String>;
}

/// The real one.
pub struct JournalRecorder<'a> {
    journal: &'a mut Journal,
    trace_id: uuid::Uuid,
}

impl<'a> JournalRecorder<'a> {
    pub fn new(journal: &'a mut Journal, trace_id: uuid::Uuid) -> Self {
        Self { journal, trace_id }
    }
}

impl Recorder for JournalRecorder<'_> {
    fn append(
        &mut self,
        clock: Clock,
        kind: EventKind,
        run: RunId,
        session: SessionId,
        payload: serde_json::Value,
    ) -> Result<Seq, String> {
        self.journal
            .append(
                clock,
                AppendRequest {
                    trace_id: self.trace_id,
                    session_id: Some(session.to_string()),
                    run_id: Some(run.to_string()),
                    actor: Actor::Harness,
                    kind,
                    payload,
                },
            )
            .map(|e| e.seq)
            .map_err(|e| e.to_string())
    }
}

/// The real one, when the journal has **two** users inside one turn.
///
/// # Why this exists, rather than a second journal or a wider `Recorder`
///
/// M2 Session D wires memory into the daemon, and a memory write is a *signed* append —
/// `marlowe_memory::ingest` takes `&mut Journal` because invariant 2 says there is no unsigned
/// write path. The loop already holds the journal for the whole turn through [`JournalRecorder`],
/// so the moment `Ports.memory` is something other than `None` there are two mutable borrows of
/// one `Journal`. That is not a borrow-checker inconvenience: it is the honest shape of one
/// append-only log with two writers in it.
///
/// The two alternatives were both worse. A second journal would give memory its own log, and the
/// project's core abstraction is that there is exactly one. Routing memory writes through
/// [`Recorder`] would point `marlowe-memory` at `marlowe-loop` — the wrong direction — and would
/// force memory events through a signature that fixes `Actor::Harness` and demands a run and a
/// session that a consolidation write does not have.
///
/// **Nothing about the signed write path changes here.** `Journal::append` is the same call with
/// the same signature over the same bytes; what moved is who holds the handle. The borrow is taken
/// per append and never spans a call into anything that could borrow again — `engine.rs` computes a
/// `remember` outcome and *then* records the event, sequentially — so the `RefCell` cannot be
/// re-entered. If a future change makes it re-entrant it panics loudly at the second borrow, which
/// is the direction this project prefers over a write that silently does not happen.
/// `Arc<Mutex<_>>` rather than `Rc<RefCell<_>>`, and that is a correction rather than a preference:
/// the daemon is held in an `Arc<Mutex<Daemon>>` and moved between threads, so an `Rc` does not
/// compile. It is still **serial** — the accept loop serves one connection to completion — but
/// "single-threaded in behaviour" and "never crosses a thread" are different claims, and only the
/// second would have justified `Rc`.
pub struct SharedJournalRecorder {
    journal: std::sync::Arc<std::sync::Mutex<Journal>>,
    trace_id: uuid::Uuid,
}

impl SharedJournalRecorder {
    pub fn new(
        journal: std::sync::Arc<std::sync::Mutex<Journal>>,
        trace_id: uuid::Uuid,
    ) -> Self {
        Self { journal, trace_id }
    }
}

impl Recorder for SharedJournalRecorder {
    fn append(
        &mut self,
        clock: Clock,
        kind: EventKind,
        run: RunId,
        session: SessionId,
        payload: serde_json::Value,
    ) -> Result<Seq, String> {
        self.journal
            // A poisoned lock means a previous append panicked mid-write. That is not survivable
            // in silence — invariant 7 — and continuing would append after an event whose state
            // nobody knows.
            .lock()
            .expect("the journal lock was poisoned by a panicking append")
            .append(
                clock,
                AppendRequest {
                    trace_id: self.trace_id,
                    session_id: Some(session.to_string()),
                    run_id: Some(run.to_string()),
                    actor: Actor::Harness,
                    kind,
                    payload,
                },
            )
            .map(|e| e.seq)
            .map_err(|e| e.to_string())
    }
}

/// An in-memory recorder, for tests and for the pre-daemon path.
///
/// It records the same sequence the journal would. It is **not** a no-op: a recorder that
/// dropped events would let a test assert on an audit trail that does not exist.
#[derive(Debug, Default)]
pub struct MemoryRecorder {
    pub events: Vec<(EventKind, RunId, serde_json::Value)>,
}

impl MemoryRecorder {
    pub fn kinds(&self) -> Vec<EventKind> {
        self.events.iter().map(|(k, _, _)| *k).collect()
    }

    pub fn count(&self, kind: EventKind) -> usize {
        self.events.iter().filter(|(k, _, _)| *k == kind).count()
    }

    pub fn payloads(&self, kind: EventKind) -> Vec<&serde_json::Value> {
        self.events.iter().filter(|(k, _, _)| *k == kind).map(|(_, _, p)| p).collect()
    }
}

impl Recorder for MemoryRecorder {
    fn append(
        &mut self,
        _clock: Clock,
        kind: EventKind,
        run: RunId,
        _session: SessionId,
        payload: serde_json::Value,
    ) -> Result<Seq, String> {
        self.events.push((kind, run, payload));
        Ok(self.events.len() as Seq)
    }
}
