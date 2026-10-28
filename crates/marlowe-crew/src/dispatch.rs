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
use crate::CrewError;

/// The tool's name.
pub const DISPATCH_TOOL: &str = "dispatch";

/// Built-in roles that run the stages of a playbook (and a review panel); a direct dispatch is
/// possible but rare, so the `role` menu lists them by name only.
const PLAYBOOK_ROLES: [&str; 8] =
    ["scout", "compressor", "verifier", "lead", "integrator", "review-correctness", "review-security", "review-performance"];

/// Roles whose work is a project's now: with projects live they are not offered to `dispatch`
/// and a call naming one is told the way to a project (`scout`, `compressor` and `verifier` only
/// ever served the retired research playbook).
pub const RESEARCH_ROLES: [&str; 4] = ["researcher", "scout", "compressor", "verifier"];

/// One-phrase menu entries for the built-in roles one picks directly.
fn blurb(role: &str) -> Option<&'static str> {
    Some(match role {
        "researcher" => "literature and evidence",
        "engineer" => "builds in a worktree",
        "reviewer" => "tries to break things",
        "analyst" => "data, logs, experiments",
        _ => return None,
    })
}

/// A role's description cut at its first clause (at most 56 characters, on a word), so a long
/// menu does not eat the prompt: the full cards stay in the role files.
fn short(description: &str) -> String {
    let first = description.split(['.', ';', ':']).next().unwrap_or(description).trim();
    if first.chars().count() <= 56 {
        return first.to_string();
    }
    let cut: String = first.chars().take(56).collect();
    match cut.rfind(' ') {
        Some(i) => cut[..i].trim_end_matches(',').to_string(),
        None => cut,
    }
}

/// The tool declaration. `roles` fills the `role` enum and its description.
pub fn dispatch_spec(roles: &[RoleInfo]) -> ToolSpec {
    dispatch_spec_with(roles, false)
}

/// [`dispatch_spec`], with the research roles left out when `projects` are live.
pub fn dispatch_spec_with(roles: &[RoleInfo], projects: bool) -> ToolSpec {
    // The workflow role runs only a workflow script's agents (the `workflow` tool starts those).
    let roles: Vec<&RoleInfo> = roles
        .iter()
        .filter(|r| r.name != crate::playbook::script::WORKFLOW_ROLE)
        .filter(|r| !(projects && RESEARCH_ROLES.contains(&r.name.as_str())))
        .collect();
    let names: Vec<&str> = roles.iter().map(|r| r.name.as_str()).collect();
    // The playbooks' own stages are named by the enum alone; the menu gives the roles to pick.
    let menu: Vec<String> = roles
        .iter()
        .filter(|r| !PLAYBOOK_ROLES.contains(&r.name.as_str()))
        .map(|r| format!("{}: {}", r.name, blurb(&r.name).map_or_else(|| short(&r.description), str::to_string)))
        .collect();
    let mut spec = marlowe_agent::compact_specs::spec(DISPATCH_TOOL);
    if let Some(role) = spec.input_schema.pointer_mut("/properties/role") {
        role["enum"] = json!(names);
        if !menu.is_empty() {
            role["description"] = json!(menu.join(" · "));
        }
    }
    spec
}

/// Parsed `dispatch` arguments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DispatchArgs {
    pub role: String,
    /// The task, with the context the crew lacks (`prompt` in the tool's schema).
    #[serde(alias = "prompt")]
    pub goal: String,
    #[serde(default)]
    pub playbook: Option<Playbook>,
    /// Where the run works (`cwd` in the tool's schema).
    #[serde(default, alias = "cwd")]
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
        // The retired research playbook, under any spelling `Playbook` takes: one sentence to the project.
        if let Some(Ok(retired @ Playbook::DeepResearch)) = args.get("playbook").and_then(Value::as_str).map(str::parse::<Playbook>) {
            return Err(CrewError::Retired(retired).to_string());
        }
        let a: DispatchArgs = serde_json::from_value(args.clone()).map_err(|e| format!("bad dispatch arguments: {e}"))?;
        if a.role.trim().is_empty() {
            return Err("`role` is required".into());
        }
        if a.goal.trim().is_empty() {
            return Err("`prompt` is required: the task, with the context, paths and done-criteria".into());
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
            Some(Playbook::Build) => 30,
            Some(Playbook::DeepResearch) | None => 8,
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
    /// The id `task_list`, `task_stop` and `task_output` use: `agent_<run>` (`wf_<run>` for a workflow).
    #[serde(default)]
    pub task_id: String,
    pub run_id: RunId,
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playbook: Option<Playbook>,
    pub estimate: Estimate,
}

/// The conductor's dispatch host, composed with its other tools when a supervisor is running.
pub struct DispatchTools {
    supervisor: Supervisor,
    /// Projects are live: research is theirs, so the research roles are neither offered nor run.
    projects: bool,
}

impl DispatchTools {
    pub fn new(supervisor: Supervisor) -> DispatchTools {
        DispatchTools { supervisor, projects: false }
    }

    /// Research runs as a project: `dispatch` stops offering the research roles and refuses them.
    pub fn with_projects(mut self, live: bool) -> DispatchTools {
        self.projects = live;
        self
    }

    /// Workspace for a call: the argument, else the session's.
    fn workspace(args: &DispatchArgs, ctx: &ToolCtx) -> Option<PathBuf> {
        args.workspace.as_ref().map(PathBuf::from).or_else(|| ctx.workspace.clone())
    }
}

impl ToolHost for DispatchTools {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![dispatch_spec_with(&self.supervisor.roles(), self.projects)]
    }

    /// Queues the run under `ctx.session` and returns the receipt; never waits on the run.
    fn call(&self, call: &ToolCallReq, ctx: &ToolCtx) -> ToolOutcome {
        let mut args = match DispatchArgs::parse(&call.args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::error(e),
        };
        if self.projects && RESEARCH_ROLES.contains(&args.role.trim().to_ascii_lowercase().as_str()) {
            return ToolOutcome::error(CrewError::ResearchRole(args.role.trim().to_string()).to_string());
        }
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
        let goal = crate::run::clip(call.args.get("prompt").or_else(|| call.args.get("goal")).and_then(Value::as_str).unwrap_or(""), 120);
        Consequence::Ask { summary: format!("dispatch {role} after untrusted content: {goal}") }
    }

    fn describe(&self, call: &ToolCallReq) -> String {
        let role = call.args.get("role").and_then(Value::as_str).unwrap_or("crew");
        let goal: String = call.args.get("prompt").or_else(|| call.args.get("goal")).and_then(Value::as_str).unwrap_or("").chars().take(60).collect();
        format!("dispatch {role} · {goal}")
    }
}
