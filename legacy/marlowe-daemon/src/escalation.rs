//! The escalation desk — M3-DESIGN §3.1's *"approved at each level"* and §3.2's *"a notification,
//! and nothing else"*.
//!
//! # Why Marlowe cannot read an escalation, stated as a fact about a type rather than a promise
//!
//! §3.2 is the requirement: *"He cannot read it, cannot query it, cannot summarise it. A window
//! opens: the user and the top-agent, directly. Marlowe is not in the room."* §2.1 is why it
//! matters more than it sounds — Marlowe is the permanent run, ADR-023's floor is monotonic, and a
//! Marlowe who ingests one finding can never compose a target again.
//!
//! The containment is **not** that a method here was named carefully, and it is not that Marlowe's
//! profile happens to expose no tool that calls one. A registry entry is configuration: an MCP
//! descriptor, a skill, a future `recall` variant or a well-meaning debugging path could all reach
//! a method that existed.
//!
//! **The loop reaches this type only as `dyn marlowe_loop::EscalationPort`, and that trait has one
//! method, which takes an `EscalationRequest` and returns an `EscalationId`.** There is nothing on
//! it that returns text, so there is no caller to add. Every richer accessor below is reachable
//! only from the daemon and the surface, which are on the other side of `Ports` — `marlowe-loop`
//! cannot name `marlowe-daemon` at all, so this is enforced by the dependency graph rather than by
//! review.
//!
//! [`EscalationDesk::secretary_notice`] is the whole of what a conversational run may learn, and
//! its return type is `Notice`, which ADR-030 §5 forbids from carrying a `String`. It has no `body`
//! parameter and no `&Escalation` return.
//!
//! # The authority rule, and why the trust floor cannot supply it
//!
//! [`EscalationDesk::advance`] takes `by: RunId` and checks it against [`Pending::at`] — the run
//! this escalation is *currently addressed to*. The desk is the authority for that, not the model
//! that named the id.
//!
//! **This is ADR-036 §5's authority rule at a saturated floor.** Inside an escalation every value
//! is `UntrustedContent`: the sentence, the labels, the artifact handle. ADR-023's floor therefore
//! reads "blocked" for every pending id and discriminates none of them — it is not broken, it is
//! saturated, and a saturated floor is silent in exactly the way a green probe beside a hung
//! product is silent. What survives saturation is not *how trusted is this id* but **who addressed
//! it here**, and that is a fact the desk holds and a model cannot compose.

use std::collections::BTreeMap;

use marlowe_contract::escalation::{
    ArtifactHandle, EscalationCategory, EscalationId, EscalationSeverity, OptionLabel,
    ValidatedSentence,
};
use marlowe_loop::escalation::{EscalationRefused, EscalationRoute};
use marlowe_loop::run::RunId;
use marlowe_loop::{EscalationPort, EscalationRequest};
use marlowe_view::escalation::{
    AgentOption, ArtifactPath, Choice, EscalationView, SourceEvidence, TerminationCost,
    TooManyOptions,
};
use marlowe_view::notice::{Echo, Notice};

/// Who an escalation is with, right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recipient {
    Run(RunId),
    /// The human. **`Recipient` has no `Secretary` variant and that absence is §3.2** — there is
    /// no value this type can take that names the conversational run, so no code path can deliver
    /// an escalation there by getting a condition wrong.
    User,
}

/// The record. **Serialize only, and never handed to a loop.**
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Escalation {
    pub id: EscalationId,
    pub raised_by: RunId,
    pub severity: EscalationSeverity,
    pub category: EscalationCategory,
    pub options: Vec<OptionLabel>,
    pub artifact: Option<ArtifactHandle>,
    /// The approving chain, newest last. **Appended by [`EscalationDesk::advance`], never by a
    /// model** — §3.1's *"approved at each level"* is this vector, and a model that could write it
    /// could claim an approval that never happened.
    pub lineage: Vec<RunId>,
    pub sentence: Option<ValidatedSentence>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pending {
    escalation: Escalation,
    /// **The authority field.** The one whose deletion no existing mechanism would notice: the
    /// trust floor reads "blocked" for every pending id and discriminates none.
    at: Recipient,
}

// **There is no `opened_ms`, and ADR-065 flagged it as conditional for exactly this reason.**
// Its note reads *"if the window does not render 'raised N minutes ago', drop it"*. The overlay
// renders the RUN's age, from the journal-derived `TerminationCost`, and nothing renders the
// escalation's own. A field with no reader is instance #16, so it is not here.

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AdvanceError {
    #[error("no escalation with that id is pending")]
    Unknown,
    #[error(
        "that escalation is not addressed to this run. Forwarding is a hop the DESK owns: a run \
         that could advance an escalation it was never handed could pull one out of a sibling's \
         chain by naming its id"
    )]
    NotYours,
    #[error("that escalation is already with the user; there is nowhere above to advance it to")]
    AlreadyWithTheUser,
    #[error("the forwarding run may not raise: {0:?}")]
    ForwarderCannotRaise(marlowe_loop::NotRaisableReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("no escalation with that id is pending")]
pub struct ResolveError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub id: EscalationId,
    pub raised_by: RunId,
    pub choice: Choice,
}

/// The desk. One per daemon.
#[derive(Debug, Default)]
pub struct EscalationDesk {
    /// `BTreeMap`, not `HashMap` — banned under `crates/`, and the ordering is what makes a
    /// listing reproducible.
    pending: BTreeMap<EscalationId, Pending>,
    /// Feeds [`EscalationId::for_raise`], so an id is a function of `(raiser, seq)` and is
    /// reproducible from the journal rather than drawn.
    seq: u32,
}

impl EscalationDesk {
    pub fn new() -> Self {
        Self::default()
    }

    // **`from_journal` is not built, and it is named here rather than faked.**
    //
    // ADR-065 §2.4 asks for the pending set to be rebuilt from the journal at boot, so a pending
    // escalation survives a restart in the milestone whose subject is durable runs. It is not
    // here, and the reason is a measurement rather than a preference: the loop records
    // `EventKind::ApprovalRequested` carrying only `{"escalation": <id>}`, so the log holds
    // *that* a raise happened and none of the record — no severity, no category, no options. A
    // `from_journal` written against today's events would return a set of empty escalations and
    // look like durability. Writing the full record through `Recorder` is the prerequisite and it
    // is one commit's work; doing it here without the accompanying event would be the durable
    // half of instance #16.
    //
    // This is a doc comment and not a `const NOT_BUILT: &str`, because a constant nothing
    // reads is the same defect one layer down.

    /// One hop. `by` MUST be the run this escalation is currently addressed to.
    ///
    /// `route` is the forwarding run's own route, computed by the loop from the run tree — the
    /// desk does not derive it, because a second answer to *"who is above this run"* is a second
    /// definition of the thing §3.1 is about.
    pub fn advance(
        &mut self,
        id: EscalationId,
        by: RunId,
        route: EscalationRoute,
    ) -> Result<Recipient, AdvanceError> {
        let p = self.pending.get_mut(&id).ok_or(AdvanceError::Unknown)?;
        match p.at {
            Recipient::Run(at) if at == by => {}
            Recipient::Run(_) => return Err(AdvanceError::NotYours),
            Recipient::User => return Err(AdvanceError::AlreadyWithTheUser),
        }
        let next = match route {
            EscalationRoute::Parent(parent) => Recipient::Run(parent),
            EscalationRoute::User => Recipient::User,
            EscalationRoute::NotRaisable(why) => {
                return Err(AdvanceError::ForwarderCannotRaise(why))
            }
        };
        // §3.1's *"approved at each level"*, as a fact the desk wrote down.
        p.escalation.lineage.push(by);
        p.at = next;
        Ok(next)
    }

    /// §3.2, **enforced by the signature**. No `body` parameter, no `&Escalation` return.
    ///
    /// The severity is a closed harness enum and `by` is composed from
    /// [`marlowe_loop::run::sayable`], the single definition of how a run is printed. **Zero model
    /// bytes reach the result.**
    ///
    /// `Echo` is a public-field newtype and does not sanitise anything — ADR-030 §5's "no `String`"
    /// is a discipline about who *composes* the value, not a check the type performs. The
    /// containment here rests on this composition site being harness-authored, and nothing in the
    /// type will remind the next person. Any future `Echo` built from model bytes needs
    /// `sanitize_line` at its own site.
    pub fn secretary_notice(&self, id: EscalationId) -> Option<Notice> {
        let p = self.pending.get(&id)?;
        Some(Notice::EscalationRaised {
            severity: p.escalation.severity,
            by: Echo::new(marlowe_loop::run::sayable(&p.escalation.raised_by.to_string())),
        })
    }

    /// What the surface draws. **Reachable from the daemon and the surface, never through
    /// `Ports`** — see the module header.
    ///
    /// `cost` and `evidence` are supplied by the caller because both are journal replays and the
    /// desk holds no journal. The model contributes neither and is never asked: they are §3.5's
    /// *"derived from the journal rather than from the agent"* and §3.6's provenance line.
    /// `path_of` turns the stored hash-only handle into a path the user can open. It is a closure
    /// because the desk holds no store root -- and it is what gives `Escalation::artifact` a
    /// reader, which is the difference between a field and a declaration.
    pub fn view_for(
        &self,
        id: EscalationId,
        cost: TerminationCost,
        evidence: SourceEvidence,
        path_of: impl Fn(&ArtifactHandle) -> ArtifactPath,
    ) -> Option<Result<EscalationView, TooManyOptions>> {
        let p = self.pending.get(&id)?;
        Some(EscalationView::new(
            Echo::new(marlowe_loop::run::sayable(&p.escalation.raised_by.to_string())),
            p.escalation.severity,
            p.escalation.category,
            p.escalation
                .options
                .iter()
                .cloned()
                .map(|label| AgentOption { label })
                .collect(),
            cost,
            evidence,
            p.escalation.artifact.as_ref().map(&path_of),
            p.escalation.sentence.clone(),
        ))
    }

    pub fn resolve(&mut self, id: EscalationId, choice: Choice) -> Result<Resolution, ResolveError> {
        let p = self.pending.remove(&id).ok_or(ResolveError)?;
        Ok(Resolution { id, raised_by: p.escalation.raised_by, choice })
    }

    pub fn pending_ids(&self) -> Vec<EscalationId> {
        self.pending.keys().copied().collect()
    }

    /// Test and daemon accessor for where an escalation currently sits. **Returns a `Recipient`,
    /// which is a run id or the user — no content.**
    pub fn addressed_to(&self, id: EscalationId) -> Option<Recipient> {
        self.pending.get(&id).map(|p| p.at)
    }
}

impl EscalationPort for EscalationDesk {
    fn raise(
        &mut self,
        raised_by: RunId,
        route: EscalationRoute,
        req: EscalationRequest,
    ) -> Result<EscalationId, EscalationRefused> {
        // **The route is re-read here, and it is not redundant.** `Engine` only calls `raise` on a
        // raisable route, so this arm is unreachable from today's one caller — and that is exactly
        // why it is here rather than trusted: the desk is a `pub` type in another crate, and a
        // second caller that skipped the check would otherwise deliver an escalation the tree
        // refuses. It costs one match and it can fail.
        let at = match route {
            EscalationRoute::Parent(p) => Recipient::Run(p),
            EscalationRoute::User => Recipient::User,
            EscalationRoute::NotRaisable(why) => return Err(EscalationRefused::NotRaisable(why)),
        };
        if req.options.len() > EscalationView::MAX_AGENT_OPTIONS {
            return Err(EscalationRefused::TooManyOptions {
                count: req.options.len(),
                max: EscalationView::MAX_AGENT_OPTIONS,
            });
        }
        self.seq = self.seq.saturating_add(1);
        let id = EscalationId::for_raise(raised_by.0, self.seq);
        self.pending.insert(
            id,
            Pending {
                escalation: Escalation {
                    id,
                    raised_by,
                    severity: req.severity,
                    category: req.category,
                    options: req.options,
                    artifact: req.artifact,
                    lineage: Vec::new(),
                    sentence: req.sentence,
                },
                at,
            },
        );
        Ok(id)
    }
}
