//! Projects: the core of everything.
//!
//! A project is a durable, staged piece of work that a small model can start and a crowd of
//! single-task models can finish (design: `docs/research/projects-design.md`). This crate holds
//! the parts that need no model and no agent loop:
//!
//! | Module | What it is |
//! |---|---|
//! | [`plan`] | The plan the manager writes: validated for a 9b model (three errors at most, each with its fix), expanded by code, revised, continued. |
//! | [`form`] | A form is the JSON Schema an employee is forced into **and** how each field is drawn: who writes it, its type, its display hint. [`form::model_schema`] and [`form::data_schema`] come from it. |
//! | [`registry`] | Every form and project kind as JSON under `registry/`; a new kind or phase type is data. |
//! | [`spec`] | Phase declarations as data: the four step primitives (fan-out, code, merge, gate), their edges and lanes, and the menu check. |
//! | [`state`] | The one JSON document that holds a project: phases, items, gates, agents, grants, live numbers. |
//! | [`mutation`] | The only way the state changes; each change bumps `rev` and returns JSON-pointer patches. A `done` item whose data breaks its form is refused. |
//! | [`replica`] | A client's copy that follows the patch stream and detects a revision gap. |
//! | [`view`] | The view-model: **one renderer over any project's JSON** (badge, link, table, bar, meter, duration, agent chips), a plain-text painter, and the overview rollups. |
//! | [`store`] | Files under the Marlowe home (atomic `project.json`, journal, content-addressed sources, archive, reopen, disposal) and the `projects` index in `cowork.db`. |
//! | [`actor`] | The single writer for a running project: applies mutations, coalesces events, saves. |
//!
//! Two rules shape everything here. The screen is derived from the JSON: nothing in [`view`]
//! knows what a source or an experiment is, so a kind nobody wrote screen code for still draws.
//! And the model writes only what a form marks `M`: ids, urls, sources, scores and live numbers
//! are written by code, so [`form::model_schema`] never offers the model a place to put them.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod actor;
pub mod form;
pub mod ids;
pub mod mutation;
pub mod plan;
pub mod registry;
pub mod replica;
pub mod spec;
pub mod state;
pub mod store;
pub mod view;

pub use actor::{ActorConfig, ActorError, ProjectActor, ProjectEvent, ProjectHandle};
pub use form::{data_schema, model_schema, Field, FieldSource, FieldType, Form, Show, ShowAs, Tone, Who};
pub use mutation::{Applied, ItemUpdate, LiveUpdate, Mutation, MutationError, PhaseUpdate};
pub use plan::{continue_plan, parse_plan, revise_plan, Continuation, Limits, Plan, PlanErrors, PlanField, Question};
pub use registry::{KindTemplate, Registry, Rollup, PHASE_MENU};
pub use replica::{apply_patches, Replica, ReplicaError};
pub use state::{
    Agent, AgentStatus, Gate, GateAnswer, GateKind, GateStatus, Item, ItemStatus, Live, Phase, ProjectState, ProjectStatus, StageStatus,
    STATE_SCHEMA,
};
pub use store::{IndexRow, ProjectStore, Snapshot, StoreError};
pub use view::{overview, paint, paint_overview, view, view_tree, Cell, Overview, Row};
