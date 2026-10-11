//! The workflow group (track W): `workflow` (run a Starlark workflow script: inline, a saved name
//! or a `script_path`; `resume_from`; `validate_only`) and `report_findings` (G13).
//!
//! `workflow` is the conductor's (configured hosts only): the call is checked here, launched by
//! the daemon's [`marlowe_workflow::Workflows`] service as a durable crew run, and returns the run
//! id at once (non-negotiable 4); `background:false` waits for the result, and Esc detaches. The
//! first run of a script digest asks, showing its META; built-ins and approved digests are free.
//! `structured_output` is not in this table: the crew adds it to a workflow agent with a schema.
//! `report_findings` is offered everywhere, crew agents included.

use std::path::PathBuf;

use marlowe_agent::{CallCtx, Consequence, Effects, ToolCallReq, ToolGroup, ToolOutcome};
use marlowe_workflow::service::{self, LaunchRequest, Waited};
use marlowe_workflow::Workflows;
use serde_json::{json, Value};

use super::{spec, ToolDef};
use crate::BuiltinTools;

/// The group's state on a host: the profile home, and whether the host is the conductor's.
#[derive(Debug, Default)]
pub struct WorkflowGroup {
    pub home: Option<PathBuf>,
    pub conductor: bool,
}

/// Longest result a waited-on workflow puts in the tool result (the journal has it whole).
const RESULT_CHARS: usize = 20_000;

fn group(t: &BuiltinTools) -> std::sync::Arc<WorkflowGroup> {
    t.state::<WorkflowGroup>()
}

fn roots(t: &BuiltinTools, workspace: Option<&std::path::Path>) -> Option<marlowe_workflow::saved::Roots> {
    let g = group(t);
    let home = g.home.as_deref()?;
    Some(service::roots(home, workspace.or(t.workspace()), Vec::new()))
}

/// The tool's description carries the script API, so a deferred schema loaded through
/// `tool_search` teaches the model enough to write a script (WF9).
const WORKFLOW_DESCRIPTION: &str = "Run a workflow: a Starlark script (Python-like) that orchestrates many agents, in the background as a crew run. \
Returns {run_id, script_path} at once; the result arrives as a notice (or set background:false to wait). \
Give exactly one of: script (inline), script_path (a .star file), name (saved: .marlowe/workflows, <home>/workflows, built-ins code-review and simplify). \
Script shape: META = {\"name\": ..., \"description\": ..., \"phases\": [...]} (a pure literal), then def main(args): ... return result. \
API: agent(prompt, label=, phase=, schema=, model=, effort=, isolation=\"worktree\", agent_type=) -> text, dict (with schema) or None; \
parallel([lambda: agent(...), ...]) runs them together (barrier); pipeline(items, lambda prev, item, i: ..., ...) moves each item through stages with no barrier; \
phase(name); log(...); workflow(name, args) (one level); args; budget.total, budget.spent(), budget.remaining(); json.encode/decode. \
No while, no recursion, no clock, randomness or files: agents do the work. In loops bind the variable: lambda f=f: agent(f). \
The script is checked before it runs; errors come back to you. validate_only:true checks without running.";

fn workflow_spec() -> marlowe_model::ToolSpec {
    spec(
        "workflow",
        WORKFLOW_DESCRIPTION,
        json!({
            "script": {"type": "string", "description": "An inline Starlark workflow script."},
            "script_path": {"type": "string", "description": "A .star file (relative to the workspace or absolute)."},
            "name": {"type": "string", "description": "A saved workflow's name."},
            "args": {"type": "object", "description": "JSON the script gets as `args` (verbatim)."},
            "background": {"type": "boolean", "description": "Default true: return the run id at once. false: wait for the result."},
            "resume_from": {"type": "string", "description": "A previous run's id: unchanged calls reuse its results (the first changed call and everything after it run live)."},
            "validate_only": {"type": "boolean", "description": "Check the script and report its META or its errors; run nothing."}
        }),
        &[],
    )
}

fn report_findings_spec() -> marlowe_model::ToolSpec {
    spec(
        "report_findings",
        "Report review findings so they show in the findings panel: each with lens, severity (info, low, medium, high, critical), file, \
         line, claim (one sentence), evidence and fix. Invalid entries are refused with what to fix; send the whole list again.",
        json!({"findings": {"type": "array", "items": marlowe_workflow::findings::finding_schema()}}),
        &["findings"],
    )
}

fn s<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty())
}

/// The inline script exactly as given (its digest is the trust key, so it is not trimmed).
fn script<'a>(a: &'a Value) -> Option<&'a str> {
    a.get("script").and_then(Value::as_str).filter(|v| !v.trim().is_empty())
}

fn workflow_call(t: &BuiltinTools, a: &Value, cx: &CallCtx) -> ToolOutcome {
    let Some(roots) = roots(t, cx.workspace.as_deref()) else {
        return ToolOutcome::error("workflows are not available on this host");
    };
    let (script, script_path, name) = (script(a), s(a, "script_path"), s(a, "name"));
    if a.get("validate_only").and_then(Value::as_bool).unwrap_or(false) {
        return match service::prepare(&roots, script, script_path, name) {
            Ok(p) => ToolOutcome::ok(format!("{} is valid. META: {}", p.filename, serde_json::to_string(&p.meta).unwrap_or_default()), "valid"),
            Err(e) => ToolOutcome::error(e),
        };
    }
    let Some(Workflows(launcher)) = cx.scope.services.get::<Workflows>().cloned() else {
        return ToolOutcome::error("workflows run in the daemon; this session has no workflow launcher");
    };
    let args = a.get("args").cloned().unwrap_or(Value::Null);
    if !(args.is_null() || args.is_object()) {
        return ToolOutcome::error("`args` must be a JSON object");
    }
    let background = a.get("background").and_then(Value::as_bool).unwrap_or(true);
    let req = LaunchRequest {
        session: Some(cx.session.clone()),
        workspace: cx.workspace.clone(),
        script: script.map(str::to_string),
        script_path: script_path.map(str::to_string),
        name: name.map(str::to_string),
        args,
        background,
        resume_from: s(a, "resume_from").map(str::to_string),
        tainted: cx.tainted,
    };
    let launched = match launcher.launch(req) {
        Ok(l) => l,
        Err(e) => return ToolOutcome::error(e),
    };
    let receipt = json!({"run_id": launched.run_id, "script_path": launched.script_path, "name": launched.name});
    if background {
        return ToolOutcome::ok(
            format!(
                "{receipt}\nWorkflow `{}` is running in the background as crew run {}. Its result arrives as a notice when it finishes; \
                 /workflows shows its progress. Carry on with other work.",
                launched.name, launched.run_id
            ),
            format!("workflow {} started", launched.name),
        );
    }
    match launcher.wait(&launched.run_id, &cx.cancel) {
        Waited::Done { value, .. } => {
            let text = serde_json::to_string_pretty(&value).unwrap_or_default();
            let shown = if text.len() > RESULT_CHARS {
                let mut end = RESULT_CHARS;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}\n… (cut; the whole value is in the run's journal)", &text[..end])
            } else {
                text
            };
            ToolOutcome::ok(format!("{receipt}\nWorkflow `{}` returned:\n{shown}", launched.name), format!("workflow {} done", launched.name))
        }
        Waited::Failed(why) => ToolOutcome::error(format!("{receipt}\nWorkflow `{}` failed: {why}", launched.name)),
        Waited::Detached => ToolOutcome::ok(
            format!("{receipt}\nStopped waiting; the workflow goes on in the background and its result arrives as a notice."),
            "workflow detached",
        ),
    }
}

fn workflow_consequence(t: &BuiltinTools, c: &ToolCallReq) -> Consequence {
    let a = &c.args;
    if a.get("validate_only").and_then(Value::as_bool).unwrap_or(false) {
        return Consequence::Free;
    }
    let g = group(t);
    let (Some(home), Some(roots)) = (g.home.as_deref(), roots(t, None)) else {
        return Consequence::Ask { summary: "run a workflow".into() };
    };
    match service::prepare(&roots, script(a), s(a, "script_path"), s(a, "name")) {
        // Nothing will run: the call returns the errors.
        Err(_) => Consequence::Free,
        Ok(p) if service::is_trusted(home, &p) => Consequence::Free,
        Ok(p) => Consequence::Ask { summary: service::approval_summary(&p) },
    }
}

fn describe_workflow(c: &ToolCallReq) -> String {
    match (s(&c.args, "name"), s(&c.args, "script_path")) {
        (Some(n), _) => format!("workflow {n}"),
        (None, Some(p)) => format!("workflow {p}"),
        _ => "workflow (inline script)".into(),
    }
}

/// The store key for findings reported in `session`: a crew run's go under the run.
pub fn findings_key(session: &str) -> String {
    match session.strip_prefix("crew:") {
        Some(run) => format!("run-{run}"),
        None => session.to_string(),
    }
}

fn report_findings_call(t: &BuiltinTools, a: &Value, cx: &CallCtx) -> ToolOutcome {
    let list = a.get("findings").cloned().unwrap_or(Value::Null);
    let findings = match marlowe_workflow::findings::parse_findings(&list) {
        Ok(f) => f,
        Err(e) => return ToolOutcome::error(format!("Nothing was reported; fix these and send the whole list again: {e}")),
    };
    if let Some(home) = group(t).home.as_deref() {
        if let Err(e) = marlowe_workflow::findings::append(home, &findings_key(&cx.session.0), &findings) {
            return ToolOutcome::error(format!("could not store the findings: {e}"));
        }
    }
    if let Some(sink) = cx.scope.services.get::<service::FindingsSink>() {
        (sink.0)(&cx.session, &findings);
    }
    let n = findings.len();
    ToolOutcome::ok(format!("Reported {n} finding(s); they are in the findings panel."), format!("{n} finding(s)"))
}

pub fn tools() -> Vec<ToolDef> {
    vec![
        ToolDef::new(workflow_spec(), ToolGroup::Workflow, Effects::RUN, workflow_call)
            .consequence(|t, c, _| workflow_consequence(t, c))
            .describe(describe_workflow)
            .enabled(|t| group(t).conductor),
        ToolDef::new(report_findings_spec(), ToolGroup::Workflow, Effects::NONE, report_findings_call)
            .describe(|c| {
                let n = c.args.get("findings").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
                format!("report_findings {n}")
            })
            // Configured hosts (the conductor's and the crew's) only: findings are stored under
            // the profile home.
            .enabled(|t| group(t).home.is_some()),
    ]
}

/// Loop middleware for this group (see `specs/mod.rs`): none.
pub fn middleware(tools: &crate::BuiltinTools) -> Vec<std::sync::Arc<dyn marlowe_agent::ToolMiddleware>> {
    super::no_middleware(tools)
}

/// `persona/blocks/workflow.md`: appended only while the group is exposed and live (WF9).
pub fn persona_block() -> Option<&'static str> {
    Some(include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../persona/blocks/workflow.md")))
}

/// The conductor's hosts: `workflow` is live and findings are stored under the profile home.
pub fn configure(tools: crate::BuiltinTools, setup: &super::HostSetup<'_>) -> crate::BuiltinTools {
    tools.with_state(WorkflowGroup { home: Some(setup.home.to_path_buf()), conductor: true })
}

/// A crew host: `report_findings` stores under the profile home; no `workflow` (agents never
/// start workflows, WF2).
pub fn crew_configure(tools: crate::BuiltinTools, setup: &super::HostSetup<'_>) -> crate::BuiltinTools {
    tools.with_state(WorkflowGroup { home: Some(setup.home.to_path_buf()), conductor: false })
}

/// Whether a local model gets this group in `root`: every workspace (on a 32k context the tool is
/// deferred behind `tool_search`, T31).
pub fn detect(_root: Option<&std::path::Path>) -> bool {
    true
}

#[cfg(test)]
mod tests;
