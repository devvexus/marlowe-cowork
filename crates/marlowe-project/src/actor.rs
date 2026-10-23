//! The state actor: the one writer of a running project's state.
//!
//! Every change is a [`Mutation`] sent over a channel to one thread that owns the
//! [`ProjectState`]. It applies them in order, bumps `rev`, coalesces the patches into
//! [`ProjectEvent::Updated`] events about every 100 ms, and writes `project.json` atomically at
//! most every 250 ms and at every flush, archive and stop. Workers never touch the file. Gate
//! events go out immediately after the updates that precede them, so a client never sees a card
//! before the state that explains it.

use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender};
use marlowe_proto::{Event, ProjectGateOption, ProjectPatch};
use thiserror::Error;

use crate::ids::now_ms;
use crate::mutation::{Applied, Mutation, MutationError};
use crate::state::{GateKind, ProjectState, ProjectStatus};
use crate::store::{ProjectStore, Snapshot, StoreError};

/// What the actor tells its listeners.
#[derive(Debug, Clone, PartialEq)]
pub enum ProjectEvent {
    /// Patches that take the state from `base` to `rev` (coalesced).
    Updated {
        /// The project.
        project: String,
        /// Revision the patches start from.
        base: u64,
        /// Revision they end at.
        rev: u64,
        /// The patches, in order.
        patches: Vec<ProjectPatch>,
    },
    /// A gate opened and the run is parked.
    GateRaised {
        /// The project.
        project: String,
        /// The gate id.
        gate: String,
        /// Plan or checkin.
        kind: GateKind,
        /// One line for a notice.
        summary: String,
        /// The answers on offer.
        options: Vec<ProjectGateOption>,
    },
    /// A gate was answered.
    GateAnswered {
        /// The project.
        project: String,
        /// The gate id.
        gate: String,
        /// The chosen option.
        choice: String,
    },
    /// The project's status changed.
    StatusChanged {
        /// The project.
        project: String,
        /// The new status.
        status: ProjectStatus,
    },
}

impl ProjectEvent {
    /// The wire event for this. `StatusChanged` has no wire event of its own (clients read it
    /// from the patches), so it maps to `None`.
    pub fn into_wire(self) -> Option<Event> {
        Some(match self {
            ProjectEvent::Updated { project, base, rev, patches } => Event::ProjectUpdated { project, base, rev, patches },
            ProjectEvent::GateRaised { project, gate, kind, summary, options } => {
                Event::ProjectGateRaised { project, gate, kind: kind.as_str().to_string(), summary, options }
            }
            ProjectEvent::GateAnswered { project, gate, choice } => Event::ProjectGateAnswered { project, gate, choice },
            ProjectEvent::StatusChanged { .. } => return None,
        })
    }
}

/// Timing and clock of an actor.
#[derive(Debug, Clone, Copy)]
pub struct ActorConfig {
    /// Patches are held this long to coalesce (default 100 ms).
    pub coalesce: Duration,
    /// `project.json` is written at most this often (default 250 ms).
    pub save_every: Duration,
    /// The clock stamped on changes; tests pass a fixed one.
    pub clock: fn() -> u64,
}

impl Default for ActorConfig {
    fn default() -> Self {
        ActorConfig { coalesce: Duration::from_millis(100), save_every: Duration::from_millis(250), clock: now_ms }
    }
}

/// Why a call to the actor failed.
#[derive(Debug, Error)]
pub enum ActorError {
    /// The mutation was refused; nothing changed.
    #[error(transparent)]
    Mutation(#[from] MutationError),
    /// The store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The actor has stopped.
    #[error("the project's state actor has stopped")]
    Gone,
    /// A query run on the actor's thread panicked (the actor itself is fine).
    #[error("a query of the project state panicked")]
    ReadPanicked,
}

enum Msg {
    Mutate(Box<Mutation>, Option<Sender<Result<Applied, MutationError>>>),
    Snapshot(Sender<Box<ProjectState>>),
    Read(Box<dyn FnOnce(&ProjectState) + Send>),
    Archive(String, Sender<Result<Snapshot, StoreError>>),
    Flush(Sender<Option<String>>),
    Stop(Sender<Option<String>>),
}

/// A handle to a running actor. Cloning shares the same actor.
#[derive(Clone)]
pub struct ProjectHandle {
    tx: Sender<Msg>,
    id: String,
}

/// The actor thread, owned by whoever started it; dropping the handle clones does not stop it.
pub struct ProjectActor {
    handle: ProjectHandle,
    join: Option<JoinHandle<()>>,
}

fn summary_of(kind: GateKind, st: &ProjectState) -> String {
    match kind {
        GateKind::Plan => format!("A plan is ready for \"{}\": {} questions", st.title, st.plan.questions.len()),
        GateKind::Checkin => format!("A checkin is waiting on \"{}\"", st.title),
    }
}

struct Core {
    st: ProjectState,
    store: ProjectStore,
    events: Sender<ProjectEvent>,
    cfg: ActorConfig,
    pending: Vec<ProjectPatch>,
    pending_base: u64,
    pending_since: Option<Instant>,
    dirty: bool,
    last_save: Instant,
    save_error: Option<String>,
}

impl Core {
    fn emit(&self, e: ProjectEvent) {
        let _ = self.events.send(e);
    }

    fn flush_patches(&mut self) {
        if !self.pending.is_empty() {
            let patches = std::mem::take(&mut self.pending);
            self.emit(ProjectEvent::Updated { project: self.st.id.clone(), base: self.pending_base, rev: self.st.rev, patches });
        }
        self.pending_since = None;
    }

    fn save(&mut self) {
        match self.store.save(&self.st) {
            Ok(()) => {
                self.dirty = false;
                self.save_error = None;
            }
            Err(e) => self.save_error = Some(e.to_string()),
        }
        self.last_save = Instant::now();
    }

    fn flush_all(&mut self) {
        self.flush_patches();
        if self.dirty {
            self.save();
        }
    }

    fn mutate(&mut self, m: Mutation) -> Result<Applied, MutationError> {
        let gate_answer = match &m {
            Mutation::AnswerGate { id, choice, .. } => Some((id.clone(), choice.clone())),
            _ => None,
        };
        let gate_kind = match &m {
            Mutation::PushGate(g) => Some((g.id.clone(), g.kind, g.options.clone())),
            _ => None,
        };
        let status_before = self.st.status;
        let applied = self.st.apply(m, (self.cfg.clock)())?;
        if self.pending.is_empty() {
            self.pending_base = applied.base;
            self.pending_since = Some(Instant::now());
        }
        self.pending.extend(applied.patches.iter().cloned());
        self.dirty = true;
        let project = self.st.id.clone();
        if gate_answer.is_some() || gate_kind.is_some() || self.st.status != status_before {
            // Updates first, so the card is never ahead of the state behind it.
            self.flush_patches();
        }
        if self.st.status != status_before {
            self.emit(ProjectEvent::StatusChanged { project: project.clone(), status: self.st.status });
        }
        if let Some((gate, kind, options)) = gate_kind {
            self.emit(ProjectEvent::GateRaised { project: project.clone(), gate, kind, summary: summary_of(kind, &self.st), options });
            // A parked run must survive a restart: write the card now.
            self.save();
        }
        if let Some((gate, choice)) = gate_answer {
            self.emit(ProjectEvent::GateAnswered { project, gate, choice });
            self.save();
        }
        Ok(applied)
    }

    fn run(mut self, rx: Receiver<Msg>) {
        loop {
            let now = Instant::now();
            let mut wait = Duration::from_secs(3600);
            if let Some(since) = self.pending_since {
                wait = wait.min((since + self.cfg.coalesce).saturating_duration_since(now));
            }
            if self.dirty {
                wait = wait.min((self.last_save + self.cfg.save_every).saturating_duration_since(now));
            }
            match rx.recv_timeout(wait) {
                Ok(Msg::Mutate(m, reply)) => {
                    let r = self.mutate(*m);
                    if let Some(tx) = reply {
                        let _ = tx.send(r);
                    }
                }
                Ok(Msg::Snapshot(tx)) => {
                    let _ = tx.send(Box::new(self.st.clone()));
                }
                Ok(Msg::Read(f)) => f(&self.st),
                Ok(Msg::Archive(label, tx)) => {
                    self.flush_all();
                    let _ = tx.send(self.store.archive(&self.st, &label));
                }
                Ok(Msg::Flush(tx)) => {
                    self.flush_all();
                    let _ = tx.send(self.save_error.clone());
                }
                Ok(Msg::Stop(tx)) => {
                    self.flush_all();
                    let _ = tx.send(self.save_error.clone());
                    return;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    self.flush_all();
                    return;
                }
            }
            let now = Instant::now();
            if self.pending_since.is_some_and(|s| now.duration_since(s) >= self.cfg.coalesce) {
                self.flush_patches();
            }
            if self.dirty && now.duration_since(self.last_save) >= self.cfg.save_every {
                self.save();
            }
        }
    }
}

impl ProjectActor {
    /// Starts the actor for `state`, which must already be in the store ([`ProjectStore::create`]
    /// or [`ProjectStore::reopen`]). Events go to `events`; a dropped receiver is not an error.
    pub fn spawn(store: ProjectStore, state: ProjectState, events: Sender<ProjectEvent>, cfg: ActorConfig) -> std::io::Result<ProjectActor> {
        let (tx, rx) = unbounded();
        let id = state.id.clone();
        let core = Core {
            st: state,
            store,
            events,
            cfg,
            pending: Vec::new(),
            pending_base: 0,
            pending_since: None,
            dirty: false,
            last_save: Instant::now(),
            save_error: None,
        };
        let join = std::thread::Builder::new().name(format!("project-{id}")).spawn(move || core.run(rx))?;
        Ok(ProjectActor { handle: ProjectHandle { tx, id }, join: Some(join) })
    }

    /// A handle for senders.
    pub fn handle(&self) -> ProjectHandle {
        self.handle.clone()
    }

    /// Flushes, saves, stops the thread and waits for it. Returns the last save error, if any.
    pub fn stop(mut self) -> Option<String> {
        let (tx, rx) = unbounded();
        let err = if self.handle.tx.send(Msg::Stop(tx)).is_ok() { rx.recv().ok().flatten() } else { None };
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        err
    }
}

impl ProjectHandle {
    /// The project's id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Applies a mutation and waits for the result.
    pub fn mutate(&self, m: Mutation) -> Result<Applied, ActorError> {
        let (tx, rx) = unbounded();
        self.tx.send(Msg::Mutate(Box::new(m), Some(tx))).map_err(|_| ActorError::Gone)?;
        Ok(rx.recv().map_err(|_| ActorError::Gone)??)
    }

    /// Queues a mutation without waiting (live numbers). A refused mutation is dropped.
    pub fn send(&self, m: Mutation) -> Result<(), ActorError> {
        self.tx.send(Msg::Mutate(Box::new(m), None)).map_err(|_| ActorError::Gone)
    }

    /// A copy of the state as it stands after every mutation sent before this call.
    pub fn snapshot(&self) -> Result<ProjectState, ActorError> {
        let (tx, rx) = unbounded();
        self.tx.send(Msg::Snapshot(tx)).map_err(|_| ActorError::Gone)?;
        Ok(*rx.recv().map_err(|_| ActorError::Gone)?)
    }

    /// Runs `f` on the actor's thread against the state as it stands after every mutation sent
    /// before this call, and returns its result. Cheap queries only: the writer waits while `f`
    /// runs, and nothing is cloned.
    pub fn read<R: Send + 'static>(&self, f: impl FnOnce(&ProjectState) -> R + Send + 'static) -> Result<R, ActorError> {
        let (tx, rx) = unbounded();
        self.tx
            .send(Msg::Read(Box::new(move |st| {
                // A query that panics must not take the writer down with it.
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(st)));
                let _ = tx.send(r.map_err(|_| ()));
            })))
            .map_err(|_| ActorError::Gone)?;
        rx.recv().map_err(|_| ActorError::Gone)?.map_err(|()| ActorError::ReadPanicked)
    }

    /// Emits pending updates and writes `project.json`. Returns the save error, if the write failed.
    pub fn flush(&self) -> Result<Option<String>, ActorError> {
        let (tx, rx) = unbounded();
        self.tx.send(Msg::Flush(tx)).map_err(|_| ActorError::Gone)?;
        rx.recv().map_err(|_| ActorError::Gone)
    }

    /// Flushes, then writes an archive snapshot labelled `label` (a phase boundary, or `final`).
    pub fn archive(&self, label: &str) -> Result<Snapshot, ActorError> {
        let (tx, rx) = unbounded();
        self.tx.send(Msg::Archive(label.to_string(), tx)).map_err(|_| ActorError::Gone)?;
        Ok(rx.recv().map_err(|_| ActorError::Gone)??)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::mutation::{ItemUpdate, LiveUpdate};
    use crate::replica::Replica;
    use crate::state::tests::sample;
    use crate::state::{Gate, GateStatus, Item, ItemStatus};
    use crate::store::TempDir;

    fn clock() -> u64 {
        5_000
    }

    fn fixed() -> ActorConfig {
        ActorConfig { coalesce: Duration::from_secs(3600), save_every: Duration::from_secs(3600), clock }
    }

    fn start(cfg: ActorConfig) -> (TempDir, ProjectActor, Receiver<ProjectEvent>, ProjectState) {
        let t = TempDir::new("actor");
        let store = ProjectStore::open(&t.0.join("projects"), &t.0.join("cowork.db")).unwrap();
        let st = sample();
        store.create(&st).unwrap();
        let (tx, rx) = unbounded();
        let a = ProjectActor::spawn(store, st.clone(), tx, cfg).unwrap();
        (t, a, rx, st)
    }

    fn source(st: &ProjectState, n: u32) -> Item {
        let mut it = Item::new(crate::ids::item_id(&st.id, "source", &n.to_string()), "source", "analyze@1");
        it.parent = Some(st.items_of_kind("question")[0].id.clone());
        it
    }

    fn drain(rx: &Receiver<ProjectEvent>) -> Vec<ProjectEvent> {
        rx.try_iter().collect()
    }

    #[test]
    fn mutations_apply_in_order_and_patches_are_coalesced_until_a_flush() {
        let (_t, a, rx, st) = start(fixed());
        let h = a.handle();
        let first = source(&st, 1);
        let id = first.id.clone();
        h.mutate(Mutation::PutItem(Box::new(first))).unwrap();
        for cpu in [10.0, 20.0, 30.0] {
            h.send(Mutation::UpdateItem { id: id.clone(), update: ItemUpdate { live: Some(LiveUpdate { cpu_pct: Some(cpu), ..Default::default() }), ..Default::default() } }).unwrap();
        }
        let snap = h.snapshot().unwrap();
        assert_eq!(snap.rev, st.rev + 4);
        assert_eq!(snap.item(&id).unwrap().live.cpu_pct, Some(30.0));
        assert!(drain(&rx).is_empty(), "nothing is emitted before the coalesce window ends");
        h.flush().unwrap();
        let evs = drain(&rx);
        assert_eq!(evs.len(), 1, "four mutations, one update");
        let ProjectEvent::Updated { base, rev, patches, .. } = &evs[0] else { panic!("{evs:?}") };
        assert_eq!((*base, *rev), (st.rev, st.rev + 4));
        // The client's copy follows it exactly.
        let mut replica = Replica::from_state(st.to_value());
        replica.apply(*base, *rev, patches).unwrap();
        assert_eq!(replica.state, snap.to_value());
        assert_eq!(replica.rev, snap.rev);
        a.stop();
    }

    #[test]
    fn an_update_goes_out_on_its_own_when_the_window_ends() {
        let (_t, a, rx, _st) = start(ActorConfig { coalesce: Duration::from_millis(20), save_every: Duration::from_millis(30), clock });
        let h = a.handle();
        h.mutate(Mutation::SetMetrics(crate::state::Metrics { cpu_pct: 9.0, ..Default::default() })).unwrap();
        let ev = rx.recv_timeout(Duration::from_secs(5)).expect("a coalesced update arrives");
        assert!(matches!(ev, ProjectEvent::Updated { .. }));
        a.stop();
    }

    #[test]
    fn a_refused_mutation_changes_nothing_and_says_why() {
        let (_t, a, rx, st) = start(fixed());
        let h = a.handle();
        let err = h.mutate(Mutation::UpdateItem { id: "i_nope".into(), update: ItemUpdate::default() }).unwrap_err();
        assert!(matches!(err, ActorError::Mutation(MutationError::UnknownItem(_))));
        assert_eq!(h.snapshot().unwrap().rev, st.rev);
        h.flush().unwrap();
        assert!(drain(&rx).is_empty());
        a.stop();
    }

    #[test]
    fn a_gate_event_follows_the_updates_before_it_and_the_card_is_saved_at_once() {
        let (t, a, rx, st) = start(fixed());
        let h = a.handle();
        h.mutate(Mutation::SetTainted(true)).unwrap();
        h.mutate(Mutation::PushGate(Box::new(Gate {
            id: "g_1".into(),
            kind: GateKind::Plan,
            status: GateStatus::Open,
            payload: json!({"questions": 3}),
            options: GateKind::Plan.options(0),
            answer: None,
            asked_ms: 1,
            answered_ms: None,
        phase: String::new(),
        step: String::new(),
        })))
        .unwrap();
        let evs = drain(&rx);
        assert!(matches!(&evs[0], ProjectEvent::Updated { .. }), "{evs:?}");
        assert!(matches!(&evs[1], ProjectEvent::GateRaised { gate, kind: GateKind::Plan, summary, .. } if gate == "g_1" && summary.contains("3 questions")), "{evs:?}");
        // Without any flush the open gate is already on disk: a restart rebuilds the parked run.
        let store = ProjectStore::open(&t.0.join("projects"), &t.0.join("cowork.db")).unwrap();
        let on_disk = store.load(&st.id).unwrap();
        assert_eq!(on_disk.open_gate().map(|g| g.id.as_str()), Some("g_1"));
        h.mutate(Mutation::AnswerGate { id: "g_1".into(), choice: "approve".into(), text: None }).unwrap();
        let evs = drain(&rx);
        assert!(matches!(evs.last(), Some(ProjectEvent::GateAnswered { choice, .. }) if choice == "approve"), "{evs:?}");
        assert!(store.load(&st.id).unwrap().open_gate().is_none());
        a.stop();
    }

    #[test]
    fn a_status_change_is_announced_and_wire_events_map() {
        let (_t, a, rx, st) = start(fixed());
        a.handle().mutate(Mutation::SetStatus(ProjectStatus::Running)).unwrap();
        let evs = drain(&rx);
        assert!(matches!(evs.last(), Some(ProjectEvent::StatusChanged { status: ProjectStatus::Running, .. })), "{evs:?}");
        let wire: Vec<Event> = evs.into_iter().filter_map(ProjectEvent::into_wire).collect();
        assert!(matches!(&wire[0], Event::ProjectUpdated { project, .. } if project == &st.id));
        assert_eq!(wire.len(), 1);
        a.stop();
    }

    #[test]
    fn stop_saves_and_archive_snapshots_the_state_as_it_stands() {
        let (t, a, _rx, st) = start(fixed());
        let h = a.handle();
        let it = source(&st, 1);
        let id = it.id.clone();
        h.mutate(Mutation::PutItem(Box::new(it))).unwrap();
        let snap = h.archive("ph_dc").unwrap();
        assert_eq!((snap.no, snap.rev), (1, st.rev + 1));
        h.mutate(Mutation::UpdateItem { id: id.clone(), update: ItemUpdate { status: Some(ItemStatus::Running), ..Default::default() } }).unwrap();
        assert!(a.stop().is_none());
        let store = ProjectStore::open(&t.0.join("projects"), &t.0.join("cowork.db")).unwrap();
        assert_eq!(store.load(&st.id).unwrap().item(&id).unwrap().status, ItemStatus::Running, "stop wrote the last change");
        assert!(h.snapshot().is_err(), "the handle reports the actor gone");
        assert!(matches!(h.mutate(Mutation::SetTainted(true)), Err(ActorError::Gone)));
    }

    #[test]
    fn many_threads_send_and_the_revision_counts_every_change_once() {
        let (_t, a, _rx, st) = start(fixed());
        let h = a.handle();
        let mut joins = Vec::new();
        for t in 0..4u32 {
            let h = h.clone();
            let base = st.clone();
            joins.push(std::thread::spawn(move || {
                for n in 0..25u32 {
                    h.mutate(Mutation::PutItem(Box::new(source(&base, t * 100 + n)))).unwrap();
                }
            }));
        }
        for j in joins {
            j.join().unwrap();
        }
        let snap = h.snapshot().unwrap();
        assert_eq!(snap.rev, st.rev + 100);
        assert_eq!(snap.items_of_kind("source").len(), 100);
        a.stop();
    }

    #[test]
    fn a_read_runs_against_the_current_state_and_a_panicking_read_leaves_the_actor_alive() {
        let (_t, a, _rx, st) = start(fixed());
        let h = a.handle();
        h.mutate(Mutation::SetTainted(true)).unwrap();
        // The read sees every mutation sent before it, without cloning the state.
        assert_eq!(h.read(|s| (s.tainted, s.rev, s.items.len())).unwrap(), (true, st.rev + 1, st.items.len()));
        assert!(matches!(h.read(|_| -> u8 { panic!("a bug in a query") }), Err(ActorError::ReadPanicked)));
        assert_eq!(h.read(|s| s.rev).unwrap(), st.rev + 1, "the writer is still there");
        a.stop();
    }
}
