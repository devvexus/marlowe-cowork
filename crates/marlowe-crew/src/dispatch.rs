//! The conductor's `dispatch` tool. Conductor only: crew tool hosts never carry it (depth 1).
//! It returns a receipt at once; the run's result reaches the parent session's inbox later.

use std::path::PathBuf;

use marlowe_agent::{Consequence, Exposure, ToolCallReq, ToolCtx, ToolHost, ToolOutcome};
use marlowe_model::{Pricing, ToolSpec};
use marlowe_proto::{CrewBudget, Playbook, RoleInfo, RunId};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::role::RoleCard;
use crate::supervisor::Supervisor;

/// The tool's name.
pub const DISPATCH_TOOL: &str = "dispatch";

/// The tool declaration. `roles` fills the `role` enum and its description.
pub fn dispatch_spec(roles: &[RoleInfo]) -> ToolSpec {
    let names: Vec<&str> = roles.iter().map(|r| r.name.as_str()).collect();
    let menu: Vec<String> = roles.iter().map(|r| format!("{}: {}", r.name, r.description)).collect();
    ToolSpec {
        name: DISPATCH_TOOL.to_string(),
        description: "Send work to a crew agent that runs in the background in its own sandbox, and \
                      get its run id back immediately. Use it for long, parallel or bulky work: a \
                      literature sweep, an implementation in a git worktree, a long investigation. \
                      The result arrives later in this conversation. The crew cannot see this \
                      conversation, so the goal must carry the context it needs."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "role": {
                    "type": "string",
                    "enum": names,
                    "description": format!("Who does the work. {}", menu.join(" · ")),
                },
                "goal": {
                    "type": "string",
                    "description": "What to find out or build, with the facts, paths, constraints and \
                                    definition of done the crew needs.",
                },
                "playbook": {
                    "type": "string",
                    "enum": ["deep_research", "build"],
                    "description": "deep_research: plan, parallel scouts, adversarial verify, cited \
                                    report. build: plan, implement in a worktree, test loop, review, \
                                    diff for merge. Omit for a single role-prompt run (investigate, \
                                    review, experiment).",
                },
                "workspace": {
                    "type": "string",
                    "description": "Directory the run works on; default is this session's workspace.",
                },
                "budget": {
                    "type": "object",
                    "properties": {
                        "tokens": {"type": "integer"},
                        "steps": {"type": "integer"},
                        "usd": {"type": "number"}
                    },
                    "additionalProperties": false,
                    "description": "Overrides the role's budget; exceeding it pauses the run for the user.",
                }
            },
            "required": ["role", "goal"],
            "additionalProperties": false,
        }),
    }
}

/// Parsed `dispatch` arguments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DispatchArgs {
    pub role: String,
    pub goal: String,
    #[serde(default)]
    pub playbook: Option<Playbook>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub budget: Option<CrewBudget>,
    /// The dispatching conversation had read untrusted content. Never taken from the model's
    /// arguments; the run starts tainted, so its shells ask from the first step.
    #[serde(skip)]
    pub tainted: bool,
}

impl DispatchArgs {
    /// Parses the model's arguments; the error says what is wrong in one line.
    pub fn parse(args: &Value) -> Result<DispatchArgs, String> {
        let a: DispatchArgs = serde_json::from_value(args.clone()).map_err(|e| format!("bad dispatch arguments: {e}"))?;
        if a.role.trim().is_empty() {
            return Err("`role` is required".into());
        }
        if a.goal.trim().is_empty() {
            return Err("`goal` is required".into());
        }
        Ok(a)
    }
}

/// A rough cost and time estimate, shown to the conductor and the user at dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Estimate {
    pub minutes: u32,
    /// `None` for free (local) models or unknown pricing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usd: Option<f64>,
}

impl Estimate {
    /// From the role, playbook and budget, priced with the resolved model's `pricing`.
    /// Rough by design: a typical run uses about a third of its token budget, four parts input
    /// to one part output, capped at the usd budget.
    pub fn for_run(role: &RoleCard, playbook: Option<Playbook>, budget: &CrewBudget, pricing: Option<&Pricing>) -> Estimate {
        let budget = role.budget.overlay(*budget);
        let minutes = match playbook {
            Some(Playbook::DeepResearch) => 20,
            Some(Playbook::Build) => 30,
            None => 8,
        };
        let usd = pricing.zip(budget.tokens).map(|(p, tokens)| {
            let used = tokens / 3;
            let cost = p.cost(used * 4 / 5, used / 5);
            let cost = budget.usd.map_or(cost, |cap| cost.min(cap));
            (cost * 100.0).round() / 100.0
        });
        Estimate { minutes, usd: usd.filter(|u| *u > 0.0) }
    }
}

/// What `dispatch` returns, as the tool result's JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DispatchReceipt {
    pub run_id: RunId,
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playbook: Option<Playbook>,
    pub estimate: Estimate,
}

/// The conductor's dispatch host, composed with its other tools when a supervisor is running.
pub struct DispatchTools {
    supervisor: Supervisor,
}

impl DispatchTools {
    pub fn new(supervisor: Supervisor) -> DispatchTools {
        DispatchTools { supervisor }
    }

    /// Workspace for a call: the argument, else the session's.
    fn workspace(args: &DispatchArgs, ctx: &ToolCtx) -> Option<PathBuf> {
        args.workspace.as_ref().map(PathBuf::from).or_else(|| ctx.workspace.clone())
    }
}

impl ToolHost for DispatchTools {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![dispatch_spec(&self.supervisor.roles())]
    }

    /// Queues the run under `ctx.session` and returns the receipt; never waits on the run.
    fn call(&self, call: &ToolCallReq, ctx: &ToolCtx) -> ToolOutcome {
        let mut args = match DispatchArgs::parse(&call.args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::error(e),
        };
        args.workspace = Self::workspace(&args, ctx).map(|p| p.display().to_string());
        args.tainted = ctx.tainted;
        match self.supervisor.dispatch(Some(ctx.session.clone()), args) {
            Ok(receipt) => {
                let summary = format!("{} run {}", receipt.role, receipt.run_id.short());
                ToolOutcome::ok(serde_json::to_string(&receipt).unwrap_or_default(), summary)
            }
            Err(e) => ToolOutcome::error(format!("dispatch failed: {e}")),
        }
    }

    /// Dispatching is free; the run's own side effects carry their approvals.
    fn consequence(&self, _call: &ToolCallReq) -> Consequence {
        Consequence::Free
    }

    /// After untrusted content the goal may carry an injection, so dispatching asks.
    fn consequence_exposed(&self, call: &ToolCallReq, exposure: &Exposure) -> Consequence {
        if !exposure.tainted {
            return self.consequence(call);
        }
        let role = call.args.get("role").and_then(Value::as_str).unwrap_or("crew");
        let goal = crate::run::clip(call.args.get("goal").and_then(Value::as_str).unwrap_or(""), 120);
        Consequence::Ask { summary: format!("dispatch {role} after untrusted content: {goal}") }
    }

    fn describe(&self, call: &ToolCallReq) -> String {
        let role = call.args.get("role").and_then(Value::as_str).unwrap_or("crew");
        let goal: String = call.args.get("goal").and_then(Value::as_str).unwrap_or("").chars().take(60).collect();
        format!("dispatch {role} · {goal}")
    }
}
