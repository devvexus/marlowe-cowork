//! The lean agent loop.
//!
//! One turn: assemble context -> stream a model call -> keep its text and run **every** tool
//! call in it (independent calls in parallel, each gated by its consequence) -> append results
//! -> repeat until a message has no tool calls, the step limit, the budget, or cancel.

#![forbid(unsafe_code)]

mod agent;
mod approval;
pub mod budget;
pub mod context;
mod convo;
mod events;
mod memory;
mod run_tools;
pub mod steer;
mod system;
mod tools;

pub use agent::{Agent, MAX_STEPS};
pub use approval::{ApprovalGate, ApprovalReq, AutoApprove, DenyAll};
pub use budget::{Budget, Spent};
pub use context::{Assembled, ContextConfig, COMPACTION_TRIGGER, KEEP_TOOL_RESULTS};
pub use convo::Conversation;
pub use events::{AgentEvent, TurnOutcome};
pub use memory::{MemoryHook, NoMemory};
pub use steer::{Steer, SteerRefused, MAX_STEER_CHARS};
pub use system::{Environment, PersonaBlock, SystemPrompt, UserInfo, PERSONA_CORE};
pub use tools::{Consequence, Exposure, ToolCallReq, ToolCtx, ToolHost, ToolOutcome};

pub use marlowe_proto::{ApprovalDecision, TurnStop};

#[cfg(test)]
mod tests;
