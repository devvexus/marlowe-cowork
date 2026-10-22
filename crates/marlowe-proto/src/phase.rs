//! Live phase events: every named step between Enter and the first token, and the work that
//! trails a turn. One family so a client renders any of them the same way.
//!
//! Additive on the wire: every new field defaults, enums end in `Unknown`, so an old reader skips
//! what it does not know. `Start` and `End` are ordered and replayable; `Progress` is ephemeral
//! (seq 0, never in the replay ring).

use serde::{Deserialize, Serialize};

use crate::crew::RunId;
use crate::ids::SessionId;

/// What a phase is. Short ones (`Sending` .. `Fit`, `Wire`) flash on the activity line; `Recall`,
/// `Load`, `Compact` and `MemoryWrite` get detail rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseKind {
    /// Enter pressed, request not yet acknowledged.
    Sending,
    /// Session setup on a first turn.
    Setup,
    /// Admission against the GPU (waits say why in `detail`).
    Admit,
    /// Prompt build.
    Prompt,
    /// Memory recall; `stage` names the sub-stage.
    Recall,
    /// Context assemble and fit.
    Fit,
    /// Context compaction.
    Compact,
    /// Request on the wire to first frame.
    Wire,
    /// Cold model load.
    Load,
    /// Prompt prefill (reading).
    Prefill,
    /// The turn's memory ingest and embed tail.
    MemoryWrite,
    /// Daemon shutdown; `stage` is pause / save / unload / journal.
    Shutdown,
    /// A research channel coming online; `stage` is its name.
    Channel,
    /// A crew run spinning up; `stage` is sandbox / model / tools / budget.
    Spinup,
    /// Work the daemon does on its own, outside any turn (a memory embed, a voice model, an MCP
    /// server starting, a brief polish): `stage` names it, `detail` says what. Never filed under a session.
    Work,
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseState {
    #[default]
    Start,
    Progress,
    End,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// One step starting, advancing or ending.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PhaseEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
    /// `next_turn` at emit time. May arrive before `TurnStarted`; clients key by (session, turn).
    #[serde(default)]
    pub turn: u32,
    pub kind: PhaseKind,
    pub state: PhaseState,
    /// Sub-stage: `embed`, `bm25`, `dense`, `gate`, `rerank`, `assemble`, `model`, a shutdown
    /// step, a channel name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    /// `items`, `pairs`, `MiB`, `tok`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// Observer-computed `done / elapsed`, per second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate: Option<f32>,
    /// `End` only: daemon-measured monotonic duration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms: Option<u64>,
    /// An estimate: the client renders it with `~` until a real number replaces it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub est: bool,
    /// `cpu`, `gpu`, `429 retrying`, `crew run b8ee`, `13.1 GB freed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl PhaseEvent {
    pub fn new(kind: PhaseKind, state: PhaseState) -> Self {
        PhaseEvent { kind, state, ..Default::default() }
    }

    pub fn start(kind: PhaseKind) -> Self {
        Self::new(kind, PhaseState::Start)
    }

    pub fn end(kind: PhaseKind, ms: u64) -> Self {
        PhaseEvent { ms: Some(ms), ..Self::new(kind, PhaseState::End) }
    }

    pub fn progress(kind: PhaseKind, done: u64, total: u64) -> Self {
        PhaseEvent { done: Some(done), total: Some(total), ..Self::new(kind, PhaseState::Progress) }
    }

    pub fn on(mut self, session: &SessionId, turn: u32) -> Self {
        self.session = Some(session.clone());
        self.turn = turn;
        self
    }

    pub fn stage(mut self, stage: impl Into<String>) -> Self {
        self.stage = Some(stage.into());
        self
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn unit(mut self, unit: impl Into<String>) -> Self {
        self.unit = Some(unit.into());
        self
    }

    pub fn with_done(mut self, done: u64) -> Self {
        self.done = Some(done);
        self
    }

    pub fn with_total(mut self, total: u64) -> Self {
        self.total = Some(total);
        self
    }
}

/// One recall stage's measured time, after the fact (`Recalled.stages`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageMs {
    pub name: String,
    pub ms: u64,
    /// Items the stage handled (pool size, slate size), when it has a count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<u64>,
}

/// One measured span of a turn (`TurnTimings`, persisted `StoredTurn.phases`). Only real,
/// measured spans; never an estimate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhaseSpan {
    pub kind: PhaseKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    /// Offset from the turn's start (the daemon receiving the Ask), in ms.
    #[serde(default)]
    pub start_ms: u64,
    pub ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The span this one nests inside (a recall stage inside `Recall`): excluded when spans are
    /// summed against the time to first token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<PhaseKind>,
    /// Always false for a persisted span; kept so a client can use one renderer for live and stored.
    #[serde(default, skip_serializing_if = "is_false")]
    pub est: bool,
}

/// What a model call read: new tokens, tokens served from the cache, and how long it took.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadStat {
    pub new_tok: u64,
    #[serde(default)]
    pub cached_tok: u64,
    pub ms: u64,
}

/// The measured totals of one turn, beside its spans (`TurnTimings`, persisted with the turn).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnNumbers {
    /// Enter (the client's stamp, else the daemon receiving the Ask) to the first token on the bus.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    /// Harness overhead before the request: Enter to request on the wire, excluding recall,
    /// admission waits and compaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enter_to_wire_ms: Option<u64>,
    /// Harness overhead after: the provider's first token reaching the bus.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_to_bus_ms: Option<u64>,
    /// What the turn's first model call read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read: Option<ReadStat>,
}
