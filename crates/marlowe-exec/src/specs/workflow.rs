//! The team-task group (track W): `team` (run a Starlark team task script: inline, a saved name
//! or a `path`; `resume_from`; `validate_only`; it was `workflow`, which still resolves) and
//! `report_findings` (G13).
//!
//! Which form a call is: `script`, else `path`, else `name` (a saved workflow); keys the chosen
//! form does not use are named in the result, not refused. With a `script` the `name` is its
//! display label.
//!
//! `team` is the conductor's (configured hosts only): the call is checked here, launched by
//! the daemon's [`marlowe_workflow::Workflows`] service as a durable crew run, and returns the run
//! id at once (non-negotiable 4); `background:false` waits for the result, and Esc detaches. The
//! first run of a script digest asks, showing its META; built-ins and approved digests are free.
//! The launch is bound to the digest the consequence saw: a script that changed in between (a
//! write earlier in the same batch) is refused, never run or trusted unseen. A waited-on run's
//! value is agent output, so `background:false` taints the conversation.
//! `structured_output` is not in this table: the crew adds it to a workflow agent with a schema.
//! `report_findings` is offered everywhere, crew agents included.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

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
    /// Calls whose script was sent back once for running its agents one after another.
    warned: Mutex<HashSet<String>>,
    /// The saved team tasks listed in the tool's description, read once so the tool list does not
    /// change while the session lives.
    catalog: OnceLock<String>,
}

impl WorkflowGroup {
    fn new(home: &Path, conductor: bool) -> WorkflowGroup {
        WorkflowGroup { home: Some(home.to_path_buf()), conductor, approved: Mutex::default(), warned: Mutex::default(), catalog: OnceLock::new() }
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
    marlowe_agent::compact_specs::spec("team")
}

/// Most characters of the saved-task list in the `team` description.
const CATALOG_CHARS: usize = 600;

/// The `team` description with this host's saved team tasks (built-in, the user's, the workspace's)
/// under it: the model runs one by name instead of writing a script. Read on first use and kept.
fn with_catalog(t: &BuiltinTools, spec: &mut marlowe_model::ToolSpec) {
    let g = group(t);
    let text = g.catalog.get_or_init(|| {
        let Some(roots) = roots(t, None) else { return String::new() };
        marlowe_workflow::saved::catalog_text(&roots.catalog(), CATALOG_CHARS)
    });
    if !text.is_empty() {
        spec.description.push_str("\nSaved tasks:\n");
        spec.description.push_str(text.trim_end());
    }
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

/// [`plan`], except that a `path` that does not exist beside the name of a saved team task means the
/// saved one (the model wrote `.marl owe/...` for `.marlowe/...`): that runs, and the result says so.
fn resolved_plan(a: &Value, roots: &marlowe_workflow::saved::Roots) -> Result<Plan, String> {
    let mut p = plan(a)?;
    if let (Some(path), Some(n)) = (p.path.clone(), s(a, "name")) {
        if !service::resolve_path(roots, &path).exists() && roots.resolve(n).is_ok() {
            p.path = None;
            p.name = Some(n.to_string());
            p.ignored = vec![format!("`path` ({path} does not exist; ran the saved team task `{n}`)")];
        }
    } else if let (Some(path), None) = (p.path.clone(), s(a, "name")) {
        // A `path` that is not a file but is a saved task's name (`web-research.star`, `web_research`).
        let stem = std::path::Path::new(&path).file_stem().map(|x| x.to_string_lossy().into_owned()).unwrap_or_default();
        if !service::resolve_path(roots, &path).exists() && roots.resolve(&stem).is_ok() {
            p.path = None;
            p.name = Some(stem.clone());
            p.ignored = vec![format!("`path` ({path} does not exist; ran the saved team task `{stem}`)")];
        }
    }
    Ok(p)
}

/// What a `path` that is not a file is told: a saved team task runs by `name`, and which ones there are.
fn missing_path(roots: &marlowe_workflow::saved::Roots, path: &str) -> String {
    let catalog = roots.catalog();
    let mut msg = format!("No file {path}.");
    if catalog.is_empty() {
        return msg;
    }
    let list: Vec<String> = catalog.iter().map(marlowe_workflow::saved::describe).collect();
    msg.push_str(&format!(" A saved team task runs by `name`, not `path`: team{{name: \"{}\", args: {{...}}}}. Saved: {}.", catalog[0].name, list.join("; ")));
    if let Some(fit) = marlowe_workflow::saved::best_match(&catalog, path) {
        msg.push_str(&format!(" Closest to that path: `{}`.", fit.name));
    }
    msg
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
        return Err("Say what to run: `script` (inline Starlark), `path` (a .star file) or `name` (a saved team task).".into());
    }
    Ok(p)
}

/// A call with nothing to run is usually a script the model wrote to a file and then forgot to
/// name: say which `.star` files the workspace holds (two levels down, newest first).
fn star_files_hint(workspace: Option<&std::path::Path>) -> String {
    let Some(root) = workspace else { return String::new() };
    let mut found: Vec<(std::time::SystemTime, String)> = Vec::new();
    let mut dirs = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = dirs.pop() {
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = e.path();
            if path.is_dir() && depth < 2 && !e.file_name().to_string_lossy().starts_with('.') {
                dirs.push((path, depth + 1));
            } else if path.extension().is_some_and(|x| x == "star") {
                let at = e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
                found.push((at, path.strip_prefix(root).unwrap_or(&path).display().to_string()));
            }
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0));
    if found.is_empty() {
        return String::new();
    }
    let names: Vec<String> = found.into_iter().take(5).map(|(_, p)| format!("`{p}`")).collect();
    format!(" This workspace holds {}: pass the one you mean as `path`.", names.join(", "))
}

/// A script with several `agent(` calls and no `parallel(` or `pipeline(` runs its agents one after
/// another, which is rarely what "a team working in parallel" means (dawn wrote six researchers in
/// a row). Sent back once with how to fix it; the same call again runs as written.
fn sequential_agents(source: &str) -> bool {
    source.matches("agent(").count() >= 3 && !source.contains("parallel(") && !source.contains("pipeline(")
}

const SEQUENTIAL_NOTE: &str = "Not run yet (the script is valid): it calls agent() several times but never parallel() or pipeline(), so its agents would run one after another. Agents that do not need each other's answers run together: parallel([\"prompt a\", \"prompt b\"]) takes the prompts as strings (or parallel([lambda: agent(...), ...]) when a call needs options), or pipeline(items, stage) for one agent per item. If one after another is what you want, send the same call again and it runs as written.";

/// Refused scripts in one turn after which the tool tells the model to stop writing them.
const STOP_WRITING_AT: u32 = 3;
/// Refused scripts in one turn after which the turn ends with a message to the user.
const END_TURN_AT: u32 = 5;

/// A call the checker refused. A script that keeps being refused is the model fighting the
/// language, not fixing one mistake: after [`STOP_WRITING_AT`] refusals in a turn the answer names
/// the saved team task that fits best (or lists them) and says to stop, and after [`END_TURN_AT`]
/// the turn ends with a plain message to the user.
fn refuse(cx: &CallCtx, roots: &marlowe_workflow::saved::Roots, plan: &Plan, source: Option<&str>, e: String) -> ToolOutcome {
    if plan.script.is_none() && plan.path.is_none() {
        return ToolOutcome::error_of("team-name", e);
    }
    let n = cx.scope.with_ext(|x| marlowe_agent::agent::guard::count_attempt(x, cx.turn, "team-script"));
    let mut msg = script_error(e);
    if n < STOP_WRITING_AT {
        return ToolOutcome::error_of("script-invalid", msg);
    }
    let catalog = roots.catalog();
    let fit = source.and_then(|s| marlowe_workflow::saved::best_match(&catalog, s));
    let way_out = match (fit, catalog.is_empty()) {
        (Some(c), _) => format!(
            "The closest saved team task is `{}`{}: run it with team{{name: \"{}\", args: {{...}}}}.",
            c.name,
            c.args.as_deref().map(|a| format!(" (args: {a})")).unwrap_or_default(),
            c.name
        ),
        (None, false) => format!("Saved team tasks: {}.", catalog.iter().map(|c| format!("`{}`", c.name)).collect::<Vec<_>>().join(", ")),
        (None, true) => "There is no saved team task to fall back on.".to_string(),
    };
    if n >= END_TURN_AT {
        let why = format!(
            "The team tool refused {n} scripts this turn, so Marlowe stopped here instead of trying more. Ask for a saved team task by name (/teams lists them), or give a simpler plan. {way_out}"
        );
        msg.push_str(&format!("\n\n[harness: {why}]"));
        cx.scope.with_ext(|x| x.insert(marlowe_agent::agent::guard::EndTurn(why)));
    } else {
        msg.push_str(&format!(
            "\n\n[harness: {n} scripts refused this turn. Stop writing scripts. {way_out} Otherwise tell the user plainly what you could not do; do not send another script.]"
        ));
    }
    ToolOutcome::error_of("script-invalid", msg)
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
        return ToolOutcome::error("team tasks are not available on this host");
    };
    let plan = match resolved_plan(a, &roots) {
        Ok(p) => p,
        Err(e) => return ToolOutcome::error(format!("{e}{}", star_files_hint(cx.workspace.as_deref()))),
    };
    if let (None, Some(path)) = (&plan.script, &plan.path) {
        if !service::resolve_path(&roots, path).exists() {
            return ToolOutcome::error_of("team-name", missing_path(&roots, path));
        }
    }
    let from_script = plan.script.is_some() || plan.path.is_some();
    let fail = |e: String| ToolOutcome::error(if from_script { script_error(e) } else { e });
    let source = plan.script.clone().or_else(|| plan.path.as_deref().and_then(|p| std::fs::read_to_string(service::resolve_path(&roots, p)).ok()));
    // Checked first, whatever else the call would be told: a note about a script that cannot run
    // would promise a run that never comes.
    let prepared = match service::prepare(&roots, plan.script.as_deref(), plan.path.as_deref(), plan.name.as_deref()) {
        Ok(p) => p,
        Err(e) => return refuse(cx, &roots, &plan, source.as_deref(), e),
    };
    if a.get("validate_only").and_then(Value::as_bool).unwrap_or(false) {
        return ToolOutcome::ok(format!("{} is valid. META: {}{}", prepared.filename, serde_json::to_string(&prepared.meta).unwrap_or_default(), notes(&plan)), "valid");
    }
    let Some(Workflows(launcher)) = cx.scope.services.get::<Workflows>().cloned() else {
        return ToolOutcome::error("team tasks run in the daemon; this session has no team task launcher");
    };
    if let Some(source) = &source {
        if sequential_agents(source) && group(t).warned.lock().unwrap_or_else(|e| e.into_inner()).insert(call_key(&plan)) {
            return ToolOutcome::error(SEQUENTIAL_NOTE);
        }
    }
    let args = match a.get("args") {
        None => Value::Null,
        // An object sent as a JSON string (a local model's habit) is the object it holds.
        Some(Value::String(s)) if s.trim_start().starts_with('{') => match serde_json::from_str(s) {
            Ok(v) => v,
            Err(e) => return ToolOutcome::error(format!("`args` must be a JSON object; the string given is not valid JSON ({e})")),
        },
        Some(other) => other.clone(),
    };
    if !(args.is_null() || args.is_object()) {
        return ToolOutcome::error("`args` must be a JSON object");
    }
    let background = a.get("background").and_then(Value::as_bool).unwrap_or(true);
    let req = LaunchRequest {
        session: Some(cx.session.clone()),
        workspace: cx.workspace.clone(),
        script: plan.script.clone(),
        script_path: plan.path.clone(),
        name: plan.name.as_ref().map(|_| prepared.name.clone()),
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
                "{receipt}\nTeam task `{name}` is running in the background as crew run {}. Its result arrives as a notice when it finishes; \
                 /teams shows its progress. task_stop / task_output take {task_id}. Carry on with other work.{extra}",
                launched.run_id
            ),
            format!("team task {name} started"),
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
            ToolOutcome::ok(format!("{receipt}\nTeam task `{name}` returned:\n{shown}{extra}"), format!("team task {name} done"))
        }
        Waited::Failed(why) => ToolOutcome::error(format!("{receipt}\nTeam task `{name}` failed: {why}")),
        Waited::Detached => ToolOutcome::ok(
            format!("{receipt}\nStopped waiting; the team task goes on in the background and its result arrives as a notice."),
            "team task detached",
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
        return Consequence::Ask { summary: "run a team task".into() };
    };
    let mut approved = g.approved.lock().unwrap_or_else(|e| e.into_inner());
    // Nothing will run when the call names nothing: it returns the error.
    let Ok(plan) = resolved_plan(a, &roots) else { return Consequence::Free };
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
        (Some(_), _, Some(n)) => format!("team task {n} (inline script)"),
        (Some(_), _, None) => "team task (inline script)".into(),
        (None, Some(p), _) => format!("team task {p}"),
        (None, None, Some(n)) => format!("team task {n}"),
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
            .spec_for(with_catalog)
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

/// No block: the `team` description carries the script API and the rules for using it.
pub fn persona_block() -> Option<&'static str> {
    None
}

/// The conductor's hosts: `team` is live and findings are stored under the profile home.
pub fn configure(tools: crate::BuiltinTools, setup: &super::HostSetup<'_>) -> crate::BuiltinTools {
    tools.with_state(WorkflowGroup::new(setup.home, true))
}

/// A crew host: `report_findings` stores under the profile home; no `team` (agents never
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
