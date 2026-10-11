//! The four tools as functions of JSON arguments: `project`, `service`, `http_check`, `browser`.
//!
//! Each tool has a *plan* (what the call would run, parsed and resolved the same way the call
//! does) and an *execution*. The tool layer (`marlowe-exec/src/specs/verify.rs`) asks the plan
//! for its approval summary, egress host, mutation and sandbox classification, so what is judged
//! is what runs. Nothing here knows about permissions; it reports facts (the command lines, the
//! hosts, whether a credential variable is requested) and the layer above decides.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use marlowe_model::ImageRef;
use serde_json::Value;

use crate::browser::{self, BrowseError, BrowseRequest, Show, Step};
use crate::cmd::{split_args, valid_env_name, Cmd};
use crate::env::Env;
use crate::http::{self, Checks, StatusSpec, Url};
use crate::migrate::{self, Target};
use crate::profile::{Kind, Migrations, ProjectCmd, ProjectProfile};
use crate::run::{self, GateOpts};
use crate::service::{Ready, StartSpec};
use crate::state::VerifyState;

/// A tool result before the tool layer wraps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Out {
    pub text: String,
    pub is_error: bool,
    pub summary: String,
    pub image: Option<ImageRef>,
}

impl Out {
    pub fn ok(text: impl Into<String>, summary: impl Into<String>) -> Self {
        Self { text: text.into(), is_error: false, summary: summary.into(), image: None }
    }

    pub fn err(text: impl Into<String>) -> Self {
        let text = text.into();
        let summary = text.lines().next().unwrap_or("").chars().take(80).collect();
        Self { text, is_error: true, summary, image: None }
    }

    fn verdict(ok: bool, text: String, summary: impl Into<String>) -> Self {
        Self { text, is_error: !ok, summary: summary.into(), image: None }
    }
}

// ---- lenient argument access -------------------------------------------------------------------

/// Arguments from a model: numbers may arrive as strings, lists as one string.
struct A<'a>(&'a Value);

impl A<'_> {
    fn str(&self, k: &str) -> Option<&str> {
        self.0.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
    }

    fn raw_str(&self, k: &str) -> Option<&str> {
        self.0.get(k).and_then(Value::as_str)
    }

    fn bool(&self, k: &str) -> Option<bool> {
        match self.0.get(k)? {
            Value::Bool(b) => Some(*b),
            Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "yes" | "1" => Some(true),
                "false" | "no" | "0" => Some(false),
                _ => None,
            },
            _ => None,
        }
    }

    fn int(&self, k: &str) -> Option<i64> {
        match self.0.get(k)? {
            Value::Number(n) => n.as_i64(),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    /// A list of strings; a lone string is one element.
    fn list(&self, k: &str) -> Vec<String> {
        match self.0.get(k) {
            Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(String::from).or_else(|| v.as_i64().map(|n| n.to_string()))).collect(),
            Some(Value::String(s)) if !s.trim().is_empty() => vec![s.clone()],
            _ => Vec::new(),
        }
    }

    fn has(&self, k: &str) -> bool {
        self.0.get(k).is_some_and(|v| !v.is_null())
    }
}

fn norm_dir(s: &str) -> String {
    let s = s.trim().replace('\\', "/");
    let s = s.trim_start_matches("./").trim_end_matches('/');
    if s.is_empty() { ".".into() } else { s.to_string() }
}

// ---- project -----------------------------------------------------------------------------------

/// What `project` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectOp {
    Profile,
    Run { kind: Kind, extra: Vec<String>, cwd: Option<String> },
    Gate { kinds: Option<Vec<Kind>>, keep_going: bool, baseline: Option<Vec<String>> },
    Migrate { database_url: Option<String>, roundtrip: bool, tool: Option<String> },
}

pub fn parse_project(args: &Value) -> Result<ProjectOp, String> {
    let a = A(args);
    let op = a.str("op").map(str::to_ascii_lowercase);
    let op = match op.as_deref() {
        None if a.has("command") => "run".to_string(),
        None => "profile".to_string(),
        Some(o) => o.to_string(),
    };
    let kind_from = |name: &str| Kind::parse(name).ok_or_else(|| format!("`{name}` is not a project command; use install, format, lint, typecheck, build or test"));
    Ok(match op.as_str() {
        "profile" | "show" | "info" => ProjectOp::Profile,
        "run" => {
            let name = a.str("command").ok_or("`command` is required for run: install, format, lint, typecheck, build or test")?;
            let extra = match args.get("args") {
                Some(Value::String(s)) => split_args(s).map_err(|e| format!("args: {e}"))?,
                _ => a.list("args"),
            };
            ProjectOp::Run { kind: kind_from(name)?, extra, cwd: a.str("cwd").map(norm_dir) }
        }
        "gate" => {
            let kinds = a.list("kinds");
            ProjectOp::Gate {
                kinds: if kinds.is_empty() { None } else { Some(kinds.iter().map(|k| kind_from(k)).collect::<Result<_, _>>()?) },
                keep_going: a.bool("keep_going").unwrap_or(false),
                baseline: if a.has("baseline") { Some(a.list("baseline")) } else { None },
            }
        }
        "migrate" => ProjectOp::Migrate {
            database_url: a.str("database_url").map(String::from),
            roundtrip: a.bool("roundtrip").unwrap_or(false),
            tool: a.str("tool").map(String::from),
        },
        other => match Kind::parse(other) {
            // `project{op:"test"}` is what a model will often write.
            Some(kind) => ProjectOp::Run { kind, extra: a.list("args"), cwd: a.str("cwd").map(norm_dir) },
            None => return Err(format!("unknown op `{other}`; ops are profile, run, gate, migrate")),
        },
    })
}

/// A parsed `project` call with the profile commands it will run.
#[derive(Debug, Clone)]
pub struct ProjectPlan {
    pub op: ProjectOp,
    pub cmds: Vec<ProjectCmd>,
    pub migration: Option<Migrations>,
    pub profile: Arc<ProjectProfile>,
}

pub fn plan_project(env: &Env<'_>, state: &VerifyState, args: &Value) -> Result<ProjectPlan, String> {
    let op = parse_project(args)?;
    let profile = if matches!(op, ProjectOp::Profile) { Arc::new(crate::profile::detect(env.ws, env.root)) } else { state.profile(env.ws, env.root) };
    let mut plan = ProjectPlan { op: op.clone(), cmds: Vec::new(), migration: None, profile: Arc::clone(&profile) };
    let none = |what: &str| -> String {
        let have: Vec<&str> = Kind::ALL.into_iter().filter(|k| !profile.commands_of(*k).is_empty()).map(|k| k.name()).collect();
        if profile.is_empty() {
            "no project is detected here; add .marlowe/project.toml with the commands, or use the shell tools".to_string()
        } else {
            format!("this project has no {what} command (it has: {}); add one to .marlowe/project.toml", if have.is_empty() { "none".into() } else { have.join(", ") })
        }
    };
    match &op {
        ProjectOp::Profile => {}
        ProjectOp::Run { kind, extra, cwd } => {
            let mut cmds: Vec<ProjectCmd> = profile.commands_of(*kind).into_iter().cloned().collect();
            if let Some(c) = cwd {
                cmds.retain(|p| norm_dir(&p.cwd) == *c);
            }
            if cmds.is_empty() {
                return Err(none(kind.name()));
            }
            if cmds.len() > 1 && !extra.is_empty() {
                return Err(format!("{} has {} commands ({}); name one with cwd so the extra args go to the right one", kind.name(), cmds.len(), cmds.iter().map(|c| c.cwd.clone()).collect::<Vec<_>>().join(", ")));
            }
            plan.cmds = cmds;
        }
        ProjectOp::Gate { kinds, .. } => {
            let wanted: Vec<Kind> = match kinds {
                Some(k) => Kind::GATE.into_iter().filter(|g| k.contains(g)).collect(),
                None => Kind::GATE.to_vec(),
            };
            plan.cmds = wanted.iter().flat_map(|k| profile.commands_of(*k)).cloned().collect();
            if plan.cmds.is_empty() {
                return Err(none("format, lint, typecheck, build or test"));
            }
        }
        ProjectOp::Migrate { tool, .. } => {
            let m = match tool {
                Some(t) => profile.migrations.iter().find(|m| m.tool.eq_ignore_ascii_case(t)),
                None => profile.migrations.first(),
            };
            plan.migration = Some(m.cloned().ok_or_else(|| {
                "no migrations are detected (alembic, django, prisma, knex, drizzle, sequelize, goose, golang-migrate); describe them in .marlowe/project.toml [migrations]".to_string()
            })?);
        }
    }
    if let Some(ms) = A(args).int("timeout_ms") {
        // A long suite may need more than the ten-minute default.
        for c in &mut plan.cmds {
            c.timeout_s = Some((ms / 1000).clamp(1, 1800) as u64);
        }
    }
    Ok(plan)
}

/// The key a call is judged and checked under.
pub fn judge_key(tool: &str, args: &Value) -> String {
    format!("{tool}:{args}")
}

const CHANGED: &str = "Nothing ran: the commands this call would run changed after it was judged (a file that defines them, \
    such as .marlowe/project.toml or package.json, was edited in the same step). Call it again.";

impl ProjectPlan {
    /// What this call runs, as one string: compared between the moment its approval was decided
    /// and the moment it runs.
    pub fn fingerprint(&self, env: &Env<'_>) -> String {
        let dirs: Vec<String> = self.work_dirs(env).iter().map(|d| d.display().to_string()).collect();
        let mig = self.migration.as_ref().map(|m| format!("{:?}|{:?}|{:?}", m.up, m.down, m.env)).unwrap_or_default();
        format!(
            "{:?}|{:?}|{}|{}|{:?}|{:?}",
            self.command_lines(),
            self.cmds.iter().map(|c| (&c.cwd, c.timeout_s, &c.pre)).collect::<Vec<_>>(),
            dirs.join(";"),
            mig,
            self.user_db(),
            self.runs()
        )
    }

    /// Every command line this call runs, extra arguments included.
    pub fn command_lines(&self) -> Vec<String> {
        let extra: &[String] = match &self.op {
            ProjectOp::Run { extra, .. } => extra,
            _ => &[],
        };
        let mut v: Vec<String> = self
            .cmds
            .iter()
            .map(|c| {
                let mut cmd = c.cmd.clone();
                if !extra.is_empty() {
                    if let Some(sep) = c.args_separator.as_ref().filter(|s| !cmd.argv.contains(s)) {
                        cmd.argv.push(sep.clone());
                    }
                    cmd.argv.extend(extra.iter().cloned());
                }
                cmd.display()
            })
            .collect();
        if let Some(m) = &self.migration {
            v.push(m.up.display());
        }
        v
    }

    /// What the commands execute when a manifest says so (package.json script bodies).
    pub fn runs(&self) -> Vec<String> {
        let mut v: Vec<String> = self.cmds.iter().filter_map(|c| c.runs.clone()).collect();
        v.dedup();
        v
    }

    /// Registry hosts the install commands reach.
    pub fn hosts(&self) -> Vec<String> {
        let mut h: Vec<String> = self.cmds.iter().filter_map(|c| c.registry_host.clone()).collect();
        h.sort();
        h.dedup();
        h
    }

    /// The database a migrate call names, redacted (a throwaway needs no approval).
    pub fn user_db(&self) -> Option<String> {
        match &self.op {
            ProjectOp::Migrate { database_url: Some(u), .. } => Some(migrate::redact_url(u)),
            _ => None,
        }
    }

    /// Where the call may change files.
    pub fn work_dirs(&self, env: &Env<'_>) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = self.cmds.iter().filter_map(|c| env.cwd(&c.cwd).ok()).collect();
        if let Some(m) = &self.migration {
            dirs.extend(env.cwd(&m.cwd).ok());
        }
        dirs.sort();
        dirs.dedup();
        dirs
    }

    pub fn is_install(&self) -> bool {
        self.cmds.iter().any(|c| c.kind == Kind::Install)
    }
}

pub fn project(env: &Env<'_>, state: &VerifyState, args: &Value) -> Out {
    let plan = match plan_project(env, state, args) {
        Ok(p) => p,
        Err(e) => return Out::err(e),
    };
    if !matches!(plan.op, ProjectOp::Profile) && !state.judged_matches(&judge_key("project", args), &plan.fingerprint(env)) {
        return Out::err(CHANGED);
    }
    match &plan.op {
        ProjectOp::Profile => {
            let p = &plan.profile;
            Out::ok(p.render(), if p.is_empty() { "no project".to_string() } else { format!("{} stack(s)", p.stacks.len()) })
        }
        ProjectOp::Run { extra, .. } => {
            let reports: Vec<_> = plan.cmds.iter().map(|c| run::run_project_cmd(env, c, extra, &state.logs)).collect();
            let ok = reports.iter().all(|r| r.ok);
            let text = reports.iter().map(|r| r.render()).collect::<Vec<_>>().join("\n");
            let summary = match reports.iter().find(|r| !r.ok) {
                None => format!("{} ok", reports[0].kind.name()),
                Some(r) if r.not_run.is_some() => format!("{} not run", r.kind.name()),
                Some(r) if !r.failing.is_empty() => format!("{} failed ({} failing)", r.kind.name(), r.failing.len()),
                Some(r) => format!("{} failed (exit {})", r.kind.name(), r.exit),
            };
            Out::verdict(ok, text, summary)
        }
        ProjectOp::Gate { kinds, keep_going, baseline } => {
            let opts = GateOpts { kinds: kinds.clone(), keep_going: *keep_going || baseline.is_some(), baseline: baseline.clone() };
            let g = run::gate(env, &plan.profile, &opts, &state.logs);
            let ok = if baseline.is_some() { g.ok_vs_baseline() } else { g.ok };
            let summary = match (&g.new_failures, ok) {
                (Some(_), true) => format!("gate: no new failures ({} known)", baseline.as_ref().map_or(0, Vec::len)),
                (Some(n), false) => format!("gate: {} new failure(s)", n.len()),
                (None, true) => "gate passed".to_string(),
                (None, false) => "gate failed".to_string(),
            };
            Out::verdict(ok, g.render(), summary)
        }
        ProjectOp::Migrate { database_url, roundtrip, .. } => {
            let m = plan.migration.as_ref().expect("planned");
            let target = match database_url {
                Some(u) => Target::User(u.clone()),
                None => Target::Throwaway,
            };
            match migrate::migrate(env, m, &target, *roundtrip, &state.logs) {
                Ok(r) => Out::verdict(r.ok(), r.render(), if r.ok() { format!("{} migrated", m.tool) } else { format!("{} migration failed", m.tool) }),
                Err(e) => Out::err(e),
            }
        }
    }
}

// ---- service -----------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceOp {
    Start,
    Stop,
    Restart,
    Status,
    Logs,
}

/// A parsed `service` call.
#[derive(Debug, Clone)]
pub struct ServicePlan {
    pub op: ServiceOp,
    pub name: Option<String>,
    pub spec: Option<StartSpec>,
    pub tail: usize,
    /// Requested credential variables (names the harness scrubs from children): the user approves these.
    pub credentials: Vec<String>,
    /// The package.json script a profile service runs, when it is one.
    pub runs: Option<String>,
}

fn sanitize_name(s: &str) -> String {
    let n: String = s.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' }).take(40).collect();
    if n.is_empty() { "service".into() } else { n }
}

pub fn plan_service(env: &Env<'_>, state: &VerifyState, args: &Value) -> Result<ServicePlan, String> {
    let a = A(args);
    let op = match a.str("op").map(str::to_ascii_lowercase).as_deref() {
        Some("start") => ServiceOp::Start,
        Some("stop") => ServiceOp::Stop,
        Some("restart") => ServiceOp::Restart,
        Some("status") | Some("list") | None => ServiceOp::Status,
        Some("logs") | Some("log") | Some("output") => ServiceOp::Logs,
        Some(o) => return Err(format!("unknown op `{o}`; ops are start, stop, restart, status, logs")),
    };
    let name = a.str("name").map(String::from);
    let tail = a.int("tail").unwrap_or(60).clamp(1, 500) as usize;
    if matches!(op, ServiceOp::Stop | ServiceOp::Logs | ServiceOp::Status) {
        return Ok(ServicePlan { op, name, spec: None, tail, credentials: Vec::new(), runs: None });
    }
    let profile = state.profile(env.ws, env.root);
    let defined = || profile.services.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", ");
    let base = match (&name, a.str("command")) {
        (_, Some(_)) => name.as_deref().and_then(|n| profile.service(n)),
        (Some(n), None) => Some(profile.service(n).ok_or_else(|| {
            format!("no service `{n}` in the profile and no `command` given{}", if profile.services.is_empty() { String::new() } else { format!(" (defined: {})", defined()) })
        })?),
        (None, None) if profile.services.len() == 1 => profile.services.first(),
        (None, None) => {
            return Err(if profile.services.is_empty() {
                "give a `command` to start (the profile defines no services)".to_string()
            } else {
                format!("which service? name one of: {}, or give a `command`", defined())
            })
        }
    };
    let cmd = match a.str("command") {
        Some(c) => Cmd::parse(c).map_err(|e| e.to_string())?,
        None => base.map(|b| b.cmd.clone()).ok_or("no command")?,
    };
    let svc_name = name.clone().or_else(|| base.map(|b| b.name.clone())).unwrap_or_else(|| sanitize_name(&cmd.program_name()));
    let cwd_rel = a.str("cwd").map(norm_dir).or_else(|| base.map(|b| b.cwd.clone())).unwrap_or_else(|| ".".into());
    let cwd = env.cwd(&cwd_rel)?;
    let pick = |own: Option<String>, from: Option<&Option<String>>| own.or_else(|| from.and_then(|f| f.clone()));
    let ready_status = match args.get("ready_status") {
        Some(v) if !v.is_null() => Some(StatusSpec::from_json(v)?),
        _ => None,
    };
    let mut env_allow: Vec<String> = base.map(|b| b.env_allow.clone()).unwrap_or_default();
    for n in a.list("env_allow") {
        if !valid_env_name(&n) {
            return Err(format!("env_allow `{n}` is not a variable name"));
        }
        if !env_allow.iter().any(|x| x.eq_ignore_ascii_case(&n)) {
            env_allow.push(n);
        }
    }
    let ready = Ready {
        url: pick(a.str("ready_url").map(String::from), base.map(|b| &b.ready_url)),
        port: a.int("ready_port").and_then(|p| u16::try_from(p).ok()).or_else(|| base.and_then(|b| b.ready_port)),
        log: pick(a.raw_str("ready_log").map(String::from).filter(|s| !s.is_empty()), base.map(|b| &b.ready_log)),
        status: ready_status,
        timeout: a.int("ready_timeout_ms").map(|ms| Duration::from_millis(ms.clamp(1_000, 600_000) as u64)),
    };
    let credentials = env_allow
        .iter()
        .filter(|n| marlowe_os::child::is_scrubbed(n) || env.scrub.iter().any(|s| s.eq_ignore_ascii_case(n)))
        .cloned()
        .collect();
    let runs = if a.str("command").is_none() { base.and_then(|b| b.runs.clone()) } else { None };
    Ok(ServicePlan { op, name: Some(svc_name.clone()), spec: Some(StartSpec { name: svc_name, cmd, cwd, ready, env_allow, env: Vec::new() }), tail, credentials, runs })
}

impl ServicePlan {
    /// What a start runs, as one string (see [`ProjectPlan::fingerprint`]).
    pub fn fingerprint(&self) -> String {
        match &self.spec {
            Some(s) => format!("{:?}|{}|{:?}|{:?}|{:?}|{:?}", s.cmd, s.cwd.display(), s.env_allow, s.ready, self.credentials, self.runs),
            None => String::new(),
        }
    }

    /// The command line a start runs.
    pub fn display(&self) -> Option<String> {
        self.spec.as_ref().map(|s| s.cmd.display())
    }

    /// The service runs inline shell or interpreter code.
    pub fn indirect(&self) -> bool {
        self.spec.as_ref().is_some_and(|s| s.cmd.is_inline_code())
    }
}

pub fn service(env: &Env<'_>, state: &VerifyState, args: &Value) -> Out {
    let plan = match plan_service(env, state, args) {
        Ok(p) => p,
        Err(e) => return Out::err(e),
    };
    let table = &state.services;
    if matches!(plan.op, ServiceOp::Start | ServiceOp::Restart) && !state.judged_matches(&judge_key("service", args), &plan.fingerprint()) {
        return Out::err(CHANGED);
    }
    match plan.op {
        ServiceOp::Start | ServiceOp::Restart => {
            let spec = plan.spec.expect("planned");
            let mut note = String::new();
            if plan.op == ServiceOp::Restart && table.stop(&spec.name).is_some() {
                note = format!("(stopped the running `{}` first)\n", spec.name);
            }
            let shown = spec.cmd.display();
            let creds = plan.credentials.clone();
            match table.start(env, spec) {
                Ok(s) => {
                    let mut t = format!("{note}service `{}` started{}: `{shown}`\n", s.name, s.pid.map(|p| format!(" (pid {p})")).unwrap_or_default());
                    if let Some(u) = &s.url {
                        t.push_str(&format!("answering at {u}\n"));
                    }
                    t.push_str(&format!("ready after {:.1} s: {}\n", s.waited.as_secs_f64(), s.ready_by.join("; ")));
                    if !creds.is_empty() {
                        t.push_str(&format!("credential variables passed in: {}\n", creds.join(", ")));
                    }
                    t.push_str("it keeps running until `service{op:\"stop\"}` or the end of this session; `service{op:\"logs\"}` shows its output.");
                    Out::ok(t, format!("{} ready", s.name))
                }
                Err(e) => Out::err(format!("{note}{e}")),
            }
        }
        ServiceOp::Stop => match plan.name.as_deref() {
            Some("all") | None => {
                let names = table.stop_all();
                if names.is_empty() { Out::ok("no services were running", "none running") } else { Out::ok(format!("stopped: {}", names.join(", ")), format!("{} stopped", names.len())) }
            }
            Some(n) => match table.stop(n) {
                Some(tail) => Out::ok(format!("stopped `{n}`.\nlast output:\n{tail}"), format!("{n} stopped")),
                None => Out::err(format!("no service `{n}` is running (running: {})", table.names().join(", "))),
            },
        },
        ServiceOp::Logs => {
            let names = table.names();
            let Some(n) = plan.name.clone().or_else(|| if names.len() == 1 { names.first().cloned() } else { None }) else {
                return Out::err(format!("which service? running: {}", if names.is_empty() { "none".into() } else { names.join(", ") }));
            };
            match table.logs(&n, plan.tail) {
                Some(t) if t.trim().is_empty() => Out::ok(format!("`{n}` has printed nothing yet"), "no output"),
                Some(t) => Out::ok(format!("last {} lines of `{n}`:\n{t}", plan.tail), format!("{n} logs")),
                None => Out::err(format!("no service `{n}` is running (running: {})", if names.is_empty() { "none".into() } else { names.join(", ") })),
            }
        }
        ServiceOp::Status => {
            let st = table.status();
            let profile = state.profile(env.ws, env.root);
            let mut t = String::new();
            if st.is_empty() {
                t.push_str("no services are running\n");
            } else {
                t.push_str("running:\n");
                for s in &st {
                    t.push_str(&format!("  {}\n", s.render()));
                }
            }
            let idle: Vec<&str> = profile.services.iter().filter(|d| !st.iter().any(|s| s.name.eq_ignore_ascii_case(&d.name))).map(|d| d.name.as_str()).collect();
            if !idle.is_empty() {
                t.push_str(&format!("defined in the profile, not started: {}\n", idle.join(", ")));
            }
            Out::ok(t.trim_end(), format!("{} running", st.iter().filter(|s| s.running).count()))
        }
    }
}

// ---- http_check --------------------------------------------------------------------------------

/// A parsed `http_check`.
#[derive(Debug)]
pub struct HttpPlan {
    pub request: http::Request,
    pub checks: Checks,
    pub wait: Duration,
}

pub fn plan_http(args: &Value) -> Result<HttpPlan, String> {
    let a = A(args);
    let raw = a.str("url").ok_or("`url` is required")?;
    // `:3000/health` and `localhost:3000` are what people type.
    let url = if raw.starts_with(':') {
        format!("http://localhost{raw}")
    } else if !raw.contains("://") {
        format!("http://{raw}")
    } else {
        raw.to_string()
    };
    let mut request = http::Request::get(&url).map_err(|e| e.to_string())?;
    request.method = a.str("method").unwrap_or("GET").to_ascii_uppercase();
    for h in a.list("headers") {
        let (k, v) = h.split_once(':').ok_or_else(|| format!("header `{h}` must be `Name: value`"))?;
        request.headers.push((k.trim().to_string(), v.trim().to_string()));
    }
    request.body = a.raw_str("body").map(String::from);
    if request.body.is_some() && !request.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")) {
        let looks_json = request.body.as_deref().is_some_and(|b| matches!(b.trim_start().chars().next(), Some('{') | Some('[')));
        request.headers.push(("Content-Type".into(), if looks_json { "application/json" } else { "text/plain" }.into()));
    }
    request.timeout = Duration::from_millis(a.int("timeout_ms").unwrap_or(15_000).clamp(500, 120_000) as u64);
    let status = match args.get("expect_status") {
        Some(v) if !v.is_null() => Some(StatusSpec::from_json(v)?),
        _ => None,
    };
    let checks = Checks { status, contains: a.list("expect_contains"), not_contains: a.list("expect_not_contains"), json: a.list("expect_json"), headers: a.list("expect_header") };
    let wait = Duration::from_millis(a.int("wait_ms").unwrap_or(0).clamp(0, 300_000) as u64);
    Ok(HttpPlan { request, checks, wait })
}

pub fn http_check(env: &Env<'_>, args: &Value) -> Out {
    let plan = match plan_http(args) {
        Ok(p) => p,
        Err(e) => return Out::err(e),
    };
    let checked = http::poll(&plan.request, &plan.checks, plan.wait, &|| env.cancel.is_cancelled());
    let text = http::render(&plan.request, &checked);
    let summary = match &checked.response {
        Ok(r) if checked.ok() => format!("{} ok", r.status),
        Ok(r) => format!("{} check failed", r.status),
        Err(e) => e.to_string().chars().take(60).collect(),
    };
    Out::verdict(checked.ok(), text, summary)
}

// ---- browser -----------------------------------------------------------------------------------

/// A parsed `browser` call.
#[derive(Debug)]
pub struct BrowserPlan {
    pub request: BrowseRequest,
    pub show: Show,
    /// A page on this machine (loopback or a file in the workspace).
    pub local: bool,
    /// The remote host, when it is not.
    pub host: Option<String>,
}

fn file_url(p: &std::path::Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let s = s.trim_start_matches("//?/");
    let enc: String = s.chars().map(|c| match c {
        ' ' => "%20".to_string(),
        '#' => "%23".to_string(),
        '?' => "%3F".to_string(),
        '%' => "%25".to_string(),
        c => c.to_string(),
    }).collect();
    if enc.starts_with('/') { format!("file://{enc}") } else { format!("file:///{enc}") }
}

pub fn plan_browser(env: &Env<'_>, args: &Value) -> Result<BrowserPlan, String> {
    let a = A(args);
    let raw = a.str("url").ok_or("`url` is required (http://localhost:PORT/..., or a path to an html file in the workspace)")?;
    let (url, local, host) = if raw.starts_with("http://") || raw.starts_with("https://") {
        let u = Url::parse(raw).map_err(|e| e.to_string())?;
        if u.is_loopback() {
            if u.https {
                return Err("https to localhost is not supported (self-signed certificate); use http://localhost:PORT".into());
            }
            (u.display(), true, None)
        } else {
            (u.display(), false, Some(u.host))
        }
    } else if raw.starts_with(':') {
        (format!("http://localhost{raw}"), true, None)
    } else if raw.starts_with("localhost") || raw.starts_with("127.0.0.1") {
        (format!("http://{raw}"), true, None)
    } else {
        let path = match raw.strip_prefix("file:///") {
            Some(p) => PathBuf::from(p.replace("%20", " ")),
            None => crate::env::resolve(env.root, raw),
        };
        let path = if path.is_dir() { path.join("index.html") } else { path };
        if !path.is_file() {
            return Err(format!("{} does not exist; give an http://localhost:PORT URL or an html file in the workspace", path.display()));
        }
        let boundary = env.pin.unwrap_or(env.root);
        if !crate::env::inside(boundary, &path) {
            return Err(format!("{} is outside the workspace; a file page must be inside it", path.display()));
        }
        (file_url(&path.canonicalize().unwrap_or(path)), true, None)
    };
    let steps = Step::parse_all(&a.list("steps"))?;
    let action = a.str("action").map(str::to_ascii_lowercase);
    let show = match action.as_deref() {
        None | Some("snapshot") | Some("check") | Some("text") => Show::Snapshot,
        Some("screenshot") | Some("image") | Some("shot") => Show::Screenshot,
        Some("console") | Some("errors") | Some("logs") => Show::Console,
        Some(o) => return Err(format!("unknown action `{o}`; actions are snapshot, screenshot, console")),
    };
    let width = a.int("width").unwrap_or(1280).clamp(200, 3840) as u32;
    let height = a.int("height").unwrap_or(800).clamp(200, 2400) as u32;
    let request = BrowseRequest {
        url,
        strict_local: local,
        viewport: (width, height),
        steps,
        wait_for: a.str("wait_for").map(String::from),
        snapshot: show == Show::Snapshot,
        screenshot: show == Show::Screenshot || a.bool("screenshot").unwrap_or(false),
        full_page: a.bool("full_page").unwrap_or(false),
        timeout: Duration::from_millis(a.int("timeout_ms").unwrap_or(45_000).clamp(5_000, 180_000) as u64),
    };
    Ok(BrowserPlan { request, show, local, host })
}

pub fn browser(env: &Env<'_>, state: &VerifyState, args: &Value) -> Out {
    let plan = match plan_browser(env, args) {
        Ok(p) => p,
        Err(e) => return Out::err(e),
    };
    let Some(driver) = state.browser().filter(|b| b.available()) else {
        return Out::err(BrowseError::NoBrowser.to_string());
    };
    match driver.run(&plan.request, env.cancel) {
        Err(e) => Out::err(format!("browser {}: {e}", plan.request.url)),
        Ok(report) => {
            let shot = report.screenshot.as_deref().map(|png| state.shots.save(png));
            let (shot, shot_err) = match shot {
                Some(Ok(s)) => (Some(s), None),
                Some(Err(e)) => (None, Some(e)),
                None => (None, None),
            };
            let mut text = browser::render(&plan.request, &report, plan.show, shot.as_ref());
            if let Some(e) = shot_err {
                text.push_str(&format!("\nscreenshot not saved: {e}"));
            }
            let errors = report.errors().count();
            let summary = match (errors, report.failed_step()) {
                (_, Some(_)) => "a step failed".to_string(),
                (0, None) => "loaded, console clean".to_string(),
                (n, None) => format!("{n} console error(s)"),
            };
            let mut out = Out::ok(text, summary);
            out.image = shot.and_then(|s| s.image_ref());
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn project_args_are_read_leniently() {
        assert_eq!(parse_project(&json!({})).unwrap(), ProjectOp::Profile);
        assert_eq!(
            parse_project(&json!({"command": "test", "args": "-k login", "cwd": "./backend/"})).unwrap(),
            ProjectOp::Run { kind: Kind::Test, extra: vec!["-k".into(), "login".into()], cwd: Some("backend".into()) }
        );
        assert_eq!(parse_project(&json!({"op": "test"})).unwrap(), ProjectOp::Run { kind: Kind::Test, extra: vec![], cwd: None });
        assert_eq!(parse_project(&json!({"op": "run", "command": "tsc"})).unwrap(), ProjectOp::Run { kind: Kind::Typecheck, extra: vec![], cwd: None });
        assert!(matches!(parse_project(&json!({"op": "gate", "kinds": ["lint", "test"], "keep_going": "true", "baseline": ["a::b"]})).unwrap(), ProjectOp::Gate { keep_going: true, baseline: Some(b), kinds: Some(k) } if b == ["a::b"] && k.len() == 2));
        assert!(parse_project(&json!({"op": "run"})).unwrap_err().contains("`command` is required"));
        assert!(parse_project(&json!({"op": "fly"})).unwrap_err().contains("unknown op"));
        assert!(parse_project(&json!({"command": "test", "args": "a && b"})).unwrap_err().contains("shell syntax"));
    }

    #[test]
    fn http_args_normalise_urls_and_validate_checks() {
        let p = plan_http(&json!({"url": ":3000/health", "expect_status": "2xx", "expect_json": ["ok=true"], "headers": ["X-A: 1"], "wait_ms": "2000"})).unwrap();
        assert_eq!(p.request.url.display(), "http://localhost:3000/health");
        assert_eq!(p.request.headers, [("X-A".to_string(), "1".to_string())]);
        assert_eq!(p.wait, Duration::from_secs(2));
        assert!(p.checks.status.as_ref().unwrap().accepts(204));
        assert_eq!(plan_http(&json!({"url": "localhost:8080"})).unwrap().request.url.port, 8080);
        assert!(plan_http(&json!({"url": "http://x/", "headers": ["no colon"]})).is_err());
        assert!(plan_http(&json!({})).unwrap_err().contains("`url` is required"));
        let p = plan_http(&json!({"url": "http://localhost/api", "method": "post", "body": "{\"a\":1}"})).unwrap();
        assert_eq!(p.request.method, "POST");
        assert!(p.request.headers.iter().any(|(k, v)| k == "Content-Type" && v == "application/json"));
    }

    #[test]
    fn file_urls_encode_what_needs_it() {
        assert_eq!(file_url(std::path::Path::new("C:\\a b\\c#d.html")), "file:///C:/a%20b/c%23d.html");
        assert_eq!(file_url(std::path::Path::new("/tmp/x.html")), "file:///tmp/x.html");
    }
}
