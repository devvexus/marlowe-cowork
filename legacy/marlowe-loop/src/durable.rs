//! Durable runs — CONTRACTS.md §5's `checkpoint` and `resume`, made real. M3 Session A.
//!
//! # What a checkpoint must carry, and why the obvious answer is wrong
//!
//! Before this file the loop recorded `EventKind::Checkpointed` with `{"step": n}` and the live
//! journal held 895 of them. A step number is an **honest record that a step completed**. It is
//! not a resumable state, and the gap is not "a few more fields" — three of the things missing
//! from it are security properties, and each one fails in the same direction if it is dropped.
//!
//! | Field | What its absence would do on resume |
//! |---|---|
//! | `trust_floor` | **The latch un-latches.** See below — this is the one that matters. |
//! | `spent` | The budget resets, so a run costs its declared ceiling *per restart*. |
//! | `profile` | A quarantined reader could come back holding tools. |
//! | `step` | `MAX_STEPS` never fires; a looping run loops forever across restarts. |
//! | `contract_retries` | Audit finding E8's bound resets, so the unsatisfiable-contract loop returns. |
//!
//! ## The trust floor is the reason this is an ADR and not a commit
//!
//! ADR-023's floor is *"monotonic and latched per run"*, and the latch exists because the floor
//! used to be **derived** from the current window: trimming the untrusted block out of the view
//! raised the floor again and the run silently regained privileges it was supposed to have lost.
//!
//! A resume that rebuilt the run through `Run::root` would reopen that hole by a different
//! route. `root` starts at `TrustClass::UserAsserted`. So a run that read a hostile page, latched
//! to `UntrustedContent`, checkpointed, and came back after a daemon restart would compose targets
//! again — **the restart would have become the trim**, with no error, no event, and a green test
//! suite, because every existing ADR-023 test runs inside one process.
//!
//! `Run::restored` is the only constructor that sets the floor from outside `run.rs`, and
//! `tests/durable_resume.rs` asserts the floor survives a round trip through the journal with a
//! control that fails when it does not.
//!
//! # Inline, not by reference, and that is a deliberate cost
//!
//! The state travels **inside the signed payload**. A checkpoint that referenced mutable state
//! elsewhere would be a durability claim resting on something the signature does not cover — the
//! log would say a run is resumable and the thing it resumes from could have changed underneath.
//!
//! The cost is real and is stated rather than hidden: a checkpoint is written every iteration, so
//! a run writes its whole window `steps` times. The window is bounded — compaction triggers at
//! `COMPACTION_TRIGGER` of the effective context — so one checkpoint is bounded too, and
//! `tests/durable_resume.rs::a_checkpoint_is_bounded_by_the_window_not_by_the_run` measures the
//! real number rather than asserting the argument. Compressing or de-duplicating them is future
//! work with a measurement attached; guessing at it now would be a third mechanism in a file that
//! needs one.
//!
//! # Provenance is RECONSTRUCTED, not stored, and it fails closed either way
//!
//! `Provenance` lives behind brief §13's boundary (`crates/marlowe-loop/src/provenance.rs` is a
//! guarded path), and it is a cache of *"the user literally typed this string"*. Its inputs are in
//! the checkpointed state: every `SourceKind::History` block at `TrustClass::UserAsserted` is a
//! user message or a steer, which is exactly what `attribute_user_message` was called with.
//! [`Checkpoint::restore`] replays those calls.
//!
//! **Losing it entirely would have been safe** — `taint_for` falls back to the run's floor for any
//! unattributed value, which is `TaintSet::of`'s fail-closed default — so this reconstruction buys
//! usability, not safety, and the test that pins it says so by asserting the *negative* case too:
//! a value only the model composed is still not attributed after a resume.

use marlowe_contract::TrustClass;
use marlowe_journal::{EventKind, Journal, OperatorCapability, Seq};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::budget::Budget;
use crate::context::{SessionState, SourceKind};
use crate::profile::CapabilityProfile;
use crate::provenance::Provenance;
use crate::run::{OrphanPolicy, OutputContract, Run, RunId, RunStatus, SessionId};

/// The checkpoint schema version.
///
/// **A checkpoint of an unknown version is refused by name, never interpreted.** `serde` would
/// happily fill a missing field with a default, and every default this struct could take is a
/// security property reset to its permissive value — the floor to `UserAsserted`, `spent` to zero.
/// The version is the load-time error that stops a schema change becoming a silent privilege
/// restoration. `CLAUDE.md`: *prefer a load-time error to a sensible default.*
pub const CHECKPOINT_VERSION: u16 = 1;

/// Everything needed to continue a run at the step after its last completed one.
///
/// `deny_unknown_fields` for the same reason the version exists: a payload carrying a field this
/// build does not know is a payload written by a build that knew something this one does not.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub version: u16,
    pub run: RunId,
    pub parent: Option<RunId>,
    pub session: SessionId,
    pub trace_id: Uuid,
    /// The last **completed** step. A resume begins at `step + 1`.
    pub step: u32,
    pub status: RunStatus,
    /// Routed through `CapabilityProfile`'s validating `Deserialize`, so a checkpoint claiming a
    /// quarantined reader with tools is refused at decode rather than restored.
    pub profile: CapabilityProfile,
    pub budget: Budget,
    pub spent: Budget,
    /// **ADR-023's latch.** See the module header.
    pub trust_floor: TrustClass,
    pub orphan_policy: OrphanPolicy,
    pub output_contract: OutputContract,
    /// Audit finding E8's per-run bound.
    pub contract_retries: u32,
    pub state: SessionState,
}

/// What [`Checkpoint::restore`] hands back: the three values `Engine::run` mutates, plus the two
/// loop counters whose *whole purpose* is to bound a run that will not stop.
pub struct Restored {
    pub run: Run,
    pub state: SessionState,
    pub provenance: Provenance,
    /// Steps already taken. The resumed loop continues from here rather than from zero.
    pub steps: u32,
    pub contract_retries: u32,
}

impl Checkpoint {
    /// Take a checkpoint of a run mid-flight.
    pub fn capture(run: &Run, state: &SessionState, step: u32, contract_retries: u32) -> Self {
        Self {
            version: CHECKPOINT_VERSION,
            run: run.id,
            parent: run.parent,
            session: run.session,
            trace_id: run.trace_id,
            step,
            status: run.status.clone(),
            profile: run.profile.clone(),
            budget: run.budget,
            spent: run.spent,
            trust_floor: run.trust_floor(),
            orphan_policy: run.orphan_policy,
            output_contract: run.output_contract.clone(),
            contract_retries,
            state: state.clone(),
        }
    }

    /// Rebuild the run, its window, and its provenance.
    pub fn restore(self) -> Restored {
        let mut provenance = Provenance::new();
        // See the module header. Only `History` at `UserAsserted` — the class the harness stamps
        // on a user message and on a steer, and on nothing a model produced.
        for block in self.state.volatile.iter().chain(self.state.context_blocks.iter()) {
            if block.source == SourceKind::History && block.trust == TrustClass::UserAsserted {
                provenance.attribute_user_message(&block.text);
            }
        }
        let run = Run::restored(
            self.run,
            self.parent,
            self.session,
            self.trace_id,
            self.status,
            self.profile,
            self.budget,
            self.spent,
            self.orphan_policy,
            self.output_contract,
            None,
            self.trust_floor,
        );
        Restored {
            run,
            state: self.state,
            provenance,
            steps: self.step,
            contract_retries: self.contract_retries,
        }
    }

    /// Whether this checkpoint describes a run that has stopped for good.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            RunStatus::Completed | RunStatus::Cancelled | RunStatus::Failed { .. }
        )
    }
}

/// Where checkpoints live.
///
/// A port rather than a concrete `Journal`, for [`crate::record::Recorder`]'s reason: the loop
/// must be constructible where no profile exists on disk, and the alternative is an `Option` with
/// a silent no-op branch — a durability claim that sometimes does not write.
pub trait CheckpointStore {
    /// Append. Returns the journal sequence the contract's `checkpoint` promises.
    ///
    /// **The clock is a parameter, exactly as `Journal::append`'s is.** §4.5 forbids reading a
    /// system clock on any path reachable from the three interfaces, and this one is: a store
    /// holding a clock taken at construction would stamp every checkpoint with the daemon's boot
    /// time, which is a wrong timestamp in a signed log rather than a missing one.
    fn write(&mut self, cp: &Checkpoint, clock: marlowe_contract::Clock) -> Result<Seq, String>;
    /// The most recent checkpoint for this run, if any.
    fn latest(&self, run: RunId) -> Option<Checkpoint>;

    /// The most recent checkpoint of **every** run the store knows.
    ///
    /// # Why a resume feature needs this, and why leaving it out is a half-shipped one
    ///
    /// The daemon's run table is in memory. After a restart it is empty, so a run that survived —
    /// the whole claim — is **unfindable**: `/runs` lists nothing and `--resume` needs an id
    /// nobody can produce. The work would have survived and the user could not reach it, which is
    /// indistinguishable from it not having survived.
    ///
    /// So the table is seeded from here at startup. Ordered by run id, because the caller renders
    /// it and a listing whose order changes between two identical restarts is not reproducible.
    fn latest_per_run(&self) -> Vec<Checkpoint>;
}

/// The real one: checkpoints are `EventKind::Checkpointed` events in the one append-only log.
///
/// **Reading is a fold over `Journal::replay`, filtered on the run id in the PAYLOAD** rather than
/// on the event row's `run_id` column. That is not a shortcut — it is what lets this ship without
/// touching `journal.rs`, which is a brief §13 guarded path. The payload's `run` is inside the
/// signed bytes, so filtering on it is if anything the stronger read.
pub struct JournalCheckpoints {
    journal: std::sync::Arc<std::sync::Mutex<Journal>>,
}

impl JournalCheckpoints {
    pub fn new(journal: std::sync::Arc<std::sync::Mutex<Journal>>) -> Self {
        Self { journal }
    }
}

impl JournalCheckpoints {
    /// Every decodable checkpoint in the log, in `seq` order.
    ///
    /// **Undecodable payloads are skipped rather than fatal**, and that is one rule with one
    /// implementation: the live journal holds 895 pre-M3 `{"step": n}` events, and a build that
    /// refused to start on them would make this change a migration. Both readers go through here
    /// so the skip cannot be right in one and forgotten in the other.
    fn decode_all(&self) -> Vec<Checkpoint> {
        let cap = OperatorCapability::for_operator_or_audit();
        self.journal
            .lock()
            .expect("the journal lock was poisoned")
            .replay(&cap, Some(EventKind::Checkpointed))
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(_, _, payload)| serde_json::from_value::<Checkpoint>(payload).ok())
            .collect()
    }
}

impl CheckpointStore for JournalCheckpoints {
    fn write(&mut self, cp: &Checkpoint, clock: marlowe_contract::Clock) -> Result<Seq, String> {
        let payload = serde_json::to_value(cp).map_err(|e| e.to_string())?;
        self.journal
            .lock()
            .expect("the journal lock was poisoned by a panicking append")
            .append(
                clock,
                marlowe_journal::AppendRequest {
                    // **The checkpoint's own trace, not one held here.** One trace across a tree
                    // is invariant 7's replay key, and a store that stamped its own would put a
                    // resumed run on a different trace from the run it continues.
                    trace_id: cp.trace_id,
                    session_id: Some(cp.session.to_string()),
                    run_id: Some(cp.run.to_string()),
                    actor: marlowe_journal::Actor::Harness,
                    kind: EventKind::Checkpointed,
                    payload,
                },
            )
            .map(|e| e.seq)
            .map_err(|e| e.to_string())
    }

    fn latest_per_run(&self) -> Vec<Checkpoint> {
        let mut by_run: std::collections::BTreeMap<RunId, Checkpoint> =
            std::collections::BTreeMap::new();
        for cp in self.decode_all() {
            by_run.insert(cp.run, cp);
        }
        by_run.into_values().collect()
    }

    fn latest(&self, run: RunId) -> Option<Checkpoint> {
        let cap = OperatorCapability::for_operator_or_audit();
        let rows = self
            .journal
            .lock()
            .expect("the journal lock was poisoned")
            .replay(&cap, Some(EventKind::Checkpointed))
            .ok()?;
        // Last wins: `replay` is ordered by `seq` ascending and a run's later checkpoint
        // supersedes its earlier one. A payload that does not decode is SKIPPED rather than
        // fatal — the live journal holds 895 pre-M3 `{"step": n}` payloads, and a build that
        // refused to start on them would make this change a migration.
        rows.into_iter()
            .rev()
            .filter_map(|(_, _, payload)| serde_json::from_value::<Checkpoint>(payload).ok())
            .find(|cp| cp.run == run)
    }
}

/// For tests and for the pre-daemon path. **Not a no-op** — it stores what the journal would, so
/// a test cannot assert on durability that did not happen.
#[derive(Debug, Default)]
pub struct MemoryCheckpoints {
    pub written: Vec<Checkpoint>,
}

impl MemoryCheckpoints {
    pub fn new() -> Self {
        Self::default()
    }
}

impl CheckpointStore for MemoryCheckpoints {
    fn write(&mut self, cp: &Checkpoint, _clock: marlowe_contract::Clock) -> Result<Seq, String> {
        self.written.push(cp.clone());
        Ok(self.written.len() as Seq)
    }

    fn latest(&self, run: RunId) -> Option<Checkpoint> {
        self.written.iter().rev().find(|c| c.run == run).cloned()
    }

    fn latest_per_run(&self) -> Vec<Checkpoint> {
        let mut by_run: std::collections::BTreeMap<RunId, Checkpoint> =
            std::collections::BTreeMap::new();
        for cp in &self.written {
            by_run.insert(cp.run, cp.clone());
        }
        by_run.into_values().collect()
    }
}

/// CONTRACTS.md §5: *"Children outlive parents. Parent completion does not kill a child."*
///
/// # The policy was DECLARED and unused for a milestone, and this is what enforcing it means
///
/// `OrphanPolicy` has been journalled at every spawn since M2 Session A, and nothing read it.
/// Enforcement is not a flag: it is an observable change to **the child**, and the three variants
/// must be distinguishable by looking at the child alone. So settlement writes a **new checkpoint
/// for the child**, which is the only durable record of a run that exists:
///
/// | Policy | What is written | What a later `resume` of the child does |
/// |---|---|---|
/// | `Terminate` | status `Cancelled` | refuses by name — `ResumeError::Terminated` |
/// | `Detach` | `parent: None` | resumes, with no parent |
/// | `Adopt { by }` | `parent: Some(by)` | resumes, under the new parent |
///
/// **No new `EventKind`.** CONTRACTS §1.1 pins the kind list, and a settlement is exactly what a
/// checkpoint already expresses: the state of a run at a moment. Adding a kind would have been a
/// schema change to a pinned contract for something the existing one already says.
///
/// Returns `None` when there is nothing to settle: the child already finished. **A completed
/// child is not an orphan**, and marking one would rewrite history.
///
/// Pure — it returns the amended checkpoint and leaves writing to the caller, because the two
/// callers write through different paths (the loop through `Recorder`, the daemon through a
/// [`CheckpointStore`]) and a decision with two implementations is a decision that drifts.
pub fn settle_orphan(
    checkpoint: &Checkpoint,
    policy: OrphanPolicy,
) -> Option<(Checkpoint, OrphanOutcome)> {
    if checkpoint.is_terminal() {
        return None;
    }
    let child = checkpoint.run;
    let mut cp = checkpoint.clone();
    let outcome = match policy {
        OrphanPolicy::Terminate => {
            cp.status = RunStatus::Cancelled;
            OrphanOutcome::Terminated { child }
        }
        OrphanPolicy::Detach => {
            cp.parent = None;
            OrphanOutcome::Detached { child }
        }
        OrphanPolicy::Adopt { by } => {
            cp.parent = Some(by);
            OrphanOutcome::Adopted { child, by }
        }
    };
    Some((cp, outcome))
}

/// [`settle_orphan`] against a store: read the child's latest checkpoint, decide, write the
/// amendment. The out-of-loop path — a daemon settling a tree it inherited across a restart.
///
/// **The decision lives in one function and this only moves bytes**, so the in-loop caller (which
/// writes through `Recorder`, the loop's single journal write path) and this one cannot drift.
pub fn settle_orphan_in(
    store: &mut dyn CheckpointStore,
    child: RunId,
    policy: OrphanPolicy,
    clock: marlowe_contract::Clock,
) -> Option<OrphanOutcome> {
    let cp = store.latest(child)?;
    let (amended, outcome) = settle_orphan(&cp, policy)?;
    store.write(&amended, clock).ok()?;
    Some(outcome)
}

/// What happened to one child when its parent ended. Reported so the fate can be asserted on and
/// announced, rather than inferred from the policy that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "fate")]
pub enum OrphanOutcome {
    Adopted { child: RunId, by: RunId },
    Detached { child: RunId },
    Terminated { child: RunId },
}

impl OrphanOutcome {
    pub fn child(&self) -> RunId {
        match self {
            OrphanOutcome::Adopted { child, .. }
            | OrphanOutcome::Detached { child }
            | OrphanOutcome::Terminated { child } => *child,
        }
    }

    /// The word the surface prints. Harness-authored, from a closed set.
    pub fn verb(&self) -> &'static str {
        match self {
            OrphanOutcome::Adopted { .. } => "adopted",
            OrphanOutcome::Detached { .. } => "detached",
            OrphanOutcome::Terminated { .. } => "terminated",
        }
    }
}
