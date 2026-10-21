//! The actor thread: sole owner of the Profile, Journal, BeliefStore, VectorStore and models.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use marlowe_contract::{Clock, Fidelity, TrustClass};
use marlowe_journal::{Journal, Profile};
use marlowe_memory::consolidate::{self, Policy};
use marlowe_memory::cue::dense::vectors::VectorStore;
use marlowe_memory::{
    correct_belief, memory_id, write_belief, Author, BeliefKind, BeliefReceipt, BeliefScope,
    BeliefStore, BeliefWrite, FrozenGate, MaturationPolicy, MemoryEntry, OperatingPoint, Tags,
    Utterance,
};
use marlowe_proto::{MemoryHit, MemoryStatsReport};

use crate::capture::{classify, Capture};
use crate::episode;
use crate::error::MindError;
use crate::legacy::{ChunkResult, ImportOp, Speaker};
use crate::models::Models;
use crate::profile::{self, Profile as UserProfile, ProfileQuery};
use crate::store::{self, Boot};
use crate::tags::{belief_kind, belief_scope, is_episode_summary, short};
use crate::types::{
    wire_trust, ConsolidateReport, Episode, Kind, MemoryIn, MindConfig, Provenance, Remembered, Retraction,
    SnapshotInfo, TurnIn, TurnRole,
};
use crate::workspaces::{is_user_request, Workspaces};
use crate::live::MindEvent;
use crate::{paths, vecfile, InjectQuery, Injection, RecallHit, RecallQuery, LEGACY_CONVERSATION_PROVENANCE, LEGACY_PROVENANCE};

type Reply<T> = Sender<T>;

/// Wait before retrying background embedding after a failure.
const EMBED_RETRY: Duration = Duration::from_secs(30);

pub(crate) enum Cmd {
    Ingest(TurnIn, Reply<Result<(), MindError>>),
    /// The memory and the ids of memories it was derived from (its lineage).
    Remember(MemoryIn, Vec<String>, Reply<Result<Remembered, MindError>>),
    Inject(InjectQuery, Option<crate::live::LiveSink>, Reply<Injection>),
    /// Replaces the write-tail listener.
    SetSink(crate::live::MindSink, Reply<()>),
    Profile(ProfileQuery, Reply<UserProfile>),
    Recall(RecallQuery, Reply<Result<Vec<RecallHit>, MindError>>),
    Correct(String, String, Provenance, Reply<Result<Remembered, MindError>>),
    Forget(String, Provenance, Reply<Result<(), MindError>>),
    Get(String, Reply<Option<MemoryHit>>),
    Episodes(Range<i64>, Reply<Result<Vec<Episode>, MindError>>),
    /// Session, summary, at_ms, whether the session read untrusted content.
    WriteEpisode(String, String, i64, bool, Reply<Result<String, MindError>>),
    /// Session, whether it read untrusted content. Extractive: no model.
    WriteSessionEpisode(String, bool, Reply<Result<Option<String>, MindError>>),
    Stats(Reply<Result<MemoryStatsReport, MindError>>),
    Consolidate(Reply<Result<ConsolidateReport, MindError>>),
    Snapshot(Reply<Result<SnapshotInfo, MindError>>),
    /// A `/rewind` dropped turns: retract what the session recorded from them.
    Retract(Retraction, Reply<Result<usize, MindError>>),
    /// The journal's last seq.
    JournalSeq(Reply<u64>),
    ImportChunk(Vec<ImportOp>, Reply<Result<ChunkResult, MindError>>),
    /// Test hook: how the store was booted.
    #[cfg(test)]
    BootInfo(Reply<Boot>),
    Shutdown(Reply<Result<(), MindError>>),
}

/// Coalescing embed queue. Past `cap` pending ids it collapses into one "embed everything
/// missing" pass.
#[derive(Debug, Default)]
pub(crate) struct EmbedQueue {
    pub pending: BTreeSet<String>,
    pub rebuild: bool,
    pub cap: usize,
}

impl EmbedQueue {
    pub fn push(&mut self, id: String) {
        if self.rebuild {
            return;
        }
        self.pending.insert(id);
        if self.pending.len() > self.cap {
            self.pending.clear();
            self.rebuild = true;
        }
    }

    pub fn is_empty(&self) -> bool {
        !self.rebuild && self.pending.is_empty()
    }

    pub fn clear(&mut self) {
        self.pending.clear();
        self.rebuild = false;
    }
}

pub(crate) struct Actor {
    pub cfg: MindConfig,
    pub profile_dir: PathBuf,
    #[cfg_attr(not(test), allow(dead_code))]
    pub boot: Boot,
    // Field order matters on drop: the journal closes before the profile lock is released.
    pub journal: Journal,
    pub profile: Profile,
    pub beliefs: BeliefStore,
    pub vectors: VectorStore,
    pub models: Models,
    pub gate: FrozenGate,
    pub point: OperatingPoint,
    pub queue: EmbedQueue,
    /// Session -> workspace, for the soft project boost.
    pub workspaces: Workspaces,
    /// Vectors changed since the vector file was last written.
    pub vectors_dirty: bool,
    /// Background embedding is paused until then after a failure.
    pub embed_retry_at: Option<Instant>,
    pub snapshot_seq: u64,
    pub last_snapshot: Instant,
    /// The embed run in progress (for the write tail's events).
    pub embed_run: Option<EmbedRun>,
}

/// One pass of the embed queue from first chunk to empty.
pub(crate) struct EmbedRun {
    pub started: Instant,
    pub total: u32,
    pub done: u32,
    pub installed: u32,
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn clock() -> Clock {
    Clock::new(now_ms())
}

/// The core's author for a provenance. Trust follows from it and nothing else.
fn author(p: &Provenance) -> Author {
    match p {
        Provenance::User { .. } => Author::User,
        Provenance::Agent { tainted: false, .. } => Author::Agent { run_floor: TrustClass::UserAsserted },
        Provenance::Agent { tainted: true, .. } => Author::Agent { run_floor: TrustClass::UntrustedContent },
        Provenance::Harness { .. } => Author::Harness,
        Provenance::External { .. } => Author::External,
    }
}

/// The source note shown with a memory. `None` for the user: the core describes that itself.
fn source_note(p: &Provenance) -> Option<String> {
    match p {
        Provenance::User { .. } => None,
        Provenance::Agent { session, turn, tainted: false } => {
            Some(format!("conductor, session {} turn {turn}", short(session)))
        }
        Provenance::Agent { session, turn, tainted: true } => {
            Some(format!("conductor after reading untrusted content, session {} turn {turn}", short(session)))
        }
        Provenance::Harness { what } => Some(format!("harness: {what}")),
        Provenance::External { source } => Some(format!("external: {source}")),
    }
}

fn receipt(r: &BeliefReceipt) -> Remembered {
    Remembered { id: r.id.clone(), trust: wire_trust(r.effective_trust), matures_at_ms: r.silent_until }
}

/// The reason a decision's text carries as a trailing ` (reason: …)`, the form `remember` writes.
fn trailing_reason(text: &str) -> Option<String> {
    let (_, r) = text.strip_suffix(')')?.rsplit_once(" (reason: ")?;
    Some(r.trim()).filter(|r| !r.is_empty()).map(str::to_string)
}

/// Not tombstoned and not superseded.
pub(crate) fn is_live(e: &MemoryEntry) -> bool {
    e.fidelity > Fidelity::Tombstone && e.superseded_by.is_none()
}

impl Actor {
    /// Opens or creates the profile and boots the store. Runs on the actor thread.
    pub fn open(home: &std::path::Path, cfg: MindConfig) -> Result<Actor, MindError> {
        let profile_dir = paths::profile_dir(home);
        let fresh = match std::fs::read_dir(&profile_dir) {
            Ok(mut it) => it.next().is_none(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            Err(e) => return Err(e.into()),
        };
        // `init` only on an absent or empty dir; anything else must open or fail loudly.
        let profile = if fresh { Profile::init(&profile_dir)? } else { Profile::open(&profile_dir)? };
        let snapshot = store::read_snapshot(&profile_dir, profile.key());
        let journal = store::open_journal(&profile, snapshot.as_ref())?;
        let (beliefs, boot) = store::boot_with(snapshot, &journal, profile.manifest().derivation_version)?;
        let gate = FrozenGate::load().map_err(|e| MindError::Artifact(e.to_string()))?;
        let point = OperatingPoint::load().map_err(|e| MindError::Artifact(e.to_string()))?;
        let models_dir = cfg.models_dir.clone().unwrap_or_else(|| paths::resolve_models_dir(home));
        crate::models::use_home_gpu_runtime(home);
        let models = Models::new(models_dir, cfg.model_idle, cfg.gpu.clone());
        // Pre-release builds kept an embedding cache here; it held vectors of forgotten text.
        let _ = std::fs::remove_dir_all(home.join("cache").join("embeddings"));
        let snapshot_seq = match boot {
            Boot::Snapshot { watermark, tail: 0 } => watermark,
            _ => 0,
        };
        let mut vectors = VectorStore::default();
        if let Err(e) = vecfile::load(&profile_dir, profile.key(), &beliefs, &mut vectors) {
            eprintln!("marlowe-mind: re-embedding: the vector file was not usable: {e}");
        }
        let rebuild = !vectors.pending(&beliefs).is_empty();
        let queue = EmbedQueue { pending: BTreeSet::new(), rebuild, cap: cfg.embed_queue.max(1) };
        let workspaces = Workspaces::load(&profile_dir);
        Ok(Actor {
            cfg,
            profile_dir,
            boot,
            journal,
            profile,
            beliefs,
            vectors,
            models,
            gate,
            point,
            queue,
            workspaces,
            vectors_dirty: false,
            embed_retry_at: None,
            snapshot_seq,
            last_snapshot: Instant::now(),
            embed_run: None,
        })
    }

    pub fn run(mut self, rx: Receiver<Cmd>) {
        loop {
            match rx.recv_timeout(self.next_wake()) {
                Ok(Cmd::Shutdown(reply)) => {
                    let _ = reply.send(self.snapshot().map(|_| ()));
                    return;
                }
                Ok(cmd) => self.handle(cmd),
                Err(RecvTimeoutError::Timeout) => self.idle_work(),
                Err(RecvTimeoutError::Disconnected) => {
                    if let Err(e) = self.snapshot() {
                        eprintln!("marlowe-mind: final snapshot failed: {e}");
                    }
                    return;
                }
            }
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Ingest(t, r) => drop(r.send(self.ingest(t))),
            Cmd::Remember(m, parents, r) => drop(r.send(self.remember(m, &parents))),
            Cmd::Inject(q, live, r) => drop(r.send(self.inject(q, live.as_ref()))),
            Cmd::SetSink(s, r) => {
                self.cfg.on_event = Some(s);
                let _ = r.send(());
            }
            Cmd::Profile(q, r) => drop(r.send(profile::build(self.beliefs.entries(), &q, now_ms()))),
            Cmd::Recall(q, r) => drop(r.send(self.recall(q))),
            Cmd::Correct(id, text, src, r) => drop(r.send(self.correct(&id, &text, src))),
            Cmd::Forget(id, src, r) => drop(r.send(self.forget(&id, &src))),
            Cmd::Get(id, r) => drop(r.send(self.get(&id))),
            Cmd::Episodes(range, r) => drop(r.send(Ok(self.episodes(range)))),
            Cmd::WriteEpisode(s, text, at, tainted, r) => drop(r.send(self.write_episode(&s, &text, at, tainted))),
            Cmd::WriteSessionEpisode(s, tainted, r) => drop(r.send(self.write_session_episode(&s, tainted))),
            Cmd::Stats(r) => drop(r.send(Ok(self.stats()))),
            Cmd::Consolidate(r) => drop(r.send(self.consolidate())),
            Cmd::Snapshot(r) => drop(r.send(self.snapshot())),
            Cmd::Retract(p, r) => drop(r.send(self.retract(p))),
            Cmd::JournalSeq(r) => drop(r.send(self.journal.last_seq())),
            Cmd::ImportChunk(ops, r) => drop(r.send(self.import_chunk(ops))),
            #[cfg(test)]
            Cmd::BootInfo(r) => drop(r.send(self.boot)),
            Cmd::Shutdown(_) => unreachable!("handled in run"),
        }
    }

    /// Zero while the embed queue has work (unless paused), else the next model unload, snapshot
    /// or embed retry deadline.
    fn next_wake(&self) -> Duration {
        if !self.queue.is_empty() && self.embed_retry_at.is_none() {
            return Duration::ZERO;
        }
        let mut at = self.last_snapshot + self.cfg.snapshot_every;
        if let (false, Some(r)) = (self.queue.is_empty(), self.embed_retry_at) {
            at = at.min(r);
        }
        if let Some(u) = self.models.next_unload() {
            at = at.min(u);
        }
        at.saturating_duration_since(Instant::now()).clamp(Duration::from_millis(50), Duration::from_secs(60))
    }

    /// Background work between commands: one embed chunk, else unloads and periodic snapshots.
    fn idle_work(&mut self) {
        let now = Instant::now();
        if self.embed_retry_at.is_some_and(|t| now >= t) {
            self.embed_retry_at = None;
        }
        if !self.queue.is_empty() && self.embed_retry_at.is_none() {
            // A failed step re-queues what it took; retry later rather than spin.
            if let Err(e) = self.embed_step() {
                eprintln!("marlowe-mind: embedding paused for {}s: {e}", EMBED_RETRY.as_secs());
                self.embed_retry_at = Some(Instant::now() + EMBED_RETRY);
            }
            return;
        }
        self.models.unload_idle(now);
        if now.duration_since(self.last_snapshot) >= self.cfg.snapshot_every {
            if self.journal.last_seq() != self.snapshot_seq || self.vectors_dirty {
                if let Err(e) = self.snapshot() {
                    eprintln!("marlowe-mind: periodic snapshot failed: {e}");
                }
            }
            self.last_snapshot = now;
        }
    }

    /// Tells the write-tail listener, if any. Never blocks the actor.
    pub(crate) fn tell(&self, e: MindEvent) {
        if let Some(s) = &self.cfg.on_event {
            s.emit(e);
        }
    }

    // ---- writes ------------------------------------------------------------------------

    /// Writes through the core and queues the new belief for embedding.
    fn write(&mut self, w: &BeliefWrite<'_>) -> Result<BeliefReceipt, MindError> {
        match write_belief(&mut self.journal, &mut self.beliefs, clock(), w)? {
            Ok(r) => {
                self.queue.push(r.id.clone());
                self.tell(MindEvent::Ingested);
                Ok(r)
            }
            Err(rejected) => Err(MindError::Rejected(rejected.as_wire_string())),
        }
    }

    /// Capture policy: user statements are Notes (injection candidates), user questions and
    /// requests are Episodes (recall only), assistant prose is an Episode the core never
    /// injects. Every turn is Global; the session's workspace goes to the boost map.
    fn ingest(&mut self, t: TurnIn) -> Result<(), MindError> {
        let text = t.text.trim();
        if text.is_empty() {
            return Ok(());
        }
        self.workspaces.record(&t.session, t.workspace.as_deref());
        let user_kind = match classify(text) {
            Capture::Statement => BeliefKind::Note,
            Capture::Request => BeliefKind::Episode,
        };
        let (author, kind, utterance, turn_id) = match t.role {
            TurnRole::User => (Author::User, user_kind, Utterance::User, "user"),
            TurnRole::Assistant => (
                Author::Agent { run_floor: TrustClass::UserAsserted },
                BeliefKind::Episode,
                Utterance::Assistant,
                "assistant",
            ),
        };
        self.write(&BeliefWrite {
            session_id: &t.session,
            turn_id,
            run_id: None,
            text,
            author,
            derived_from: &[],
            occurred_at_ms: t.at_ms,
            tags: Tags {
                kind: Some(kind),
                scope: BeliefScope::Global,
                utterance: Some(utterance),
                source: None,
                reason: None,
            },
            maturation: MaturationPolicy::ByTrust,
        })?;
        Ok(())
    }

    /// `parents`: memories this one was derived from. Those still in the store become its
    /// lineage, so its trust is the worst over them and a correction can find it.
    fn remember(&mut self, m: MemoryIn, parents: &[String]) -> Result<Remembered, MindError> {
        let text = m.text.trim();
        let reason = match &m.kind {
            Kind::Decision { reason } if !reason.trim().is_empty() => Some(reason.trim().to_string()),
            _ => None,
        };
        // A decision carries its reason in its text, so retrieval sees it.
        let text = match &reason {
            Some(r) if !text.is_empty() => format!("{text} (reason: {r})"),
            _ => text.to_string(),
        };
        let author = author(&m.source);
        let scope = belief_scope(&m.scope);
        let kind = m.kind.tag();
        // An identical live deliberate memory of the same kind and scope at equal or higher trust
        // is the same memory. Duplicates would also collapse the rank-1/rank-2 rerank margin and
        // block injection. A captured turn or an import with the same text is not: matching it
        // would keep the deliberate record, and so the profile line, from being written.
        let trust = m.source.trust_class();
        if let Some(e) = self.beliefs.entries().find(|e| {
            is_live(e)
                && e.text == text
                && e.effective_trust >= trust
                && *e.scope() == scope
                && e.kind() == belief_kind(kind)
                && profile::is_deliberate(e)
        }) {
            return Ok(Remembered { id: e.id.clone(), trust: wire_trust(e.effective_trust), matures_at_ms: e.silent_until });
        }
        let session = m.source.session().unwrap_or("mind").to_string();
        let run_id = match &m.source {
            Provenance::Agent { turn, .. } => Some(format!("turn-{turn}")),
            _ => None,
        };
        // A deliberate write from a session that never read untrusted content cannot have been
        // steered by it (the taint latch guarantees that), so it is live at once; a tainted one
        // is UntrustedContent, keeps the maturation window and never enters the profile.
        let lineage: Vec<String> = parents.iter().filter(|p| self.beliefs.get(p).is_some()).cloned().collect();
        let r = self.write(&BeliefWrite {
            session_id: &session,
            turn_id: kind.label(),
            run_id: run_id.as_deref(),
            text: &text,
            author,
            derived_from: &lineage,
            occurred_at_ms: now_ms(),
            tags: Tags { kind: Some(belief_kind(kind)), scope, utterance: None, source: source_note(&m.source), reason },
            maturation: MaturationPolicy::Deliberate,
        })?;
        Ok(receipt(&r))
    }

    fn correct(&mut self, id: &str, new_text: &str, source: Provenance) -> Result<Remembered, MindError> {
        let target = self.beliefs.get(id).cloned().ok_or_else(|| MindError::NotFound(id.to_string()))?;
        if target.fidelity == Fidelity::Tombstone {
            return Err(MindError::NotFound(id.to_string()));
        }
        if let Some(by) = &target.superseded_by {
            return Err(MindError::Rejected(format!("{id} was already corrected by {by}; correct that one instead")));
        }
        let session = source.session().unwrap_or("mind").to_string();
        let note = match source_note(&source) {
            Some(n) => format!("{n}, corrects {id}"),
            None => format!("you, corrects {id}"),
        };
        // The reason comes from the corrected text, never from the target: the corrector vouches
        // only for words it wrote, so an untrusted target's reason cannot ride into the profile.
        let reason = if target.kind() == BeliefKind::Decision { trailing_reason(new_text.trim()) } else { None };
        let tags = Tags {
            kind: Some(target.kind()),
            scope: target.scope().clone(),
            utterance: None,
            source: Some(note),
            reason,
        };
        let write = BeliefWrite {
            session_id: &session,
            turn_id: "correction",
            run_id: None,
            text: new_text.trim(),
            author: author(&source),
            derived_from: &[],
            occurred_at_ms: now_ms(),
            tags,
            maturation: MaturationPolicy::Deliberate,
        };
        match correct_belief(&mut self.journal, &mut self.beliefs, clock(), &write, id)? {
            Ok(r) => {
                self.queue.push(r.id.clone());
                Ok(receipt(&r))
            }
            Err(rejected) => Err(MindError::Rejected(rejected.as_wire_string())),
        }
    }

    /// Forgets `id` on behalf of `source`, which must carry at least the memory's trust (the
    /// rule `correct` applies): a run that read untrusted content cannot erase what the user said.
    /// Then rewrites the snapshot and vector file so neither keeps the text.
    fn forget(&mut self, id: &str, source: &Provenance) -> Result<(), MindError> {
        let entry = self.beliefs.get(id).ok_or_else(|| MindError::NotFound(id.to_string()))?;
        if entry.fidelity == Fidelity::Tombstone {
            return Ok(());
        }
        let (have, need) = (source.trust_class(), entry.effective_trust);
        if have < need {
            return Err(MindError::Rejected(format!(
                "{id} was recorded as {need:?} and this request is only {have:?}, because of who \
                 asked or what this run has read. Forgetting it needs the user's approval."
            )));
        }
        self.forget_entry(id)?;
        if let Err(e) = self.snapshot() {
            eprintln!("marlowe-mind: snapshot after forget failed: {e}");
        }
        Ok(())
    }

    /// Journals `Retracted` and folds it: the session's beliefs from the rewound turns stay in the
    /// journal and leave every retrieval path. The snapshot is rewritten so a boot never sees
    /// them live between the rewind and the next periodic snapshot.
    fn retract(&mut self, r: Retraction) -> Result<usize, MindError> {
        let payload = marlowe_memory::store::RetractedPayload {
            session: r.session,
            turns: r.turns,
            after_seq: r.after_seq,
            from_ms: r.from_ms,
        };
        let n = marlowe_memory::store::append_retraction(&mut self.journal, &mut self.beliefs, clock(), &payload)?;
        if let Err(e) = self.snapshot() {
            eprintln!("marlowe-mind: snapshot after retract failed: {e}");
        }
        Ok(n)
    }

    /// Journals the tombstone and drops the text and vector from memory.
    fn forget_entry(&mut self, id: &str) -> Result<(), MindError> {
        if let Err(rejected) =
            marlowe_memory::forget_claim(&mut self.journal, &mut self.beliefs, clock(), "mind", "forget", id)?
        {
            return Err(MindError::Rejected(rejected.as_wire_string()));
        }
        self.queue.pending.remove(id);
        // The vector goes with the text.
        self.vectors.remove(id);
        self.vectors_dirty = true;
        Ok(())
    }

    /// A memory by id, live or superseded; `None` for an unknown id, a tombstone or a retraction.
    fn get(&self, id: &str) -> Option<MemoryHit> {
        let retracted = |e: &MemoryEntry| {
            e.superseded_by.as_deref().is_some_and(|b| b.starts_with(marlowe_memory::store::RETRACTED_BY_PREFIX))
        };
        self.beliefs
            .get(id)
            .filter(|e| e.fidelity > Fidelity::Tombstone && !retracted(e))
            .map(|e| crate::select::hit(e, 0.0))
    }

    fn episodes(&self, range: Range<i64>) -> Vec<Episode> {
        let mut out: Vec<Episode> = self
            .beliefs
            .entries()
            .filter(|e| is_live(e) && is_episode_summary(e) && range.contains(&e.occurred_at_ms))
            .map(|e| Episode {
                id: e.id.clone(),
                session: e.source_session_id.clone(),
                workspace: match e.scope() {
                    BeliefScope::Project(p) => Some(p.clone()),
                    BeliefScope::Global => self.workspaces.of(&e.source_session_id).map(str::to_string),
                },
                summary: e.text.clone(),
                at_ms: e.occurred_at_ms,
            })
            .collect();
        out.sort_by(|a, b| a.at_ms.cmp(&b.at_ms).then_with(|| a.id.cmp(&b.id)));
        out
    }

    /// The session's ingested turns, oldest first.
    fn session_turns(&self, session: &str) -> Vec<&MemoryEntry> {
        let mut turns: Vec<&MemoryEntry> = self
            .beliefs
            .entries()
            .filter(|e| e.source_session_id == session && e.fidelity > Fidelity::Tombstone && e.tags.utterance.is_some())
            .collect();
        turns.sort_by(|a, b| a.occurred_at_ms.cmp(&b.occurred_at_ms).then_with(|| a.id.cmp(&b.id)));
        turns
    }

    /// An episode is built from the session's own turns, so agent-inferred at best, and its
    /// lineage is every turn of the session: the core takes the worst trust over it. A session
    /// that read untrusted content writes an untrusted episode.
    fn write_episode(&mut self, session: &str, summary: &str, at_ms: i64, tainted: bool) -> Result<String, MindError> {
        self.write_episode_over(session, summary, at_ms, tainted, None)
    }

    /// Extracts and writes the session's episode (no model), superseding its earlier one.
    fn write_session_episode(&mut self, session: &str, tainted: bool) -> Result<Option<String>, MindError> {
        let (text, at_ms, earlier) = {
            let turns = self.session_turns(session);
            let Some(last) = turns.last() else { return Ok(None) };
            let at_ms = last.occurred_at_ms;
            let view: Vec<episode::Turn<'_>> = turns
                .iter()
                .map(|e| episode::Turn { user: e.tags.utterance == Some(Utterance::User), at_ms: e.occurred_at_ms, text: &e.text })
                .collect();
            let lexicon = self.beliefs.lexicon();
            let text = episode::compose(&view, self.workspaces.of(session), lexicon.len(), |t| lexicon.document_frequencies(t));
            let earlier = self
                .beliefs
                .entries()
                .filter(|e| is_live(e) && is_episode_summary(e) && e.source_session_id == session)
                .max_by_key(|e| e.occurred_at_ms)
                .map(|e| (e.id.clone(), e.text.clone()));
            (text, at_ms, earlier)
        };
        let Some(text) = text else { return Ok(None) };
        if let Some((id, old)) = &earlier {
            if *old == text {
                return Ok(Some(id.clone()));
            }
        }
        self.write_episode_over(session, &text, at_ms, tainted, earlier.map(|(id, _)| id)).map(Some)
    }

    /// Writes an episode; with `supersede`, as the correction of that earlier episode (a plain
    /// write when the correction would carry less trust, e.g. the session became tainted).
    fn write_episode_over(
        &mut self,
        session: &str,
        summary: &str,
        at_ms: i64,
        tainted: bool,
        supersede: Option<String>,
    ) -> Result<String, MindError> {
        let turns = self.session_turns(session);
        let scope = turns.last().map(|e| e.scope().clone()).unwrap_or_default();
        let lineage: Vec<String> = turns.iter().map(|e| e.id.clone()).collect();
        let source = Some(format!("episode summary, session {}", short(session)));
        let write = BeliefWrite {
            session_id: session,
            turn_id: "episode",
            run_id: None,
            text: summary.trim(),
            author: Author::Agent {
                run_floor: if tainted { TrustClass::UntrustedContent } else { TrustClass::UserAsserted },
            },
            derived_from: &lineage,
            occurred_at_ms: at_ms,
            tags: Tags { kind: Some(BeliefKind::Episode), scope, utterance: None, source, reason: None },
            maturation: MaturationPolicy::ByTrust,
        };
        if let Some(target) = supersede {
            if let Ok(r) = correct_belief(&mut self.journal, &mut self.beliefs, clock(), &write, &target)? {
                self.queue.push(r.id.clone());
                return Ok(r.id);
            }
        }
        Ok(self.write(&write)?.id)
    }

    fn stats(&self) -> MemoryStatsReport {
        let live: Vec<&MemoryEntry> = self.beliefs.entries().filter(|e| is_live(e)).collect();
        MemoryStatsReport {
            beliefs: live.len() as u64,
            episodes: live.iter().filter(|e| is_episode_summary(e)).count() as u64,
            vectors: self.vectors.len() as u64,
            models_loaded: self.models.loaded(),
            degraded: self.models.degraded().map(str::to_string),
            journal_seq: self.journal.last_seq(),
        }
    }

    /// Frozen-policy consolidation per session, never across trust classes and never touching
    /// assistant prose or user requests, so a merge cannot let a weaker belief supersede a
    /// stronger one, nor a question supersede a statement.
    fn consolidate(&mut self) -> Result<ConsolidateReport, MindError> {
        if let Some(reason) = self.models.degraded() {
            return Err(MindError::Degraded(reason.to_string()));
        }
        self.flush_embeddings(usize::MAX).map_err(MindError::Degraded)?;
        let policy = Policy::load().map_err(|e| MindError::Artifact(e.to_string()))?;
        let mut groups: BTreeMap<(String, TrustClass), BeliefStore> = BTreeMap::new();
        for e in self.beliefs.entries() {
            if is_live(e) && !e.is_assistant_prose() && !is_user_request(e) {
                groups.entry((e.source_session_id.clone(), e.effective_trust)).or_default().insert(e.clone());
            }
        }
        let mut report = ConsolidateReport::default();
        for ((session, _), view) in groups {
            if view.len() < 2 {
                continue;
            }
            let plan = consolidate::plan(&view, &session, &self.vectors, policy);
            if !plan.clusters.is_empty() {
                consolidate::apply(&mut self.journal, &mut self.beliefs, &plan, clock())?;
            }
            report.suppressed += plan.suppressed;
            report.sessions.push(plan);
        }
        Ok(report)
    }

    fn snapshot(&mut self) -> Result<SnapshotInfo, MindError> {
        let seq = self.journal.last_seq();
        let path =
            store::write_snapshot(&self.profile_dir, self.profile.key(), seq, self.journal.last_signature(), &self.beliefs)?;
        if self.vectors_dirty {
            vecfile::write(&self.profile_dir, self.profile.key(), &self.beliefs, &self.vectors)?;
            self.vectors_dirty = false;
        }
        self.snapshot_seq = seq;
        self.last_snapshot = Instant::now();
        Ok(SnapshotInfo { path, journal_seq: seq, beliefs: self.beliefs.len() as u64 })
    }

    fn import_chunk(&mut self, ops: Vec<ImportOp>) -> Result<ChunkResult, MindError> {
        let mut out = ChunkResult::default();
        let mut forgot = false;
        for op in ops {
            match op {
                ImportOp::Write(w) => {
                    let id = memory_id(crate::legacy::SESSION, &w.digest, 0);
                    if self.beliefs.get(&id).is_some() {
                        out.skipped_duplicates += 1;
                        continue;
                    }
                    // Lineage only through parents that made it here; the carried effective
                    // trust already holds the old journal's worst case.
                    let parents: Vec<String> =
                        w.derived_from.iter().filter(|p| self.beliefs.get(p).is_some()).cloned().collect();
                    let r = self.write(&BeliefWrite {
                        session_id: crate::legacy::SESSION,
                        turn_id: &w.digest,
                        run_id: Some("legacy-import"),
                        text: &w.text,
                        author: Author::Imported { trust_class: w.trust_class, effective_trust: w.effective_trust },
                        derived_from: &parents,
                        occurred_at_ms: w.occurred_at_ms,
                        tags: Tags {
                            kind: Some(w.kind),
                            scope: BeliefScope::Global,
                            utterance: None,
                            source: Some(LEGACY_PROVENANCE.to_string()),
                            reason: None,
                        },
                        maturation: MaturationPolicy::ByTrust,
                    })?;
                    debug_assert_eq!(r.id, id);
                    out.imported += 1;
                }
                ImportOp::Turn(t) => {
                    let id = memory_id(&t.session, &t.digest, 0);
                    if self.beliefs.get(&id).is_some() {
                        out.skipped_duplicates += 1;
                        continue;
                    }
                    let utterance = match t.speaker {
                        Speaker::User => Utterance::User,
                        Speaker::Assistant => Utterance::Assistant,
                    };
                    let r = self.write(&BeliefWrite {
                        session_id: &t.session,
                        turn_id: &t.digest,
                        run_id: Some("legacy-import"),
                        text: &t.text,
                        author: Author::Imported { trust_class: t.trust_class, effective_trust: t.trust_class },
                        derived_from: &[],
                        occurred_at_ms: t.occurred_at_ms,
                        tags: Tags {
                            kind: Some(t.kind),
                            scope: BeliefScope::Global,
                            utterance: Some(utterance),
                            source: Some(LEGACY_CONVERSATION_PROVENANCE.to_string()),
                            reason: None,
                        },
                        maturation: MaturationPolicy::ByTrust,
                    })?;
                    debug_assert_eq!(r.id, id);
                    out.imported += 1;
                    match t.speaker {
                        Speaker::User => {
                            out.user_turns += 1;
                            out.user_requests += u64::from(t.kind == BeliefKind::Episode);
                        }
                        Speaker::Assistant => out.assistant_turns += 1,
                    }
                }
                ImportOp::Supersede { loser, winner } => {
                    if self.beliefs.get(&loser).is_some_and(|e| e.superseded_by.is_none()) {
                        self.supersede("legacy-import", &loser, &winner)?;
                        out.superseded += 1;
                    }
                }
                ImportOp::Tombstone { id } => {
                    if self.beliefs.get(&id).is_some_and(|e| e.fidelity > Fidelity::Tombstone) {
                        self.forget_entry(&id)?;
                        forgot = true;
                    }
                }
            }
        }
        if forgot {
            self.snapshot()?;
        }
        Ok(out)
    }

    /// Journals `Superseded{id, by}` and folds it.
    fn supersede(&mut self, session: &str, id: &str, by: &str) -> Result<(), MindError> {
        use marlowe_journal::{Actor as JournalActor, AppendRequest, EventKind, TraceId};
        self.journal.append(
            clock(),
            AppendRequest {
                trace_id: TraceId::new_v5(&TraceId::NAMESPACE_OID, session.as_bytes()),
                session_id: Some(session.to_string()),
                run_id: None,
                actor: JournalActor::Harness,
                kind: EventKind::Superseded,
                payload: serde_json::to_value(marlowe_memory::store::SupersededPayload {
                    id: id.to_string(),
                    by: by.to_string(),
                })?,
            },
        )?;
        self.beliefs.supersede(id, by);
        Ok(())
    }
}
