//! CONTRACTS.md §5's `RunControl`.
//!
//! **Two implementations, and the ephemeral one is kept rather than replaced.**
//!
//! [`EphemeralControl`] is M2's: the parent blocks, the child returns, the child dies with the
//! parent, and `resume` refuses **by name**. [`DurableControl`] is M3's: it reads and writes
//! [`crate::durable::Checkpoint`]s, so `resume` reconstructs a run that a daemon restart, a
//! provider failover or a host reboot ended.
//!
//! A no-op `resume` would be the worst available shape: a durable-run control plane whose resume
//! path returns `Ok(())` and does nothing is a K5 failure that reports as a pass. That was the
//! reason the M2 stub refused by name, and it is why
//! `resume_refuses_by_name_rather_than_succeeding_quietly` still runs against the ephemeral
//! control — **a build where every control resumes is a build where nothing tells you that one of
//! them cannot.**
//!
//! # Addressing: a control plane whose methods ignore their run id is not a control plane
//!
//! M2's `steer(run, …)` and `cancel(run)` took a `RunId` and dropped it, which was honest for a
//! world with one addressable run. M3's tree is depth four, and §10.1 requires steering *"from
//! outside"* — another terminal, no TUI, a script — which means naming the run. So [`Control`]'s
//! own hooks now carry the id, and [`DurableControl`] routes on it. `EphemeralControl` keeps the
//! broadcast behaviour it always had, stated rather than inherited.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::driver::{Control, SteerMessage};
use crate::durable::{Checkpoint, CheckpointStore, CHECKPOINT_VERSION};
use crate::run::{RunId, RunStatus};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResumeError {
    #[error(
        "run {run} cannot be resumed: this build spawns ephemerally, so there is no durable \
         checkpoint to resume from. Durable runs are M3 (K5)"
    )]
    NotDurable { run: RunId },
    /// The durable control looked and found nothing.
    ///
    /// **Distinct from `NotDurable` on purpose.** One says *this control plane cannot resume
    /// anything*; the other says *this control plane can, and this particular run left no
    /// checkpoint*. Collapsing them would make a missing checkpoint read as a missing feature.
    #[error(
        "run {run} has no checkpoint in the journal. Either it never ran under a durable control, \
         or it ended before its first step completed"
    )]
    NoCheckpoint { run: RunId },
    /// Refused rather than interpreted. See [`CHECKPOINT_VERSION`].
    #[error(
        "run {run}'s checkpoint is version {found} and this build writes version {expected}. It is \
         refused rather than read with defaults: every field this build would default is a \
         security property, and the default of each one is its permissive value"
    )]
    UnknownVersion { run: RunId, found: u16, expected: u16 },
    #[error(
        "run {run} was cancelled and will not resume. A run whose parent ended under \
         `OrphanPolicy::Terminate` is cancelled, and that is the policy taking effect rather than \
         a fault"
    )]
    Terminated { run: RunId },
    #[error("run {run} already finished ({status}); there is no step after its last one")]
    AlreadyFinished { run: RunId, status: String },
}

/// The control plane. `spawn` is absent on purpose: a spawn is the loop re-entering itself (see
/// `engine::Engine::spawn`), and a `spawn` method here would be a second entry point into run
/// creation that the scheduler would then have to unify.
pub trait RunControl {
    fn steer(&mut self, run: RunId, guidance: SteerMessage);
    fn cancel(&mut self, run: RunId);
    /// Durably record a run's state. Returns the journal sequence, or the reason it could not be
    /// written — **never a silent success**, because a checkpoint nobody wrote is exactly the
    /// state a resume cannot distinguish from a run that never got that far.
    fn checkpoint(&mut self, cp: &Checkpoint, clock: marlowe_contract::Clock) -> Result<u64, String>;
    /// Stage a run for resumption from its last completed step.
    ///
    /// It does not run the loop: the pinned signature has no ports, and inventing an engine here
    /// would put a second driving loop in this crate — which `tests/hp10_budgets.rs` fails the
    /// build over, correctly. `Ok(())` means a real checkpoint was found, decoded and accepted;
    /// the caller takes it with [`DurableControl::take_resumed`] and hands it to the engine.
    fn resume(&mut self, run: RunId) -> Result<(), ResumeError>;
}

/// The M2 implementation: steering and cancellation for the run the user is watching.
///
/// **Broadcast, and that is stated rather than inherited.** One queue, one flag: whichever run
/// asks next takes the steer, and a cancel stops the whole stack. That was correct when the only
/// addressable run was the one in front of the user, and it is the behaviour every M2 test was
/// written against. [`DurableControl`] is where addressing lives.
#[derive(Debug, Default)]
pub struct EphemeralControl {
    steers: VecDeque<SteerMessage>,
    interrupts: VecDeque<String>,
    cancelled: bool,
}

impl EphemeralControl {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue guidance for the next iteration boundary. Ordered, and never applied mid-tool-call.
    pub fn push_steer(&mut self, steer: SteerMessage) {
        self.steers.push_back(steer);
    }

    pub fn push_interrupt(&mut self, text: impl Into<String>) {
        self.interrupts.push_back(text.into());
    }
}

impl Control for EphemeralControl {
    fn cancelled(&self, _run: RunId) -> bool {
        self.cancelled
    }

    fn take_steer(&mut self, _run: RunId) -> Option<SteerMessage> {
        self.steers.pop_front()
    }

    fn take_interrupt(&mut self) -> Option<String> {
        self.interrupts.pop_front()
    }
}

impl RunControl for EphemeralControl {
    fn steer(&mut self, _run: RunId, guidance: SteerMessage) {
        self.push_steer(guidance);
    }

    fn cancel(&mut self, _run: RunId) {
        self.cancelled = true;
    }

    fn checkpoint(&mut self, _cp: &Checkpoint, _clock: marlowe_contract::Clock) -> Result<u64, String> {
        Err("this build has no checkpoint store; runs are ephemeral (K5, M3)".to_string())
    }

    fn resume(&mut self, run: RunId) -> Result<(), ResumeError> {
        Err(ResumeError::NotDurable { run })
    }
}

/// The M3 implementation: a run is addressable, its state is in the journal, and `resume` reads
/// it back.
///
/// # Steering is routed, never broadcast
///
/// `steers` is keyed by [`RunId`]. Scope item 3 is *"`steer` injects guidance into a running child
/// with no restart"*, and a broadcast queue cannot express that: the parent is the run at the top
/// of the stack and would take the message first. The engine asks
/// `Control::take_steer(self_run_id)` at each iteration boundary, so a steer addressed to a child
/// waits in the child's queue and is picked up by the child — while it is still running, at the
/// next boundary, with no restart. That is what "mid-flight" means here.
///
/// # A steer cannot restore a privilege, and that is structural rather than checked
///
/// §6.1 corrected an earlier draft: the run window's steer field **is a write**, and takes the
/// same adjudication as `/steer`. What that adjudication amounts to is worth stating, because it
/// is not a branch anywhere: a steer enters the window as a `SourceKind::History` block at
/// `TrustClass::UserAsserted`, and ADR-023's floor is a *monotonic latch* —
/// `Run::latch_trust_floor` only ever lowers it. So no steer, from the window or from `/steer`,
/// can raise a latched floor.
/// `tests/durable_resume.rs::steering_a_latched_run_does_not_restore_composed_targets` asserts it
/// where it is enforced rather than where it is declared.
pub struct DurableControl<S: CheckpointStore> {
    store: S,
    steers: BTreeMap<RunId, VecDeque<SteerMessage>>,
    interrupts: VecDeque<String>,
    cancelled: BTreeSet<RunId>,
    resumed: Option<Checkpoint>,
}

impl<S: CheckpointStore> DurableControl<S> {
    pub fn new(store: S) -> Self {
        Self {
            store,
            steers: BTreeMap::new(),
            interrupts: VecDeque::new(),
            cancelled: BTreeSet::new(),
            resumed: None,
        }
    }

    pub fn push_interrupt(&mut self, text: impl Into<String>) {
        self.interrupts.push_back(text.into());
    }

    /// The checkpoint a successful [`RunControl::resume`] staged. Taken once.
    pub fn take_resumed(&mut self) -> Option<Checkpoint> {
        self.resumed.take()
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    pub fn store_mut(&mut self) -> &mut S {
        &mut self.store
    }

    /// How many steers are still queued for a run. For the surface and for tests; a steer that
    /// was accepted and never delivered is otherwise indistinguishable from one that landed.
    pub fn pending_steers(&self, run: RunId) -> usize {
        self.steers.get(&run).map_or(0, VecDeque::len)
    }
}

impl<S: CheckpointStore> Control for DurableControl<S> {
    fn cancelled(&self, run: RunId) -> bool {
        self.cancelled.contains(&run)
    }

    fn take_steer(&mut self, run: RunId) -> Option<SteerMessage> {
        self.steers.get_mut(&run)?.pop_front()
    }

    fn take_interrupt(&mut self) -> Option<String> {
        self.interrupts.pop_front()
    }
}

impl<S: CheckpointStore> RunControl for DurableControl<S> {
    fn steer(&mut self, run: RunId, guidance: SteerMessage) {
        self.steers.entry(run).or_default().push_back(guidance);
    }

    fn cancel(&mut self, run: RunId) {
        self.cancelled.insert(run);
    }

    fn checkpoint(&mut self, cp: &Checkpoint, clock: marlowe_contract::Clock) -> Result<u64, String> {
        self.store.write(cp, clock)
    }

    fn resume(&mut self, run: RunId) -> Result<(), ResumeError> {
        let cp = self.store.latest(run).ok_or(ResumeError::NoCheckpoint { run })?;
        if cp.version != CHECKPOINT_VERSION {
            return Err(ResumeError::UnknownVersion {
                run,
                found: cp.version,
                expected: CHECKPOINT_VERSION,
            });
        }
        match &cp.status {
            RunStatus::Cancelled => return Err(ResumeError::Terminated { run }),
            RunStatus::Completed => {
                return Err(ResumeError::AlreadyFinished { run, status: "completed".into() })
            }
            RunStatus::Failed { error } => {
                return Err(ResumeError::AlreadyFinished {
                    run,
                    // The error text is the harness's own and is already in the journal; it is
                    // repeated here because a person reading `/runs` needs it and nobody else does.
                    status: format!("failed: {error}"),
                })
            }
            _ => {}
        }
        // A resumed run is cancelled no longer: the cancellation that stopped it was the daemon
        // going down, not a decision. A run cancelled by POLICY is refused above and never
        // reaches here.
        self.cancelled.remove(&run);
        self.resumed = Some(cp);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::Budget;
    use crate::context::SessionState;
    use crate::driver::Urgency;
    use crate::durable::MemoryCheckpoints;
    use crate::profile::CapabilityProfile;
    use crate::run::{OutputContract, Run, SessionId};

    fn a_clock() -> marlowe_contract::Clock {
        marlowe_contract::Clock::new(1_780_000_000_000)
    }

    fn a_run(name: &str) -> Run {
        Run::root(
            RunId::from_name(name),
            SessionId::from_name(name),
            CapabilityProfile::interactive(),
            Budget::interactive(),
            OutputContract::answer(),
        )
    }

    #[test]
    fn resume_refuses_by_name_rather_than_succeeding_quietly() {
        let mut c = EphemeralControl::new();
        let run = RunId::from_name("r");
        let e = c.resume(run).unwrap_err();
        assert_eq!(e, ResumeError::NotDurable { run });
        assert!(e.to_string().contains("M3"), "the refusal must name where it lands: {e}");
    }

    #[test]
    fn steering_is_ordered() {
        let mut c = EphemeralControl::new();
        let run = RunId::from_name("r");
        c.steer(run, SteerMessage { text: "first".into(), urgency: Urgency::Advisory });
        c.steer(run, SteerMessage { text: "second".into(), urgency: Urgency::Advisory });
        assert_eq!(c.take_steer(run).unwrap().text, "first");
        assert_eq!(c.take_steer(run).unwrap().text, "second");
        assert!(c.take_steer(run).is_none());
    }

    #[test]
    fn a_durable_resume_with_no_checkpoint_names_the_absence_not_the_feature() {
        // The two refusals are different facts and the messages say so. Collapsing them would
        // make "this run left nothing" read as "this build cannot resume".
        let mut c = DurableControl::new(MemoryCheckpoints::new());
        let run = RunId::from_name("r");
        assert_eq!(c.resume(run).unwrap_err(), ResumeError::NoCheckpoint { run });
        assert!(!c.resume(run).unwrap_err().to_string().contains("ephemeral"));
    }

    #[test]
    fn a_steer_addressed_to_a_child_is_not_taken_by_the_parent() {
        // The property scope item 3 rests on. With one broadcast queue the parent -- which is the
        // run at the top of the stack -- takes the message first, and the child is never steered.
        let mut c = DurableControl::new(MemoryCheckpoints::new());
        let parent = RunId::from_name("parent");
        let child = RunId::from_name("child");
        c.steer(child, SteerMessage { text: "narrow it".into(), urgency: Urgency::Advisory });

        assert!(c.take_steer(parent).is_none(), "the parent must not take the child's steer");
        assert_eq!(c.take_steer(child).unwrap().text, "narrow it");
    }

    #[test]
    fn cancelling_one_run_does_not_cancel_its_sibling() {
        let mut c = DurableControl::new(MemoryCheckpoints::new());
        let a = RunId::from_name("a");
        let b = RunId::from_name("b");
        c.cancel(a);
        assert!(c.cancelled(a));
        assert!(!c.cancelled(b), "cancel names a run; broadcasting would kill the tree");
    }

    #[test]
    fn a_checkpoint_of_an_unknown_version_is_refused_rather_than_read_with_defaults() {
        let mut store = MemoryCheckpoints::new();
        let run = a_run("versioned");
        let mut cp = Checkpoint::capture(&run, &SessionState::default(), 3, 0);
        cp.version = CHECKPOINT_VERSION + 7;
        store.write(&cp, a_clock()).unwrap();

        let mut c = DurableControl::new(store);
        assert_eq!(
            c.resume(run.id).unwrap_err(),
            ResumeError::UnknownVersion {
                run: run.id,
                found: CHECKPOINT_VERSION + 7,
                expected: CHECKPOINT_VERSION,
            }
        );
        assert!(c.take_resumed().is_none(), "a refused resume must stage nothing");
    }

    #[test]
    fn a_cancelled_run_refuses_to_resume_and_says_the_policy_did_it() {
        let mut store = MemoryCheckpoints::new();
        let mut run = a_run("cancelled");
        run.status = RunStatus::Cancelled;
        store.write(&Checkpoint::capture(&run, &SessionState::default(), 2, 0), a_clock()).unwrap();

        let mut c = DurableControl::new(store);
        let e = c.resume(run.id).unwrap_err();
        assert_eq!(e, ResumeError::Terminated { run: run.id });
        assert!(e.to_string().contains("Terminate"), "the refusal names the policy: {e}");
    }

    #[test]
    fn resume_stages_a_real_checkpoint_and_hands_it_over_exactly_once() {
        let mut store = MemoryCheckpoints::new();
        let mut run = a_run("live");
        run.status = RunStatus::Running;
        store.write(&Checkpoint::capture(&run, &SessionState::default(), 9, 1), a_clock()).unwrap();

        let mut c = DurableControl::new(store);
        assert_eq!(c.resume(run.id), Ok(()));
        let staged = c.take_resumed().expect("resume staged nothing after returning Ok");
        assert_eq!(staged.step, 9);
        assert_eq!(staged.contract_retries, 1);
        assert!(c.take_resumed().is_none(), "taken once; a second take would resume twice");
    }
}
