//! `agent{type:"fork"}` (A14): the child starts from the parent's history byte for byte.
//!
//! A fork is a subagent that inherits the conversation instead of a clean context. Its first
//! request is the parent's last request unchanged (same system blocks, same tool list, same
//! messages) plus the assistant message that made the fork call, a tool-result message answering
//! every call in it with a placeholder, and the fork's task as the next user message. Providers
//! with a prefix cache (Anthropic breakpoints, Ollama's KV reuse) therefore serve the whole
//! inherited history from cache, which is the point of forking over briefing a fresh agent.
//!
//! To keep the prefix identical:
//! - the parent's model, provider and effort are forced (a `model` on the call is ignored);
//! - the tool list is the parent's exactly. `agent` stays in it so the bytes match; a call to it
//!   (or to any tool a subagent never gets, [`NEVER_FOR_SUBAGENTS`]) is refused before approval by
//!   [`ForkGuard`], so a fork can never start another fork;
//! - nothing is rebuilt from the child's own state: the loop reads [`ForkPrefix`] from the child's
//!   extensions and puts it first (`context::assemble_with`).
//!
//! The loop leaves a [`ForkBase`] in the conversation's extensions for a step whose calls include
//! a fork, and takes it away when the step's calls are done.

use std::sync::Arc;

use marlowe_model::{ChatRequest, Message, SystemBlock, ToolResult, ToolSpec};
use marlowe_proto::SessionId;

use crate::agent::Agent;
use crate::approval::ApprovalGate;
use crate::convo::Conversation;
use crate::middleware::{CallFlow, ToolMiddleware};
use crate::subagent::{ResolvedModel, NEVER_FOR_SUBAGENTS, SUBAGENT_MAX_STEPS};
use crate::system::SystemPrompt;
use crate::tools::{
    CallCtx, Consequence, Exposure, Mutation, SandboxView, Services, ToolCallReq, ToolCtx, ToolHost, ToolManifest, ToolOutcome,
};

/// The agent type name that forks.
pub const FORK_TYPE: &str = "fork";

/// What each tool call of the fork's step answers in the inherited history.
pub const FORK_PLACEHOLDER: &str = "Fork started; processing in the background. Its report will come back to the main agent.";

/// Whether `call` is `agent{type:"fork"}`.
pub fn is_fork_call(call: &ToolCallReq) -> bool {
    call.name == "agent"
        && ["type", "subagent_type"]
            .iter()
            .find_map(|k| call.args.get(*k).and_then(|v| v.as_str()))
            .is_some_and(|t| t.trim().eq_ignore_ascii_case(FORK_TYPE))
}

/// The inherited history: the parent's system blocks and messages as the request carried them.
#[derive(Debug, Clone)]
pub struct ForkPrefix {
    pub system: Vec<SystemBlock>,
    pub messages: Vec<Message>,
}

/// What a fork runs on: the parent's model and tools.
#[derive(Clone)]
pub struct ForkRun {
    pub model: ResolvedModel,
    pub host: Arc<dyn ToolHost>,
    /// The parent's tool list, verbatim.
    pub tools: Vec<ToolSpec>,
    pub max_output: Option<u32>,
}

/// Everything a fork needs of its parent's step; in `Conversation::ext` while the step's tool
/// calls run.
#[derive(Clone)]
pub struct ForkBase {
    pub run: ForkRun,
    pub prefix: ForkPrefix,
    /// Every tool call id of the step's assistant message.
    pub call_ids: Vec<String>,
}

impl std::fmt::Debug for ForkBase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForkBase")
            .field("model", &self.run.model.info.id)
            .field("messages", &self.prefix.messages.len())
            .field("calls", &self.call_ids.len())
            .finish()
    }
}

impl ForkBase {
    /// Captures the parent's request and the assistant message that answered it.
    pub fn capture(req: &ChatRequest, assistant: Message, model: ResolvedModel, host: Arc<dyn ToolHost>) -> Self {
        let call_ids = assistant.tool_calls().map(|c| c.id.clone()).collect();
        let mut messages = req.messages.clone();
        messages.push(assistant);
        Self {
            run: ForkRun { model, host, tools: req.tools.clone(), max_output: req.max_output_tokens },
            prefix: ForkPrefix { system: req.system.clone(), messages },
            call_ids,
        }
    }

    /// The tool-result message that answers the step's calls in the inherited history.
    pub fn placeholders(&self) -> Message {
        Message::tool_results(self.call_ids.iter().map(|id| ToolResult {
            call_id: id.clone(),
            content: FORK_PLACEHOLDER.to_string(),
            is_error: false,
            parts: Vec::new(),
        }))
    }

    /// The conversation a fork starts with: the placeholders as its own first message, the
    /// inherited history in its extensions.
    pub fn conversation(&self, session: SessionId, workspace: Option<std::path::PathBuf>, tainted: bool) -> Conversation {
        let mut convo = Conversation::new(session, workspace);
        convo.exposure.tainted = tainted;
        convo.messages.push(self.placeholders());
        convo.ext.insert(self.prefix.clone());
        convo
    }
}

/// The first user message of a fork.
pub fn directive(task: &str) -> String {
    format!(
        "<fork>\nYou are a fork of the main agent: you were started from this conversation at this point. The tool \
         results just above are placeholders, not real results. You are not the main agent and you cannot start other \
         agents. Do only the task below, with your tools, in your own copy of this context. When you are done, reply \
         with a concise report of what you found or did, with file paths as evidence; that reply is returned to the main \
         agent as it is.\n\nTask: {}\n</fork>",
        task.trim()
    )
}

/// The agent a fork runs as: the parent's model and effort, the parent's tool list, no memory,
/// `services` for its calls and `middleware` (the working set, loop guards) after the guard.
pub fn build_agent(
    run: &ForkRun,
    services: Services,
    approvals: Arc<dyn ApprovalGate>,
    middleware: &[Arc<dyn ToolMiddleware>],
    max_steps: Option<u32>,
) -> Agent {
    let tools: Arc<dyn ToolHost> = Arc::new(ForkTools { inner: run.host.clone(), specs: run.tools.clone() });
    let mut agent = Agent::new(run.model.provider.clone(), run.model.info.clone(), tools, SystemPrompt::with_core(String::new()));
    agent.effort = run.model.effort;
    agent.approvals = approvals;
    agent.services = services;
    agent.max_output = run.max_output;
    agent.budget.max_steps = max_steps.unwrap_or(SUBAGENT_MAX_STEPS);
    agent.middleware.insert(0, Arc::new(ForkGuard));
    agent.middleware.extend(middleware.iter().cloned());
    agent
}

/// Refuses, before approval, the tools a subagent never gets. They stay in the fork's tool list
/// (the list must match the parent's), so the model can still ask for them.
pub struct ForkGuard;

/// What the model is told when it calls a tool a fork does not have.
pub const FORK_REFUSAL: &str =
    "This is a fork: it cannot start agents, ask the user, or move the session. Do the task with the other tools and report back.";

impl ToolMiddleware for ForkGuard {
    fn name(&self) -> &str {
        "fork-guard"
    }

    fn before_approval(&self, call: &mut marlowe_model::ToolCall, _cx: &CallCtx) -> CallFlow {
        if NEVER_FOR_SUBAGENTS.contains(&call.name.as_str()) {
            return CallFlow::Done(ToolOutcome { summary: "not available to a fork".into(), ..ToolOutcome::error(FORK_REFUSAL) });
        }
        CallFlow::Continue
    }
}

/// The parent's host under the parent's tool list.
struct ForkTools {
    inner: Arc<dyn ToolHost>,
    specs: Vec<ToolSpec>,
}

impl ForkTools {
    fn refuses(name: &str) -> bool {
        NEVER_FOR_SUBAGENTS.contains(&name)
    }
}

impl ToolHost for ForkTools {
    fn specs(&self) -> Vec<ToolSpec> {
        self.specs.clone()
    }
    fn call(&self, call: &ToolCallReq, ctx: &ToolCtx) -> ToolOutcome {
        if Self::refuses(&call.name) {
            return ToolOutcome::error(FORK_REFUSAL);
        }
        self.inner.call(call, ctx)
    }
    fn call_scoped(&self, call: &ToolCallReq, cx: &CallCtx) -> ToolOutcome {
        if Self::refuses(&call.name) {
            return ToolOutcome::error(FORK_REFUSAL);
        }
        self.inner.call_scoped(call, cx)
    }
    fn middleware(&self) -> Vec<Arc<dyn ToolMiddleware>> {
        self.inner.middleware()
    }
    fn persona_blocks(&self) -> Vec<crate::system::PersonaBlock> {
        Vec::new()
    }
    fn consequence(&self, call: &ToolCallReq) -> Consequence {
        self.inner.consequence(call)
    }
    fn consequence_exposed(&self, call: &ToolCallReq, exposure: &Exposure) -> Consequence {
        self.inner.consequence_exposed(call, exposure)
    }
    fn taints(&self, call: &ToolCallReq) -> bool {
        self.inner.taints(call)
    }
    fn egress_host(&self, call: &ToolCallReq) -> Option<String> {
        self.inner.egress_host(call)
    }
    fn manifest(&self, name: &str) -> Option<ToolManifest> {
        self.inner.manifest(name)
    }
    fn mutation(&self, call: &ToolCallReq, ctx: &ToolCtx) -> Mutation {
        self.inner.mutation(call, ctx)
    }
    fn consequence_in_sandbox(&self, call: &ToolCallReq, exposure: &Exposure, sandbox: &dyn SandboxView) -> Option<Consequence> {
        self.inner.consequence_in_sandbox(call, exposure, sandbox)
    }
    fn describe(&self, call: &ToolCallReq) -> String {
        self.inner.describe(call)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(args: serde_json::Value) -> ToolCallReq {
        ToolCallReq { id: "c1".into(), name: "agent".into(), args }
    }

    #[test]
    fn a_fork_call_is_recognised_by_its_type() {
        assert!(is_fork_call(&call(json!({"type": "fork", "prompt": "x"}))));
        assert!(is_fork_call(&call(json!({"subagent_type": "Fork"}))));
        assert!(!is_fork_call(&call(json!({"type": "explore"}))));
        assert!(!is_fork_call(&ToolCallReq { id: "c".into(), name: "read".into(), args: json!({"type": "fork"}) }));
    }

    #[test]
    fn the_directive_names_the_task_and_forbids_nesting() {
        let d = directive("  count the TODOs  ");
        assert!(d.contains("Task: count the TODOs"));
        assert!(d.contains("cannot start other agents"));
    }
}
