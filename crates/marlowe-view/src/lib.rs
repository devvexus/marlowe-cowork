//! The view models a producer publishes and a surface renders. **Shapes only.**
//!
//! M2 C2d. This crate exists to make ARCHITECTURE §2.14 — *surfaces hold no state the daemon does
//! not have* — a property of the type system rather than a claim in a module header.
//!
//! # The dependency graph is the argument
//!
//! ```text
//! marlowe-view    ← nothing
//!   ↑        ↑
//!   │        └── marlowe-surface   renders a SessionView, returns Intents
//!   ├─ marlowe-stub                scripted producer  (M1's 91 tests)
//!   └─ marlowe-daemon              real producer      (ARCHITECTURE §6)
//! ```
//!
//! `marlowe-surface` does **not** depend on `marlowe-stub` or `marlowe-daemon` — the stub is a
//! dev-dependency there, so its own `src/` cannot name either producer. That is the C2d acceptance
//! and it is checked by the compiler rather than by review: a promotion that left the surface able
//! to reach a producer would be a rename, and a rename would have left the suite green.
//!
//! # What this crate must never grow
//!
//! A constructor that invents a session, a `Default`, or any function that applies an
//! [`view::Intent`]. Each of the three would hand a surface the ability to author state, which is
//! the exact thing the split exists to prevent.

#![forbid(unsafe_code)]

pub mod approval;
pub mod escalation;
pub mod meter;
pub mod notice;
pub mod model;
pub mod produce;
pub mod provider;
pub mod run;
pub mod turn;
pub mod view;

pub use escalation::{
    AgentOption, ArtifactPath, Choice, EscalationView, HostLabel, IrreversibleAct, SourceEvidence,
    SourceTrace, Survivor, TerminationCost, TooManyOptions, TERMINATE_CANARY, TERMINATE_LABEL,
};
pub use meter::{Frame, MeterSource, BASELINE, LEVELS, SAMPLES};
pub use produce::{ClockRead, Produce};
pub use provider::{HYBRID, OLLAMA, OPENROUTER, PROVIDERS};
pub use approval::{
    BlastRadius, Ceiling, Deviation, Effect, FirstTime, Medium, Novelty, Offered, PathLabel,
    RiskTier,
};
pub use model::{
    Ambient, Cadence, ControlStrip, Entry, Item, Pager, Picker, StatusBand, StatusState, Tab, Tone,
    ToolCall,
};
pub use notice::{CommandLine, Terminal,
    Capability, Disposition, Echo, Listing, Milestone, Notice, PaneSummary, Refusal, RenderContext,
    Speech,
};
pub use run::{
    CheckpointView, OrphanPolicyLabel, RunState, RunView,
};
pub use turn::{DegradedPath, Metric, ResultSummary, ToolLineState, TurnEvent};
pub use view::{
    ClientLine, ControlId, Intent, IntentError, PendingLine, PendingState, SessionView,
};
