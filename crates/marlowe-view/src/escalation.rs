//! The escalation overlay's shape — M3-DESIGN §3.4, §3.5, §3.6.
//!
//! # The whole file in one sentence
//!
//! **[`EscalationView`] has no field for TERMINATE, and that absence is the enforcement.**
//!
//! §3.4 asks for a control that *"the agent does not know exists — not in its tool set, not in its
//! system prompt, not describable, not styleable, not annotatable. Fixed position, harness-authored
//! wording, always present."* Five requirements, and four of them are about what a producer must
//! not be able to do to it. A `terminate: bool` on this type would satisfy none of them: a producer
//! could set it false, a test asserting it was true would be green on a build that never drew the
//! row, and M3-DESIGN §3.4's own last line says so — *"asserting a flag is set is the declaration,
//! not the enforcement."*
//!
//! So there is no flag. A producer cannot suppress the row, cannot label it, cannot style it,
//! cannot annotate it and cannot move it, **because this type has nowhere to put any of those**.
//! It is `BlastRadius`'s argument run backwards: there, a surface cannot show what it was never
//! given; here, a surface cannot omit what it was never given. The row's text is
//! [`TERMINATE_LABEL`], a `&'static str` in this file, and its position is computed by the
//! surface's layout **before** the option list is given any rows at all.
//!
//! # Why `marlowe-view` now has a dependency, when its description says *"Shapes only"*
//!
//! This crate's `[dependencies]` was empty and its package description reads *"Shapes only —
//! nothing here can produce a value."* Adding `marlowe-contract` is a change of stance and it has a
//! `DECISIONS.md` entry rather than a claimed precedent — `marlowe-surface`'s `Cargo.toml` carries
//! the *argument* (*"`marlowe-contract` is the deepest crate in the workspace and holds no
//! producer, so this does not weaken the note above"*), but that is a different crate one layer up,
//! so the argument transfers and the precedent does not.
//!
//! The alternative was a second [`marlowe_contract::is_renderable`] inside the shapes crate, and
//! two answers to *"which characters may reach a terminal"* on the surface where SECURITY-AUDIT B1
//! and B2 both live is the worse trade. The stance is intact in the way that matters: nothing
//! imported here can produce a value either.

use marlowe_contract::escalation::{
    EscalationCategory, EscalationSeverity, OptionLabel, ValidatedSentence,
};

use crate::notice::Echo;
use crate::run::OrphanPolicyLabel;

/// What the human reads on the harness's own row.
///
/// **Narrower than the first draft's *"terminate this agent and everything under it"*, because the
/// harness cannot keep that promise.** `Control::cancel` is per-run — `control.rs`'s own test is
/// named `cancelling_one_run_does_not_cancel_its_sibling` — `CONTRACTS.md` §5 pins *"Children
/// outlive parents. Parent completion does not kill a child"*, and `settle_orphan` makes
/// `OrphanPolicy::Detach` set `parent = None` and the run survive. **There is no subtree cancel in
/// the workspace.** §3.5's entire subject is that *"reverts the scope"* is three different promises
/// and that this row must tell the truth about which one it keeps, so the label says less and
/// [`TerminationCost::survivors`] says the rest.
pub const TERMINATE_LABEL: &str = "terminate this agent and the runs under it";

/// **What a test reads, and it is deliberately not the word `terminate`.**
///
/// M3-DESIGN §11's original row asked for *"TERMINATE present in an agent's `request_body`: 0
/// occurrences"*, and **that row is red on a correct build**: `terminate` ships to every model
/// holding `run` from `marlowe-tools/src/builtin.rs`'s `orphan_policy` parameter description, and
/// again from the spawn receipt `marlowe-loop/src/engine.rs` pushes into a parent's history. Two
/// unrelated things share the word — `OrphanPolicy::Terminate` is a declared, model-nameable,
/// harmless lifecycle value, and the escape hatch is a control the agent must not be able to
/// invoke, describe, suppress or style.
///
/// The repair that must not be made is narrowing the search until the zero comes back. This token
/// is the repair that may be made: it exists in this file, in the surface that draws the row, and
/// in the two tests that look for it — **and in no type any `ModelDriver` can reach**.
///
/// It is read by the product, not only by tests: [`Choice::wire`] is its spelling, so a canary
/// that went missing from the binary would take the resolution path with it rather than quietly
/// weakening an assertion.
pub const TERMINATE_CANARY: &str = "harness-escape-hatch-6b1f";

/// What the human picked. `Agent(i)` indexes [`EscalationView::options`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Agent(u8),
    Terminate,
}

impl Choice {
    /// The spelling that crosses the control plane. **`Terminate`'s is the canary**, which is what
    /// makes the canary a product string rather than a test fixture — a fixture cannot go missing
    /// from a request body, because it was never in any code path that builds one.
    pub fn wire(self) -> String {
        match self {
            Choice::Agent(i) => format!("option-{i}"),
            Choice::Terminate => TERMINATE_CANARY.to_string(),
        }
    }
}

/// One row the **agent** wrote. Attacker-controlled by assumption — §3.4's premise is that a
/// compromised top-agent writes every one of these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentOption {
    pub label: OptionLabel,
}

/// **Hand-written `Display` rather than `thiserror`.** This crate had an empty `[dependencies]`
/// and gains exactly one edge in this change, argued in a `DECISIONS.md` entry; a second edge for
/// a derive that saves four lines would be that decision widened by convenience rather than by
/// argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooManyOptions {
    pub count: usize,
    pub max: usize,
}

impl std::fmt::Display for TooManyOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "an escalation offered {} options and the ceiling is {}. Refused at construction, \
             never truncated -- and deliberately NOT a budget dimension: `Budget::exhausted` \
             compares `spent >= budget`, so a ceiling written as a counter of zero reads as \
             already-exhausted (CLAUDE.md instance #17)",
            self.count, self.max
        )
    }
}

impl std::error::Error for TooManyOptions {}

/// Everything the escalation overlay draws. **No TERMINATE field — see the module header.**
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EscalationView {
    /// The raiser's name, composed by the producer from `marlowe_loop::run::sayable` — the single
    /// definition of how a run is printed. **Not a model-chosen display name**: a string the agent
    /// picked, rendered to a human about to choose between labelled rows, can name itself
    /// `Marlowe`, `SYSTEM`, or the label of the row beside it.
    pub raised_by: Echo,
    pub severity: EscalationSeverity,
    pub category: EscalationCategory,
    /// Private and bounded at construction. A surface reads it through [`Self::options`] and
    /// therefore can never be handed more rows than the layout reserved space for.
    options: Vec<AgentOption>,
    pub cost: TerminationCost,
    /// §3.6's *"show provenance, not just a caution"*. Named `SourceEvidence` rather than
    /// `Provenance`, which is already `marlowe_loop::provenance`'s taint attribution (layer-3
    /// machinery, in a §13-guarded file) and `marlowe-memory`'s gate. Three definitions of one
    /// load-bearing word, two of them security machinery, is the collision this name avoids.
    pub evidence: SourceEvidence,
    /// A path the **user** opens, composed by the producer from a hash-only handle. The record
    /// carries no text from the artifact and this field carries none either.
    pub artifact: Option<ArtifactPath>,
    /// M3-DESIGN section 9.1 A8's middle arm, and **the reason the field on the request is not
    /// dropped**: a `sentence` the model may set and nothing renders is instance #16, so it is
    /// here, and `marlowe_surface::escalation::draw_escalation` puts it on the header's third row
    /// through `chrome::prepare_model_text`.
    ///
    /// It is a `ValidatedSentence`, so it has already been through `sanitize_line` and a cap;
    /// running the model-text pipeline over it again at draw time is not belt-and-braces, because
    /// that pass reserves the CHROME and `ValidatedSentence` knows nothing about chrome.
    pub sentence: Option<ValidatedSentence>,
}

impl EscalationView {
    pub const MAX_AGENT_OPTIONS: usize = 4;

    pub fn new(
        raised_by: Echo,
        severity: EscalationSeverity,
        category: EscalationCategory,
        options: Vec<AgentOption>,
        cost: TerminationCost,
        evidence: SourceEvidence,
        artifact: Option<ArtifactPath>,
        sentence: Option<ValidatedSentence>,
    ) -> Result<Self, TooManyOptions> {
        if options.len() > Self::MAX_AGENT_OPTIONS {
            return Err(TooManyOptions {
                count: options.len(),
                max: Self::MAX_AGENT_OPTIONS,
            });
        }
        Ok(Self { raised_by, severity, category, options, cost, evidence, artifact, sentence })
    }

    pub fn options(&self) -> &[AgentOption] {
        &self.options
    }
}

/// A filesystem path the surface offers the user. Producer-composed from an
/// `marlowe_contract::ArtifactHandle`, which is hex and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactPath(pub String);

/// §3.5, **derived from the journal**. The model contributes nothing and is never asked, because
/// these are exactly the facts a compromised agent would lie about: *"you'll lose six hours, the
/// migration is half-done."*
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminationCost {
    pub runs: u32,
    pub age_ms: u64,
    pub spend_micros_usd: u64,
    pub artifacts: u32,
    /// What cannot be undone. A growing set by its own rule, which is why the surface lays the
    /// terminate row out first: growth here must not push the escape hatch off the frame.
    pub irreversible: Vec<IrreversibleAct>,
    /// **What TERMINATE will NOT stop.** Computed by replaying the subtree's declared orphan
    /// policies. Without this field [`TERMINATE_LABEL`] is a lie the harness tells, on the one row
    /// the harness authors specifically because the agent would lie.
    pub survivors: Vec<Survivor>,
}

impl TerminationCost {
    /// One row per field, and the reason the count is asserted rather than the values: a field
    /// added here with no row is instance #16, and `every_termination_cost_field_is_rendered`
    /// mutates each field and asserts the rendering changes.
    pub const ROWS: usize = 6;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Survivor {
    /// The child's harness-derived name, from `sayable`.
    pub run: Echo,
    pub policy: OrphanPolicyLabel,
}

/// A closed set of things the journal records as already done. **No free text**: each variant
/// carries counts, which is what makes the row un-forgeable by the run that produced them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrreversibleAct {
    FilesWritten { count: u32 },
    CommitsPushed { count: u32 },
    MessagesSent { count: u32 },
    ProcessesRun { count: u32 },
}

/// §3.6: *"'If this seems tainted' asks the user to detect a well-written lie. 'This agent has read
/// 14 external sources; this claim traces to `example.com`, fetched 40 minutes ago' is checkable."*
///
/// **Total, not an `Option`.** `NoExternalSources` is a claim a producer makes on purpose, exactly
/// as `Novelty::Routine` is — an `Option` a producer may omit while nothing reports the omission is
/// the permissive-default family on the surface where a human is about to choose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceEvidence {
    NoExternalSources,
    External { sources: u32, most_recent: SourceTrace },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceTrace {
    /// **The host the egress layer resolved**, never a string the agent wrote. A model-supplied
    /// host on this row is the display attack with the provenance line as its vector.
    pub host: HostLabel,
    pub fetched_ms_ago: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostLabel(pub String);
