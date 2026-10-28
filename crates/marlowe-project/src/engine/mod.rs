//! The phase engine: runs a project's phases on worker threads with the conversation never
//! waiting for them.
//!
//! | Module | What it is |
//! |---|---|
//! | [`runner`] | One employee: a fresh context, no tools, the reply forced into a form by the engine's grammar, validated again in code, retried by the table in design 5.5. |
//! | [`gate`] | Where an employee gets a slot ([`ModelGate`]); [`SlotPool`] is a self-contained pool. |
//! | [`functions`] | The [`Toolbox`]: the named prompt builders, form rules, code steps and gate cards a phase declaration points at. |
//! | [`builtin`] | The generic functions every kind shares (plan card and approval, checkin card and answer, continuation rounds). |
//! | [`record`] | The journal record of a finished task and the keys that name tasks. |
//! | [`core`] | The scheduler: [`Engine`], lanes, pipelining, delta merges, gates, budgets, cancel, resume. |
//! | [`fence`] | Text from an earlier step enters a later prompt as fenced data. |
//! | [`machine`] | The machine's load (cpu, gpu, vram) the engine writes into `metrics` while it runs. |
//! | [`route`] | [`ByModel`]: one provider in front of the several a project's models need. |
//!
//! Phases are data ([`crate::spec`]); the engine knows four step primitives and nothing about
//! sources, claims or experiments.

pub mod builtin;
mod core;
mod exec;
pub mod fence;
pub mod functions;
pub mod gate;
mod index;
pub mod machine;
pub mod record;
pub mod route;
pub mod runner;

pub use self::core::{answer_gate, Budgets, Control, Engine, EngineConfig, Lanes, RunHandle, RunOutcome};
pub use self::functions::{default_prompt, narrow_schema, CodeFn, Effect, Finished, FinishFn, GateCard, GateCtx, Prompt, PromptFn, TaskInput, Toolbox};
pub use self::gate::{GateError, ModelGate, Permit, SlotPool, Unlimited};
pub use self::machine::{Machine, MachineProbe};
pub use self::route::ByModel;
pub use self::record::{Outcome, Record};
pub use self::runner::{run_employee, Failure, RuleFailure, RunnerConfig, Success, ThinkSupport};
