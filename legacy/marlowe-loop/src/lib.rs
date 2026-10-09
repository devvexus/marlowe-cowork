//! The one loop, its runs, and the context assembler.
//!
//! ARCHITECTURE §2.8: *"One loop. Research, voice, coding, automation, consolidation, and
//! quarantined reading are **capability profiles** — differing in tool exposure, budgets, and
//! interrupt policy — not variants."*
//!
//! HP10 makes that executable: `tests/hp10_budgets.rs` fails the build if a second driving loop
//! appears in this crate. If you find yourself needing one, the architecture is wrong and the
//! fix is a `CapabilityProfile`, not a `while`.
//!
//! # What this crate does **not** do yet, stated so it is not assumed
//!
//! | Missing | Session | Consequence today |
//! |---|---|---|
//! | Path scoping | M2 B | `read`/`edit`/`grep`/`bash` are blocked at adjudication — see `marlowe_permission::scope` |
//! | Tool executors, skills, MCP | M2 C | [`driver::ToolHost`] has no production implementation |
//! | Memory | M2 D | [`driver::MemoryHost`] is `None`; `remember` reports that it is not wired |
//! | A provider client | M2 C | [`driver::ModelDriver`] has no production implementation |
//! | ~~Durable runs~~ | ~~M3~~ | **Shipped, M3 Session A** — [`control::DurableControl`]; the ephemeral control still refuses by name and that test still runs |
//!
//! Every one of those is a **named refusal** rather than a permissive default. That is the
//! standing rule in `CLAUDE.md` — prefer a load-time error to a sensible default — and it is
//! why the absences above are visible in a run's output instead of being invisible successes.

#![forbid(unsafe_code)]

pub mod budget;
pub mod context;
pub mod control;
pub mod driver;
pub mod durable;
pub mod engine;
pub mod escalation;
pub mod profile;
pub mod provenance;
pub mod record;
pub mod run;
pub mod steer;
pub mod turn;
pub mod upward;

pub use budget::{Budget, BudgetShare, CallLimits, Dimension, MIN_CALL_TOKENS, MIN_CHILD_TOKENS};
pub use context::{
    estimate_tokens, Assembler, Block, ContextView, GovernanceConstraint, PrefixCache,
    SessionState, SourceBudgets, SourceKind, Tier as ContextTier, COMPACTION_TRIGGER, MEMORY_TOKEN_BUDGET, WireTurn, WireToolCall};
pub use control::{DurableControl, EphemeralControl, ResumeError, RunControl};
pub use durable::{
    settle_orphan, settle_orphan_in, Checkpoint, CheckpointStore, JournalCheckpoints, MemoryCheckpoints,
    OrphanOutcome, Restored, CHECKPOINT_VERSION,
};
pub use driver::{BatchItem, EscalationPort, EscalationRequest, ToolInvocation,
    ApprovalGate, ClaimRequest, ClockSource, Control, ExternalContent, MemoryHost, ModelCall, ModelDriver,
    ModelStep, NoControl, ProviderError, SpawnRequest, SteerMessage, SteppingClock, Summarizer,
    ToolBody, ToolHost, ToolOutcome, TurnSink, Urgency, Usage,
};
pub use engine::{
    coerce_to_declared_types, Engine, LoopOutcome, Ports, QuarantineRefusal,
    ASK_IS_THE_CONVERSATIONS_DOOR, CONTRACT_UNMET,
    KEEP_TOOL_RESULTS, MAX_SOURCES_PER_READER, MAX_STEPS, UNDESCRIBED_SOURCE,
};
pub use profile::{AgentLevel, CapabilityProfile, Disposition, InterruptPolicy, LevelRefusal, ModelRoute, ProfileError, UnrunnableTools, verify_every_exposed_tool_is_runnable};
pub use escalation::{
    escalation_route, EscalationRefused, EscalationRoute, NotRaisableReason, ESCALATION_NO_DESK,
    ESCALATION_RAISED,
};
pub use provenance::Provenance;
pub use steer::{MAX_STEER_CHARS, SteerOrigin, SteerRefused, admit as admit_steer};
pub use record::{JournalRecorder, MemoryRecorder, Recorder};
pub use run::{
    is_renderable, CondensedResult, ContractViolation, FieldSpec, FieldType, OrphanPolicy,
    OutputContract, PauseReason, Run, RunId, RunStatus, SessionId, DEFAULT_RESULT_MAX_CHARS,
};
pub use turn::{DegradedPath, ToolLineState, TurnEvent};
pub use upward::{UnknownShape, UpwardShape, HEADLINE_FIELD, HEADLINE_MAX_CHARS};
