//! The verify group (track D): `project`, `service`, `http_check` and `browser`, backed by
//! `marlowe-verify`. This file is the permission and registration face of the tools; what they do
//! lives in that crate, which plans each call (the command lines, hosts and credentials it would
//! use) the same way the call later runs it, so the approval judges what executes.
//!
//! | Call | Default | In a crew sandbox |
//! |---|---|---|
//! | `project` profile | free | free |
//! | `project` run / gate / migrate | asks, naming the command lines | free after the sandbox policy classifies each line, unless tainted; an install or a named database asks |
//! | `service` start / restart | asks, naming the command and any credential variables | as above; credentials and inline shell code (`bash -c`) always ask |
//! | `service` status / logs / stop | free | free |
//! | `http_check`, `browser` on this machine | free (a state-changing request asks unless it goes to a service started here) | same |
//! | `http_check`, `browser` on another host | asks for a new host (a `browser` call names every host its `goto` steps reach, and always shows its `eval` text), and taints the session | same |
//! | `browser` steps that act (`click`, `fill`, `eval`, `goto`...) on a local port no service here started | asks | same |
//!
//! Every child starts through `marlowe_os::child` (scrubbed environment, Job Object, per-launch
//! `env_allow`); services and the browser die with the host.

use std::path::Path;
use std::sync::Arc;

use marlowe_agent::{CallCtx, Consequence, Effects, Exposure, Mutation, SandboxView, ToolCallReq, ToolCtx, ToolGroup, ToolOutcome};
use marlowe_model::{CancelToken, ResultPart, ToolSpec};
use marlowe_verify::browser::Step;
use marlowe_verify::tools::{self as vt, Out, ProjectOp, ServiceOp};
use marlowe_verify::{Env, VerifyState};
use serde_json::Value;

use super::{clip, ToolDef};
use crate::BuiltinTools;

pub fn tools() -> Vec<ToolDef> {
    vec![
        ToolDef::new(project_spec(), ToolGroup::Verify, Effects::RUN, |t, a, c| run(t, c, |env, st| vt::project(env, st, a)))
            .path_args(&["cwd"])
            .consequence(project_consequence)
            .crew_consequence(project_crew)
            .describe(project_describe)
            .egress(project_egress)
            .mutation(project_mutation),
        ToolDef::new(service_spec(), ToolGroup::Verify, Effects::RUN, |t, a, c| run(t, c, |env, st| vt::service(env, st, a)))
            .path_args(&["cwd"])
            .consequence(service_consequence)
            .crew_consequence(service_crew)
            .describe(service_describe)
            .mutation(service_mutation),
        ToolDef::new(http_spec(), ToolGroup::Verify, Effects::NETWORK, |t, a, c| run(t, c, |env, _| vt::http_check(env, a)))
            .consequence(http_consequence)
            .describe(http_describe)
            .taints(http_taints)
            .egress(http_egress),
        ToolDef::new(browser_spec(), ToolGroup::Verify, Effects::NETWORK, |t, a, c| run(t, c, |env, st| vt::browser(env, st, a)))
            .consequence(browser_consequence)
            .describe(browser_describe)
            .taints(browser_taints)
            .egress(browser_egress)
            .enabled(|t| t.state::<VerifyState>().browser().is_some_and(|b| b.available())),
    ]
}

// ---- running -----------------------------------------------------------------------------------

/// The host's roots, scrub list and sandbox pin as the harness's [`Env`], for one call.
fn with_env<R>(tools: &BuiltinTools, workspace: Option<&Path>, cancel: &CancelToken, f: impl FnOnce(&Env<'_>, &VerifyState) -> R) -> Option<R> {
    let root = tools.root.as_deref().or(workspace)?;
    let state = tools.state::<VerifyState>();
    let env = Env {
        ws: tools.workspace_port(),
        root,
        pin: tools.shell_dir.as_deref(),
        scrub: &tools.scrub,
        child_env: &tools.child_env,
        cancel,
    };
    Some(f(&env, &state))
}

fn run(tools: &BuiltinTools, cx: &CallCtx, f: impl FnOnce(&Env<'_>, &VerifyState) -> Out) -> ToolOutcome {
    match with_env(tools, cx.workspace.as_deref(), &cx.cancel, f) {
        Some(out) => outcome(out),
        None => ToolOutcome::error("no workspace is open; these tools run against the project in the workspace"),
    }
}

fn outcome(out: Out) -> ToolOutcome {
    let mut o = if out.is_error { ToolOutcome::error(out.text) } else { ToolOutcome::ok(out.text, out.summary.clone()) };
    if out.is_error {
        o.summary = out.summary;
    }
    if let Some(img) = out.image {
        o = o.with_part(ResultPart::Image(img));
    }
    o
}

/// Planning needs the same view of the host as running, without a call context.
fn plan_with<R>(tools: &BuiltinTools, f: impl FnOnce(&Env<'_>, &VerifyState) -> R) -> Option<R> {
    let cancel = CancelToken::new();
    with_env(tools, None, &cancel, f)
}

// ---- project -----------------------------------------------------------------------------------

fn op_name(plan: &vt::ProjectPlan) -> &'static str {
    match plan.op {
        ProjectOp::Profile => "profile",
        ProjectOp::Run { .. } => "run",
        ProjectOp::Gate { .. } => "gate",
        ProjectOp::Migrate { .. } => "migrate",
    }
}

/// What a call that could not be planned is recorded as: if the files change so that it can be
/// planned by the time it runs, it was never approved and refuses to run.
const UNPLANNABLE: &str = "unplannable";

/// The plan of a project call that runs something, recorded as the one this call was judged
/// with ([`VerifyState::judge`]); `None` for a profile read or a call that cannot run.
fn judged_project(env: &Env<'_>, st: &VerifyState, call: &ToolCallReq) -> Option<vt::ProjectPlan> {
    let key = vt::judge_key("project", &call.args);
    match vt::plan_project(env, st, &call.args) {
        Ok(plan) if matches!(plan.op, ProjectOp::Profile) => None,
        Ok(plan) => {
            st.judge(key, plan.fingerprint(env));
            Some(plan)
        }
        Err(_) => {
            st.judge(key, UNPLANNABLE.into());
            None
        }
    }
}

/// [`judged_project`] for a service start or restart.
fn judged_service(env: &Env<'_>, st: &VerifyState, call: &ToolCallReq) -> Option<vt::ServicePlan> {
    let key = vt::judge_key("service", &call.args);
    match vt::plan_service(env, st, &call.args) {
        Ok(plan) if matches!(plan.op, ServiceOp::Start | ServiceOp::Restart) => {
            st.judge(key, plan.fingerprint());
            Some(plan)
        }
        Ok(_) => None,
        Err(_) => {
            st.judge(key, UNPLANNABLE.into());
            None
        }
    }
}

fn project_consequence(tools: &BuiltinTools, call: &ToolCallReq, _exposure: &Exposure) -> Consequence {
    let ask = |s: String| Consequence::Ask { summary: s };
    plan_with(tools, |env, st| match judged_project(env, st, call) {
        // A profile read, or a call that cannot be planned (it fails on its own and runs nothing).
        None => Consequence::Free,
        Some(plan) => {
            if let Some(db) = plan.user_db() {
                return ask(format!("project migrate against {db} (a database other than the throwaway one)"));
            }
            let hosts = plan.hosts();
            let reach = if !hosts.is_empty() {
                format!(" (reaches {})", hosts.join(", "))
            } else if plan.install_destination_unknown() {
                " (reaches a network host that is not known)".to_string()
            } else {
                String::new()
            };
            let over = if plan.profile.commands.iter().any(|c| c.source == marlowe_verify::profile::Source::Override) { " [commands from .marlowe/project.toml]" } else { "" };
            ask(format!("project {}: {}{}{reach}{over}", op_name(&plan), clip(&plan.command_lines().join("; "), 160), script_note(&plan.scripts())))
        }
    })
    .unwrap_or_else(|| ask("project: no workspace".into()))
}

fn project_crew(tools: &BuiltinTools, call: &ToolCallReq, exposure: &Exposure, sandbox: &dyn SandboxView) -> Consequence {
    let plain = || project_consequence(tools, call, exposure);
    plan_with(tools, |env, st| match judged_project(env, st, call) {
        None => Consequence::Free,
        Some(plan) => {
            // An install reaches a registry: never free where its destination is the repo's own word
            // (an override) or unknown, since the crew's shell policy does not see network use.
            if plan.user_db().is_some()
                || plan.install_destination_unknown()
                || plan.hosts().iter().any(|h| !exposure.approved_hosts.contains(h))
                || exposure.tainted
            {
                return plain();
            }
            // The script a command names is what runs; the sandbox policy reads it too.
            for line in plan.command_lines().into_iter().chain(plan.runs()) {
                if let Consequence::Ask { summary } = sandbox.classify_shell(&line, false) {
                    return Consequence::Ask { summary: format!("project {}: {summary}", op_name(&plan)) };
                }
            }
            Consequence::Free
        }
    })
    .unwrap_or_else(plain)
}

/// The scripts a command runs, for an approval: the one the command names as its body, the others
/// (`pretest`, `postinstall`, what a script calls) with their names, each cut at 100 characters.
fn script_note(scripts: &[marlowe_verify::profile::Script]) -> String {
    if scripts.is_empty() {
        return String::new();
    }
    let shown: Vec<String> = scripts.iter().map(|s| if s.name.is_empty() { clip(&s.body, 100) } else { format!("{}: {}", s.name, clip(&s.body, 100)) }).collect();
    format!(" [script: {}]", shown.join("; "))
}

fn project_describe(call: &ToolCallReq) -> String {
    match vt::parse_project(&call.args) {
        Ok(ProjectOp::Profile) | Err(_) => "project profile".into(),
        Ok(ProjectOp::Run { kind, extra, .. }) => {
            let tail = if extra.is_empty() { String::new() } else { format!(" {}", clip(&extra.join(" "), 40)) };
            format!("project {}{tail}", kind.name())
        }
        Ok(ProjectOp::Gate { .. }) => "project gate".into(),
        Ok(ProjectOp::Migrate { .. }) => "project migrate".into(),
    }
}

fn project_egress(tools: &BuiltinTools, call: &ToolCallReq) -> Option<String> {
    plan_with(tools, |env, st| vt::plan_project(env, st, &call.args).ok().and_then(|p| p.hosts().into_iter().next())).flatten()
}

fn project_mutation(tools: &BuiltinTools, call: &ToolCallReq, ctx: &ToolCtx) -> Mutation {
    with_env(tools, ctx.workspace.as_deref(), &ctx.cancel, |env, st| match vt::plan_project(env, st, &call.args) {
        Ok(plan) if !matches!(plan.op, ProjectOp::Profile) => match plan.work_dirs(env).as_slice() {
            [one] => Mutation::Under(one.clone()),
            _ => Mutation::Under(env.root.to_path_buf()),
        },
        _ => Mutation::None,
    })
    .unwrap_or(Mutation::None)
}

// ---- service -----------------------------------------------------------------------------------

fn service_consequence(tools: &BuiltinTools, call: &ToolCallReq, _exposure: &Exposure) -> Consequence {
    plan_with(tools, |env, st| match judged_service(env, st, call) {
        None => Consequence::Free,
        Some(plan) => {
            let creds = if plan.credentials.is_empty() { String::new() } else { format!(" [passes credential variables: {}]", plan.credentials.join(", ")) };
            let inline = if plan.indirect() { " [runs inline shell code]" } else { "" };
            let script = script_note(&plan.runs);
            let verb = if plan.op == ServiceOp::Start { "start" } else { "restart" };
            Consequence::Ask { summary: format!("service {verb}: {}{script}{creds}{inline}", clip(&plan.display().unwrap_or_default(), 160)) }
        }
    })
    .unwrap_or(Consequence::Ask { summary: "service: no workspace".into() })
}

fn service_crew(tools: &BuiltinTools, call: &ToolCallReq, exposure: &Exposure, sandbox: &dyn SandboxView) -> Consequence {
    let plain = || service_consequence(tools, call, exposure);
    plan_with(tools, |env, st| match judged_service(env, st, call) {
        None => Consequence::Free,
        Some(plan) => {
            // A credential or inline shell code is the user's call even inside a sandbox.
            if !plan.credentials.is_empty() || plan.indirect() || exposure.tainted {
                return plain();
            }
            for line in plan.display().into_iter().chain(plan.runs.iter().map(|s| s.body.clone())) {
                if let Consequence::Ask { summary } = sandbox.classify_shell(&line, false) {
                    return Consequence::Ask { summary: format!("service start: {summary}") };
                }
            }
            Consequence::Free
        }
    })
    .unwrap_or_else(plain)
}

fn service_describe(call: &ToolCallReq) -> String {
    let op = call.args.get("op").and_then(Value::as_str).unwrap_or("status");
    match call.args.get("name").and_then(Value::as_str) {
        Some(n) => format!("service {op} {}", clip(n, 40)),
        None => format!("service {op}"),
    }
}

fn service_mutation(tools: &BuiltinTools, call: &ToolCallReq, ctx: &ToolCtx) -> Mutation {
    with_env(tools, ctx.workspace.as_deref(), &ctx.cancel, |env, st| match vt::plan_service(env, st, &call.args) {
        Ok(plan) => match plan.spec {
            Some(s) if matches!(plan.op, ServiceOp::Start | ServiceOp::Restart) => Mutation::Under(s.cwd),
            _ => Mutation::None,
        },
        Err(_) => Mutation::None,
    })
    .unwrap_or(Mutation::None)
}

// ---- http_check and browser --------------------------------------------------------------------

/// The remote host a call reaches, if it is not this machine.
fn http_host(call: &ToolCallReq) -> Option<String> {
    let plan = vt::plan_http(&call.args).ok()?;
    (!plan.request.url.is_loopback()).then(|| plan.request.url.host.clone())
}

fn browser_plan(tools: &BuiltinTools, call: &ToolCallReq) -> Option<vt::BrowserPlan> {
    plan_with(tools, |env, _| vt::plan_browser(env, &call.args).ok()).flatten()
}

fn new_host(name: &str, host: Option<String>, exposure: &Exposure) -> Consequence {
    match host.filter(|h| !exposure.approved_hosts.contains(h)) {
        Some(h) => Consequence::Ask { summary: format!("{name} {h}: a new host") },
        None => Consequence::Free,
    }
}

fn http_consequence(tools: &BuiltinTools, call: &ToolCallReq, exposure: &Exposure) -> Consequence {
    let Ok(plan) = vt::plan_http(&call.args) else { return Consequence::Free };
    let url = &plan.request.url;
    if !url.is_loopback() {
        return new_host("http_check", Some(url.host.clone()), exposure);
    }
    // Reading anything on this machine is free; a request that changes state is free only to a
    // service this session started, since another local port may be a database or someone's server.
    let reads = matches!(plan.request.method.as_str(), "GET" | "HEAD" | "OPTIONS");
    if reads || tools.state::<VerifyState>().services.owns_port(url.port) {
        Consequence::Free
    } else {
        Consequence::Ask { summary: format!("http_check {} {}: a local service this session did not start", plan.request.method, url.authority()) }
    }
}

fn http_taints(_tools: &BuiltinTools, call: &ToolCallReq) -> bool {
    http_host(call).is_some()
}

fn http_egress(_tools: &BuiltinTools, call: &ToolCallReq) -> Option<String> {
    http_host(call)
}

fn http_describe(call: &ToolCallReq) -> String {
    let method = call.args.get("method").and_then(Value::as_str).unwrap_or("GET").to_ascii_uppercase();
    match call.args.get("url").and_then(Value::as_str) {
        Some(u) => format!("http_check {method} {}", clip(u, 80)),
        None => "http_check".into(),
    }
}

/// Everything a `browser` call can do is in its plan: the hosts it can reach (the page and each
/// absolute `goto`), the steps that act, the scripts it runs. A page on this machine that acts on
/// a service this session did not start asks, as `http_check` does for a state-changing request.
fn browser_consequence(tools: &BuiltinTools, call: &ToolCallReq, exposure: &Exposure) -> Consequence {
    // A call that cannot be planned fails on its own and reaches nothing.
    let Some(plan) = browser_plan(tools, call) else { return Consequence::Free };
    let steps = |v: Vec<&Step>| clip(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>().join("; "), 160);
    let evals = plan.evals();
    if !plan.local {
        // Script on a remote page always shows its text, even where the host is approved.
        let new: Vec<&str> = plan.hosts.iter().filter(|h| !exposure.approved_hosts.contains(*h)).map(String::as_str).collect();
        if new.is_empty() && evals.is_empty() {
            return Consequence::Free;
        }
        let mut why = Vec::new();
        if !new.is_empty() {
            why.push(match new.as_slice() {
                [_] if plan.hosts.len() == 1 => "a new host".to_string(),
                [only] => format!("a new host: {only}"),
                many => format!("new hosts: {}", many.join(", ")),
            });
        }
        if !evals.is_empty() {
            why.push(format!("runs script on the page [{}]", steps(plan.request.steps.iter().filter(|s| matches!(s, Step::Eval(_))).collect())));
        }
        return Consequence::Ask { summary: format!("browser {}: {}", plan.hosts.join(", "), why.join("; ")) };
    }
    if exposure.tainted && !evals.is_empty() {
        return Consequence::Ask { summary: format!("browser: runs script after an untrusted read [{}]", steps(plan.request.steps.iter().filter(|s| matches!(s, Step::Eval(_))).collect())) };
    }
    let acting = plan.acting_steps();
    match plan.port {
        Some(port) if !acting.is_empty() && !tools.state::<VerifyState>().services.owns_port(port) => {
            Consequence::Ask { summary: format!("browser localhost:{port}: {} on a local service this session did not start", steps(acting)) }
        }
        _ => Consequence::Free,
    }
}

fn browser_taints(tools: &BuiltinTools, call: &ToolCallReq) -> bool {
    browser_plan(tools, call).is_some_and(|p| !p.hosts.is_empty())
}

fn browser_egress(tools: &BuiltinTools, call: &ToolCallReq) -> Option<String> {
    browser_plan(tools, call).and_then(|p| p.hosts.into_iter().next())
}

fn browser_describe(call: &ToolCallReq) -> String {
    match call.args.get("url").and_then(Value::as_str) {
        Some(u) => format!("browser {}", clip(u, 80)),
        None => "browser".into(),
    }
}

// ---- schemas -----------------------------------------------------------------------------------

fn project_spec() -> ToolSpec {
    marlowe_agent::compact_specs::spec("project")
}

fn service_spec() -> ToolSpec {
    marlowe_agent::compact_specs::spec("service")
}

fn http_spec() -> ToolSpec {
    marlowe_agent::compact_specs::spec("http_check")
}

fn browser_spec() -> ToolSpec {
    marlowe_agent::compact_specs::spec("browser")
}

// ---- group hooks -------------------------------------------------------------------------------

/// Loop middleware for this group, built once per host next to its state (see `specs/mod.rs`).
pub fn middleware(tools: &crate::BuiltinTools) -> Vec<std::sync::Arc<dyn marlowe_agent::ToolMiddleware>> {
    super::no_middleware(tools)
}

/// `persona/blocks/verify.md`, appended only while this group is exposed and has a live tool.
pub fn persona_block() -> Option<&'static str> {
    Some(include_str!("../../../../persona/blocks/verify.md"))
}

/// Logs, screenshots and image blobs under the profile home; Edge when it is installed.
pub fn configure(tools: crate::BuiltinTools, setup: &super::HostSetup<'_>) -> crate::BuiltinTools {
    let mut state = VerifyState::default().with_home(setup.home);
    if let Some(edge) = crate::render::EdgeBrowser::find() {
        state = state.with_browser(Arc::new(edge));
    }
    tools.with_state(state)
}

/// Whether a local model gets this group in `root`: a project manifest is there.
pub fn detect(root: Option<&std::path::Path>) -> bool {
    root.is_some_and(marlowe_verify::profile::looks_like_project)
}
