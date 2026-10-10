//! The lean agent loop.
//!
//! One turn: assemble context -> stream a model call -> keep its text and run **every** tool
//! call in it (independent calls in parallel, each gated by its consequence) -> append results
//! -> repeat until a message has no tool calls, the step limit, the budget, or cancel.

#![forbid(unsafe_code)]

pub mod agent;
pub mod approval;
pub mod budget;
pub mod checkpoint;
pub mod commands;
pub mod context;
pub mod convo;
mod events;
pub mod hooks;
pub mod instructions;
pub mod memory;
pub mod middleware;
mod run_tools;
pub mod steer;
pub mod subagent;
pub mod system;
pub mod todo;
pub mod tools;
pub mod worktree;

pub use agent::{Agent, MAX_STEPS};
pub use approval::{ApprovalGate, ApprovalReq, AutoApprove, DenyAll};
pub use budget::{Budget, Spent};
pub use context::{Assembled, ContextConfig, COMPACTION_TRIGGER, KEEP_TOOL_RESULTS};
pub use convo::Conversation;
pub use events::{AgentEvent, TurnOutcome};
pub use memory::{MemoryHook, NoMemory, Recollection};
pub use steer::{Steer, SteerRefused, MAX_STEER_CHARS};
pub use system::{Environment, PersonaBlock, SystemPrompt, UserInfo, PERSONA_CORE};
pub use middleware::{
    CallFlow, Extensions, FinalFlow, ModelCall, Notification, NotificationKind, StepInfo, ToolMiddleware, TurnFlow,
    TurnObserver,
};
pub use tools::{
    exposure_profile, select_groups, CallCtx, Consequence, Effects, Exposure, ExposureProfile, Mutation, Profiles,
    SandboxView, Services, SessionScope, ToolCallReq, ToolCtx, ToolGroup, ToolHost, ToolManifest, ToolOutcome,
};

pub use marlowe_proto::{ApprovalDecision, TurnStop};

#[cfg(test)]
mod tests;
