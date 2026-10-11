//! **Where an escalation goes, decided by the run tree and never by a model** — M3-DESIGN §3.1,
//! ADR-065 §2.2.
//!
//! §3.1 is three sentences and each one is an arm below:
//!
//! * *"Escalation starts at the agent's **direct master** and must be **approved at each level**."*
//!   [`escalation_route`] answers one hop. The chain is walked by `EscalationDesk::advance`, which
//!   calls this function again for the run doing the forwarding.
//! * *"It is **scoped to the top-agent's subtree**. A worker can never address Marlowe."* There is
//!   **no arm that returns the conversational run**, and that absence *is* §3.2. A `Master` or
//!   `Worker` reaches `Parent(p)`; a `TopAgent` reaches [`EscalationRoute::User`] directly, so the
//!   hop that would have crossed into Marlowe does not exist to be taken.
//! * *"Only a **top-agent** may escalate to the user, and even then Marlowe is not the recipient."*
//!   [`EscalationRoute::User`] is reachable from `AgentLevel::TopAgent` and from nothing else.
//!
//! # This reads the level that already exists. It does not add a second one
//!
//! ADR-065 §2.1 proposed dropping `AgentLevel` in favour of a `RaisesTo` bit on `Run`, on the
//! grounds that `AgentLevel::ToolSpawned` was *"constructed by nothing"* and that a level on `Run`
//! is a second definition of tree position that `Run::adopted_by` and `Run::detached` silently
//! falsify. **Both halves of that argument are now false**, and the ADR predates the commit that
//! falsified them (`a017ee0`, 2026-08-30):
//!
//! * `AgentLevel` lives on [`crate::CapabilityProfile`], **not** on `Run`, as a private
//!   constructor-validated field. `adopted_by` and `detached` cannot falsify it because it is not
//!   a claim about the parent link at all — it is a claim about what the profile may hold, and
//!   `CapabilityProfile::new` enforces three rules against it at load time.
//! * `AgentLevel::ToolSpawned` **is** constructed: `CapabilityProfile::quarantined_reader()` names
//!   it, and `Engine::condense_batch` builds every layer-1 reader from that constructor.
//!
//! So a `RaisesTo` field would be the second definition, not the first. It would also be a
//! `Checkpoint` field and a `CHECKPOINT_VERSION` bump; reading the level off the profile needs
//! neither, because `Checkpoint::profile` already round-trips through
//! `CapabilityProfile`'s validating `Deserialize`.
//!
//! # The two refusals are both necessary, and neither implies the other
//!
//! [`NotRaisableReason::ReadsUntrusted`] and [`NotRaisableReason::ToolSpawned`] look redundant and
//! are not. `CapabilityProfile::new` enforces `reads_untrusted ⟹ empty tool set` and
//! `ToolSpawned ⟹ empty tool set`, but **neither implies the other**: a `Worker` profile with
//! `reads_untrusted` and no tools constructs today, and `SCOPED-MEMORY.md` §4's fact extractor is
//! a `ToolSpawned` run that does **not** set `reads_untrusted`. Delete either disjunct and a real
//! run gains a live route upward. `escalation_routing.rs` asserts each one with the other's
//! condition held false, so neither test can be passing on the other's behalf.

use marlowe_contract::EscalationId;

use crate::profile::AgentLevel;
use crate::run::{Run, RunId};

/// One hop upward. **There is no arm that returns the conversational run.**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscalationRoute {
    /// The raiser's direct parent, which for a `Master` or `Worker` is its master.
    Parent(RunId),
    /// The human. Reachable from `AgentLevel::TopAgent` and from nothing else.
    User,
    /// Nobody above, or a run that may not raise. **Carries why**: a channel that goes quiet is
    /// not a channel that is closed, and the first design's silent-vanish on `Detach` is the
    /// defect this field exists to make impossible.
    NotRaisable(NotRaisableReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotRaisableReason {
    /// The conversational run. §3.1: nobody above it.
    Root,
    /// Layer 1's quarantined reader. Its window holds raw attacker bytes (ADR-041), so its
    /// question is a statement about the model and not about the page — `QuarantineRefusal`'s
    /// `Escalated` variant already owns that wording.
    ReadsUntrusted,
    /// M3-DESIGN §1's level 5: spawned by a **tool**, not by a model. Covers
    /// `SCOPED-MEMORY.md` §4's fact extractor, which reads no untrusted content and would
    /// otherwise be raisable.
    ToolSpawned,
    /// `OrphanPolicy::Detach` cut the parent link. The raiser is told and the journal records it.
    Detached,
}

impl NotRaisableReason {
    /// The line the harness pushes into the raiser's own window, and the one the journal records.
    ///
    /// **One shared constant per reason, on `UNDESCRIBED_SOURCE`'s precedent** — a literal at each
    /// end is two spellings of one sentence, which is exactly the mismatch nobody observes.
    pub fn note(self) -> &'static str {
        match self {
            Self::Root => {
                "[escalation refused] this run is the conversation itself; there is nobody above \
                 it to raise to. Put the question to the user directly"
            }
            Self::ReadsUntrusted => {
                "[escalation refused] this run reads untrusted content, so it holds no upward \
                 channel: a question composed inside this window is a statement about the model, \
                 not about the source"
            }
            Self::ToolSpawned => {
                "[escalation refused] this run was created by a tool rather than by an agent, and \
                 has no master to raise to"
            }
            Self::Detached => {
                "[escalation refused] this run was detached from its parent, so there is no longer \
                 anyone above it. It was not silently dropped: the journal records the refusal"
            }
        }
    }
}

/// The line the harness pushes into the raiser's window when an escalation **was** delivered.
///
/// Harness-authored and carries no id the model could quote back at a human — the model learns
/// that it was heard and nothing about who is deciding.
pub const ESCALATION_RAISED: &str =
    "[escalation raised] it is with a human now, and this run is paused until they answer";

/// The line for an escalation the harness accepted the shape of but could not deliver, because
/// this build has no desk wired. Distinguished from a refusal on purpose: *"nobody is listening"*
/// and *"you may not raise"* are different facts and a model told the wrong one changes the wrong
/// thing.
pub const ESCALATION_NO_DESK: &str =
    "[escalation not delivered] this build has no escalation desk wired, so nothing was raised. \
     The run continues";

/// One hop, from the tree.
///
/// The `reads_untrusted` check comes **first**, before the level match, and the order is
/// load-bearing: a `Worker`-level profile that reads untrusted content is constructible today and
/// must be refused on the stronger ground, not routed to its parent because its level said so.
pub fn escalation_route(run: &Run) -> EscalationRoute {
    if run.profile.reads_untrusted() {
        return EscalationRoute::NotRaisable(NotRaisableReason::ReadsUntrusted);
    }
    match run.profile.level() {
        AgentLevel::ToolSpawned => EscalationRoute::NotRaisable(NotRaisableReason::ToolSpawned),
        AgentLevel::Secretary => EscalationRoute::NotRaisable(NotRaisableReason::Root),
        // §3.1's third sentence. **`parent` is deliberately not consulted**: a top-agent that was
        // detached is still a top-agent, and its route to the human is the one channel in the
        // system that must not be closable by a lifetime decision the agent itself declared.
        AgentLevel::TopAgent { .. } => EscalationRoute::User,
        AgentLevel::Master | AgentLevel::Worker => match run.parent {
            Some(p) => EscalationRoute::Parent(p),
            None => EscalationRoute::NotRaisable(NotRaisableReason::Detached),
        },
    }
}

/// Why a desk refused a record whose route was fine.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EscalationRefused {
    #[error(
        "no escalation desk is wired in this build, so there is nowhere for this to go. The run \
         is not paused and nothing was recorded as pending"
    )]
    NoDesk,
    #[error(
        "the escalation offered {count} options and the ceiling is {max}. Refused, never \
         truncated: dropping the fifth option is the harness silently choosing which of a \
         raiser's alternatives a human gets to see"
    )]
    TooManyOptions { count: usize, max: usize },
    #[error(
        "this run may not raise: {0:?}. The route is a property of the tree and a desk cannot \
         widen it"
    )]
    NotRaisable(NotRaisableReason),
    #[error("the escalation desk refused the record: {0}")]
    Rejected(String),
}

// **There is no `pub type Raised = Result<EscalationId, EscalationRefused>` here.** The first
// draft of this file had one, and nothing read it -- the trait in `driver.rs` spells the result
// out. An alias with no user is a declaration, which is the family this whole module is written
// against.
