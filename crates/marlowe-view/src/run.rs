//! What a run window draws. `M3-DESIGN.md` §6.2, CONTRACTS.md §5.
//!
//! # These are shapes, and they mirror the control plane rather than importing it
//!
//! This crate has **no dependencies** — that is what makes "the surface holds no policy" a property
//! of `Cargo.toml` instead of a claim in a header. So [`RunState`] and [`OrphanPolicyLabel`] mirror
//! `marlowe_loop::{RunStatus, OrphanPolicy}` the same way [`crate::Tab`] and
//! `marlowe_surface::TabId` mirror each other: one owns the decision, the other owns the pixels, and
//! neither can be constructed from the other's crate.
//!
//! The mirroring is exhaustive on purpose. A `RunState::Other(String)` would let a new control-plane
//! status arrive as an unstyled word with no state colour, which is the "defaults that make a
//! mismatch unobservable" family — the window would render a run it did not understand and look
//! fine doing it.
//!
//! # One state, two renderings
//!
//! §6.6: *"The window renders the same control-plane state the **Runs** tab reports and never keeps
//! its own."* [`RunView`] is that state. Everything about *looking at* a run — the steer draft, the
//! scroll offset, which reasoning block is expanded, what time it is — lives on the surface's
//! `WindowApp`, exactly as [`crate::SessionView`]'s header says of the main pane.
//!
//! **No `Default`, and no constructor that invents values**, for the reason [`crate::SessionView`]
//! gives: a surface able to conjure one of these is a surface that can hold state the daemon lacks,
//! and every test would stay green while it did.

use crate::model::{Entry, Item};

/// Render-only mirror of `marlowe_loop::RunStatus`.
///
/// **Exhaustive, with no escape hatch.** See the module header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunState {
    Queued,
    Running,
    /// Blocked on a §B9 decision. The decision id is what a `/runs` listing prints beside it.
    WaitingApproval { decision: u64 },
    WaitingEvent,
    /// `reason` is the control plane's own words — a budget dimension, an approval, an answer.
    Paused { reason: String },
    Completed,
    Failed { error: String },
    Cancelled,
}

impl RunState {
    /// The word on the identity line. Harness-authored, one per variant.
    pub fn name(&self) -> &'static str {
        match self {
            RunState::Queued => "queued",
            RunState::Running => "running",
            RunState::WaitingApproval { .. } => "waiting for you",
            RunState::WaitingEvent => "waiting",
            RunState::Paused { .. } => "paused",
            RunState::Completed => "completed",
            RunState::Failed { .. } => "failed",
            RunState::Cancelled => "cancelled",
        }
    }

    /// §B2: **state colours encode state, never category.** This is the only place a run's status
    /// chooses a tone, so there is no second table to drift from this one.
    ///
    /// `Running` is deliberately [`Tone::Normal`] and not the accent: on a screen whose entire
    /// subject is one running run, accenting the ordinary case spends the budget's one accent on the
    /// thing that needs no attention.
    pub fn tone(&self) -> crate::Tone {
        match self {
            RunState::Queued | RunState::Running | RunState::Completed => crate::Tone::Normal,
            RunState::WaitingApproval { .. } | RunState::WaitingEvent | RunState::Paused { .. } => {
                crate::Tone::Amber
            }
            RunState::Failed { .. } | RunState::Cancelled => crate::Tone::Red,
        }
    }

    /// Whether the clock is still moving. Elapsed freezes when it stops — a finished run whose
    /// elapsed kept counting would be the surface inventing a fact.
    pub fn is_live(&self) -> bool {
        matches!(
            self,
            RunState::Queued
                | RunState::Running
                | RunState::WaitingApproval { .. }
                | RunState::WaitingEvent
                | RunState::Paused { .. }
        )
    }
}

/// Render-only mirror of `marlowe_loop::OrphanPolicy`. **Declared at spawn, never inferred** — so
/// this is reported, never computed from what the window can see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrphanPolicyLabel {
    /// Reparented to another run. The id is the adopting run's, short form.
    Adopt { by: String },
    Detach,
    Terminate,
}

impl OrphanPolicyLabel {
    /// What cancelling actually does to this run's children, in a sentence a person can act on.
    ///
    /// §6.2 asks for the policy *"stated plainly"*. The word `Detach` is not plain; "children keep
    /// running with no parent" is, and it is the difference between a confirmation the user
    /// understood and one they clicked through.
    pub fn plainly(&self) -> String {
        match self {
            OrphanPolicyLabel::Adopt { by } => {
                format!("children are reparented to run {by} and keep running")
            }
            OrphanPolicyLabel::Detach => "children keep running with no parent".to_string(),
            OrphanPolicyLabel::Terminate => "children end with it".to_string(),
        }
    }
}

/// **The field that makes this window a debugging instrument** (§6.2).
///
/// Two facts, and they are separate because they answer different questions: *what did this run
/// finish* and *could it be resumed*. A window that showed only the first would invite the reader
/// to infer the second, and inferring it is exactly what goes wrong around a half-built resume.
///
/// **Both come from the checkpoint store, not from the run table.** Session A's `ControlPlane::
/// detail` puts it exactly right: the table is what the daemon remembers and the store is what
/// survived, and a resume is about what survived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointView {
    /// The last completed step. **`None` means no checkpoint exists — not step zero.**
    pub last_completed: Option<u32>,
    /// Whether `RunControl::resume` would take this run further.
    ///
    /// **A fact the control plane computed, never a guess.** `false` for a run that completed,
    /// failed or was cancelled; the window then says the run cannot be resumed *from here* and
    /// invents no reason of its own, because it has none.
    pub resumable: bool,
}

/// Everything a run window draws, and nothing about looking at it.
///
/// The four `Vec<Item>` panels at the bottom are §6.3's placeholders. **They are empty because
/// nothing produces them yet, and empty is a fact**: `Subagents — none` is true now, stays true for
/// a childless run once the tree lands, and the same panel simply fills. There is no variant for
/// "not built" and no string naming a milestone — that would leak the roadmap into the product and
/// become a lie the moment the roster ships.
///
/// They are `Vec<Item>` rather than four new structs for the same reason: an invented
/// `struct Subagent { … }` whose fields nobody fills is a shape guessed a session early, and
/// [`Item`] is what every other pane in this product already lists things with.
#[derive(Debug, Clone, PartialEq)]
pub struct RunView {
    /// Short form, as the window title and every listing print it.
    pub id: String,
    /// The run's mnemonic — `daring-storm` — **derived by the daemon and rendered here**.
    ///
    /// This crate has no dependencies at all and cannot compute it: the derivation folds over a
    /// UUID's bytes and lives beside `RunId`, which is where the identity lives. The same
    /// arrangement as `elapsed_ms` — the thing that holds the value resolves it, and the surface
    /// renders what it is handed rather than growing a second copy of the rule.
    ///
    /// It is a **rendering of the id, never a second identity**: 4096 names collide, so anything
    /// resolving a typed name back to a run refuses an ambiguous one by listing the candidates.
    pub name: String,
    pub state: RunState,
    /// The run that spawned this one, if any. Short form.
    pub parent: Option<String>,
    /// **Resolved by the daemon, which is the thing with a clock.**
    ///
    /// It was `started_ms` + `finished_ms` with the arithmetic done here, which meant the surface
    /// needed a `now_ms` to render a number. `ControlPlane::detail` reports the final wall time
    /// when the run has one and the live figure otherwise, so a frame is a pure function of state
    /// alone — which is a stronger flicker property than the one §6.4 asks for, not a weaker one.
    pub elapsed_ms: u64,
    pub spend_micros_usd: u64,
    /// The run's declared ceiling. Spend is always shown *against* it, because a number with no
    /// denominator is what ADR-028 requirement 2 exists to forbid.
    pub ceiling_micros_usd: u64,
    pub spent_tokens: u64,
    pub granted_tokens: u64,
    pub depth: u8,
    pub checkpoint: CheckpointView,
    pub orphan_policy: OrphanPolicyLabel,
    /// Steers waiting for this run's next iteration boundary.
    ///
    /// **A count, not an acknowledgement** — Session A's rule, and the better one: *"queued" is a
    /// claim about a mechanism; a number is a fact the next `/watch` can be checked against.*
    pub pending_steers: usize,
    /// The run's own output, in the same [`Entry`] vocabulary the conversation pane uses. One
    /// definition of what a transcript is; ADR-055 governs what may be in it.
    pub output: Vec<Entry>,
    pub subagents: Vec<Item>,
    pub budget: Vec<Item>,
    pub scope_memory: Vec<Item>,
    pub meetings: Vec<Item>,
}

impl RunView {
    /// Spend as a fraction of the ceiling, `0.0..=1.0`. `0.0` when no ceiling was declared, which
    /// renders as "no ceiling" rather than as a full bar.
    ///
    /// **`0` as a denominator is the seventeenth instance's question asked of a meter**: a missing
    /// ceiling must read as absent, not as reached.
    pub fn spend_fraction(&self) -> f64 {
        if self.ceiling_micros_usd == 0 {
            return 0.0;
        }
        (self.spend_micros_usd as f64 / self.ceiling_micros_usd as f64).min(1.0)
    }

    /// Whether spend has reached the line. §B2: this is a **state**, so it may carry a state colour.
    pub fn at_ceiling(&self) -> bool {
        self.ceiling_micros_usd > 0 && self.spend_micros_usd >= self.ceiling_micros_usd
    }
}

/// `1_234_567` micros → `$1.23`. Four significant places below a dollar, because a run that has
/// spent a tenth of a cent must not render as `$0.00` beside a ceiling of `$3.00`.
pub fn micros_usd(micros: u64) -> String {
    if micros == 0 {
        return "$0".to_string();
    }
    if micros < 10_000 {
        return format!("${:.4}", micros as f64 / 1_000_000.0);
    }
    format!("${:.2}", micros as f64 / 1_000_000.0)
}

/// `93_061_000` ms → `25h 51m`. Two units, never three, and never a bare seconds count for a run
/// that has been going for hours.
pub fn elapsed(ms: u64) -> String {
    let s = ms / 1_000;
    if s < 60 {
        return format!("{}.{}s", s, (ms % 1_000) / 100);
    }
    let (m, s) = (s / 60, s % 60);
    if m < 60 {
        return format!("{m}m {s:02}s");
    }
    let (h, m) = (m / 60, m % 60);
    format!("{h}h {m:02}m")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tenth_of_a_cent_does_not_render_as_zero_beside_a_three_dollar_ceiling() {
        // The failure this formatter exists for: `$0.00 of $3.00` on a run that has spent
        // something, which reads as "free" and is how a spend ceiling stops being watched.
        assert_eq!(micros_usd(1_000), "$0.0010");
        assert_eq!(micros_usd(0), "$0");
        assert_eq!(micros_usd(1_234_567), "$1.23");
    }

    #[test]
    fn elapsed_never_prints_a_bare_seconds_count_for_a_long_run() {
        assert_eq!(elapsed(2_500), "2.5s");
        assert_eq!(elapsed(90_000), "1m 30s");
        assert_eq!(elapsed(93_061_000), "25h 51m");
    }

    #[test]
    fn no_ceiling_is_not_a_full_bar() {
        // `0` as a denominator: the seventeenth instance's question asked of a meter. A missing
        // ceiling must read as absent, not as reached.
        let mut v = view();
        v.ceiling_micros_usd = 0;
        v.spend_micros_usd = 10_000;
        assert_eq!(v.spend_fraction(), 0.0);
        assert!(!v.at_ceiling());

        // The control, or the assertion above would hold on a struct whose fraction is always 0.
        let mut w = view();
        w.spend_micros_usd = w.ceiling_micros_usd;
        assert_eq!(w.spend_fraction(), 1.0);
        assert!(w.at_ceiling());
    }

    #[test]
    fn the_orphan_policy_is_stated_plainly_and_never_as_its_variant_name() {
        // §6.2. "Detach" is not plain; a confirmation the user did not understand is a
        // confirmation that did not happen.
        for p in [
            OrphanPolicyLabel::Detach,
            OrphanPolicyLabel::Terminate,
            OrphanPolicyLabel::Adopt { by: "a1b2".into() },
        ] {
            let s = p.plainly();
            assert!(s.contains("children"), "{s}");
            assert!(!s.contains("Detach") && !s.contains("Terminate") && !s.contains("Adopt"), "{s}");
        }
    }

    #[test]
    fn every_state_that_has_stopped_reports_itself_as_not_live() {
        for s in [RunState::Completed, RunState::Cancelled, RunState::Failed { error: "e".into() }] {
            assert!(!s.is_live(), "{s:?}");
        }
        for s in [RunState::Queued, RunState::Running, RunState::WaitingEvent] {
            assert!(s.is_live(), "{s:?}");
        }
    }

    fn view() -> RunView {
        RunView {
            id: "a1b2c3d4".into(),
            name: "daring-storm".into(),
            state: RunState::Running,
            parent: None,
            elapsed_ms: 93_000,
            spend_micros_usd: 120_000,
            ceiling_micros_usd: 3_000_000,
            spent_tokens: 400,
            granted_tokens: 50_000,
            depth: 0,
            checkpoint: CheckpointView { last_completed: Some(41), resumable: true },
            orphan_policy: OrphanPolicyLabel::Detach,
            pending_steers: 0,
            output: Vec::new(),
            subagents: Vec::new(),
            budget: Vec::new(),
            scope_memory: Vec::new(),
            meetings: Vec::new(),
        }
    }
}
