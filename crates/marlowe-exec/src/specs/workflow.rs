//! The workflow group (track W): `workflow` (run a Starlark workflow script: inline, a saved name
//! or a `path`; `resume_from`; `validate_only`) and `report_findings` (G13).
//!
//! Which form a call is: `script`, else `path`, else `name` (a saved workflow); keys the chosen
//! form does not use are named in the result, not refused. With a `script` the `name` is its
//! display label.
//!
//! `workflow` is the conductor's (configured hosts only): the call is checked here, launched by
//! the daemon's [`marlowe_workflow::Workflows`] service as a durable crew run, and returns the run
//! id at once (non-negotiable 4); `background:false` waits for the result, and Esc detaches. The
//! first run of a script digest asks, showing its META; built-ins and approved digests are free.
//! The launch is bound to the digest the consequence saw: a script that changed in between (a
//! write earlier in the same batch) is refused, never run or trusted unseen. A waited-on run's
//! value is agent output, so `background:false` taints the conversation.
//! `structured_output` is not in this table: the crew adds it to a workflow agent with a schema.
//! `report_findings` is offered everywhere, crew agents included.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use marlowe_agent::{CallCtx, Consequence, Effects, ToolCallReq, ToolGroup, ToolOutcome};
use marlowe_workflow::service::{self, LaunchRequest, Waited};
use marlowe_workflow::Workflows;
use serde_json::{json, Value};

use super::ToolDef;
use crate::BuiltinTools;

/// The group's state on a host: the profile home, and whether the host is the conductor's.
#[derive(Debug, Default)]
pub struct WorkflowGroup {
    pub home: Option<PathBuf>,
    pub conductor: bool,
    /// The digest each pending call's consequence checked, by [`call_key`]: the launch runs that
    /// script or nothing.
    approved: Mutex<HashMap<String, String>>,
}

impl WorkflowGroup {
    fn new(home: &Path, conductor: bool) -> WorkflowGroup {
        WorkflowGroup { home: Some(home.to_path_buf()), conductor, approved: Mutex::default() }
    }
}

/// What a call names, as the key the consequence and the call share.
fn call_key(p: &Plan) -> String {
    json!([p.script, p.path, p.name]).to_string()
}

fn script_error(e: impl std::fmt::Display) -> String {
    e.to_string().trim_end().to_string()
}

/// Longest result a waited-on workflow puts in the tool result (the journal has it whole).
const RESULT_CHARS: usize = 20_000;

fn group(t: &BuiltinTools) -> std::sync::Arc<WorkflowGroup> {
    t.state::<WorkflowGroup>()
}

fn roots(t: &BuiltinTools, workspace: Option<&std::path::Path>) -> Option<marlowe_workflow::saved::Roots> {
    let g = group(t);
    let home = g.home.as_deref()?;
    let workspace = workspace.or(t.workspace());
    Some(service::roots(home, workspace, marlowe_agent::commands::plugin::bundle_roots(Some(home), workspace)))
}

fn workflow_spec() -> marlowe_model::ToolSpec {
    marlowe_agent::compact_specs::spec("workflow")
}

fn report_findings_spec() -> marlowe_model::ToolSpec {
    marlowe_agent::compact_specs::spec("report_findings")
}

fn s<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty())
}

/// The `.star` file a call names (`path`; `script_path` is the older spelling).
fn path<'a>(a: &'a Value) -> Option<&'a str> {
    s(a, "path").or_else(|| s(a, "script_path"))
}

/// The inline script exactly as given (its digest is the trust key, so it is not trimmed).
fn script<'a>(a: &'a Value) -> Option<&'a str> {
    a.get("script").and_then(Value::as_str).filter(|v| !v.trim().is_empty())
}

/// The form a call is, and what it leaves unused.
#[derive(Debug, Default)]
struct Plan {
    script: Option<String>,
    path: Option<String>,
    /// What `service::prepare` resolves: a saved name.
    name: Option<String>,
    /// The run's display name when the model gave one that is not what runs.
    label: Option<String>,
    ignored: Vec<String>,
}

fn present(a: &Value, k: &str) -> bool {
    a.get(k).is_some_and(|v| !v.is_null())
}

fn plan(a: &Value) -> Result<Plan, String> {
    let (script, path, name) = (script(a), path(a), s(a, "name"));
    let mut p = Plan::default();
    let ignore = |p: &mut Plan, keys: &[&str], why: &str| {
        for k in keys.iter().filter(|k| present(a, k) || (**k == "path" && present(a, "script_path"))) {
            p.ignored.push(format!("`{k}` ({why})"));
        }
    };
    if let Some(sc) = script {
        p.script = Some(sc.to_string());
        p.label = name.map(str::to_string);
        ignore(&mut p, &["path"], "a `script` was given");
    } else if let Some(pa) = path {
        p.path = Some(pa.to_string());
        ignore(&mut p, &["name"], "a `path` was given");
    } else if let Some(n) = name {
        p.name = Some(n.to_string());
    } else {
        return Err("Say what to run: `script` (inline Starlark), `path` (a .star file) or `name` (a saved workflow).".into());
    }
    Ok(p)
}

/// The ignored keys, as the line after a result.
fn notes(p: &Plan) -> String {
    if p.ignored.is_empty() {
        return String::new();
    }
    format!("
Ignored: {}.", p.ignored.join("; "))
}

fn workflow_call(t: &BuiltinTools, a: &Value, cx: &CallCtx) -> ToolOutcome {
    let Some(roots) = roots(t, cx.workspace.as_deref()) else {
        return ToolOutcome::error("workflows are not available on this host");
    };
    let plan = match plan(a) {
        Ok(p) => p,
        Err(e) => return ToolOutcome::error(e),
    };
    let from_script = plan.script.is_some() || plan.path.is_some();
    let fail = |e: String| ToolOutcome::error(if from_script { script_error(e) } else { e });
    if a.get("validate_only").and_then(Value::as_bool).unwrap_or(false) {
        return match service::prepare(&roots, plan.script.as_deref(), plan.path.as_deref(), plan.name.as_deref()) {
            Ok(p) => ToolOutcome::ok(format!("{} is valid. META: {}{}", p.filename, serde_json::to_string(&p.meta).unwrap_or_default(), notes(&plan)), "valid"),
            Err(e) => fail(e),
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
        script: plan.script.clone(),
        script_path: plan.path.clone(),
        name: plan.name.clone(),
        args,
        background,
        resume_from: s(a, "resume_from").map(str::to_string),
        tainted: cx.tainted,
        approved_digest: group(t).approved.lock().unwrap_or_else(|e| e.into_inner()).remove(&call_key(&plan)),
    };
    let launched = match launcher.launch(req) {
        Ok(l) => l,
        Err(e) => return fail(e),
    };
    let task_id = format!("wf_{}", launched.run_id);
    let mut receipt = json!({"task_id": task_id, "run_id": launched.run_id, "script_path": launched.script_path, "name": launched.name});
    if let Some(l) = &plan.label {
        receipt["label"] = json!(l);
    }
    let name = plan.label.clone().unwrap_or_else(|| launched.name.clone());
    let extra = notes(&plan);
    if background {
        return ToolOutcome::ok(
            format!(
                "{receipt}\nWorkflow `{name}` is running in the background as crew run {}. Its result arrives as a notice when it finishes; \
                 /workflows shows its progress. task_stop / task_output take {task_id}. Carry on with other work.{extra}",
                launched.run_id
            ),
            format!("workflow {name} started"),
        );
    }
    match launcher.wait(&launched.run_id, &cx.cancel) {
        Waited::Done { value, tainted } => {
            if tainted {
                cx.scope.with_ext(|e| e.insert(marlowe_agent::subagent::ReportTainted));
            }
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
            ToolOutcome::ok(format!("{receipt}\nWorkflow `{name}` returned:\n{shown}{extra}"), format!("workflow {name} done"))
        }
        Waited::Failed(why) => ToolOutcome::error(format!("{receipt}\nWorkflow `{name}` failed: {why}")),
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
    let mut approved = g.approved.lock().unwrap_or_else(|e| e.into_inner());
    // Nothing will run when the call names nothing: it returns the error.
    let Ok(plan) = plan(a) else { return Consequence::Free };
    let key = call_key(&plan);
    match service::prepare(&roots, plan.script.as_deref(), plan.path.as_deref(), plan.name.as_deref()) {
        // Nothing will run: the call returns the errors. A script that appears by the time the
        // call runs (a write in the same batch) has no approved digest, so the launch refuses it.
        Err(_) => {
            approved.remove(&key);
            Consequence::Free
        }
        Ok(p) => {
            approved.insert(key, p.digest.clone());
            // Built-ins and approved digests are free.
            if service::is_trusted(home, &p) {
                Consequence::Free
            } else {
                Consequence::Ask { summary: service::approval_summary(&p) }
            }
        }
    }
}

fn describe_workflow(c: &ToolCallReq) -> String {
    let a = &c.args;
    match (script(a), path(a), s(a, "name")) {
        (Some(_), _, Some(n)) => format!("workflow {n} (inline script)"),
        (Some(_), _, None) => "workflow (inline script)".into(),
        (None, Some(p), _) => format!("workflow {p}"),
        (None, None, Some(n)) => format!("workflow {n}"),
        _ => "workflow".into(),
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
            // A waited-on run puts agent output (web pages, repository text) into the result.
            .taints(|_, c| c.args.get("background").and_then(Value::as_bool) == Some(false))
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

/// Loop middleware for this group (see `specs/mod.rs`): the `.star` validator.
pub fn middleware(tools: &crate::BuiltinTools) -> Vec<std::sync::Arc<dyn marlowe_agent::ToolMiddleware>> {
    let _ = tools;
    vec![std::sync::Arc::new(star::StarValidator::default())]
}

/// No block: the `workflow` description carries the script API and the rules for using it.
pub fn persona_block() -> Option<&'static str> {
    None
}

/// The conductor's hosts: `workflow` is live and findings are stored under the profile home.
pub fn configure(tools: crate::BuiltinTools, setup: &super::HostSetup<'_>) -> crate::BuiltinTools {
    tools.with_state(WorkflowGroup::new(setup.home, true))
}

/// A crew host: `report_findings` stores under the profile home; no `workflow` (agents never
/// start workflows, WF2).
pub fn crew_configure(tools: crate::BuiltinTools, setup: &super::HostSetup<'_>) -> crate::BuiltinTools {
    tools.with_state(WorkflowGroup::new(setup.home, false))
}

/// Whether a local model gets this group in `root`: every workspace.
pub fn detect(_root: Option<&std::path::Path>) -> bool {
    true
}

mod star;

#[cfg(test)]
mod tests;
