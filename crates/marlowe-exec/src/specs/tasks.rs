//! The tasks group (track N): `task_list`, `task_output`, `task_stop`, `monitor`, `schedule` and
//! `watch`.
//!
//! One registry (`marlowe_agent::Tasks`, the daemon's) holds everything the conductor started and
//! is not waiting for: background shells (`job_*`), agents and crew runs (`agent_*`), workflows
//! (`wf_*`), monitors (`mon_*`) and schedules (`cron_*`). The tools here read and stop it; a job
//! id goes to the host's own job table first (`jobs/`), so a host without the registry (a crew
//! sandbox) still reads and stops its shells with `task_output` / `task_stop`. What
//! needs the daemon's threads, files or windows (schedules, the Sentinel, other agents) goes
//! through the `Background` service. Both are installed by the daemon per session; a host without
//! them (a crew sandbox) does not offer the group.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use marlowe_agent::background::{clamp_wakeup, CronRequest, WatchAction};
use marlowe_agent::cron::{self, CronExpr};
use marlowe_agent::todo::Questions;
use marlowe_agent::{CallCtx, Consequence, Effects, Exposure, TaskInfo, Tasks, ToolCallReq, ToolGroup, ToolOutcome};
use marlowe_proto::{Question, SessionId};
use serde_json::{json, Value};

use super::{describe_by, ToolDef};
use crate::{first_line, int_arg, str_arg, BuiltinTools};

/// Whether the group is live on a host: the daemon's conductor sessions configure it.
#[derive(Debug, Default)]
pub struct TasksGroup {
    pub enabled: bool,
}

fn live(t: &BuiltinTools) -> bool {
    t.state::<TasksGroup>().enabled
}

/// `task_output` and `task_stop` also serve a shell host's own jobs, registry or not.
fn job_or_live(t: &BuiltinTools) -> bool {
    live(t) || t.exposure().1.contains(&ToolGroup::Shell)
}

const NO_REGISTRY: &str = "background tasks are not available in this session";
const NO_DAEMON: &str = "this needs the daemon, which this session does not have";

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// `4m12s` for a duration in ms.
pub fn fmt_elapsed(ms: i64) -> String {
    let s = (ms.max(0) / 1000) as u64;
    match s {
        0..=59 => format!("{s}s"),
        60..=3_599 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3_600, (s % 3_600) / 60),
    }
}

fn task_line(t: &TaskInfo, me: &SessionId, now: i64) -> String {
    let end = t.ended_ms.unwrap_or(now);
    let mut line = format!(
        "{:<14} {:<9} {:<8} {:>7}  {}",
        t.id,
        t.kind.as_str(),
        t.status.as_str(),
        fmt_elapsed(end - t.started_ms),
        first_line(&t.title, 80)
    );
    if let Some(d) = t.detail.as_deref().filter(|d| !d.is_empty()) {
        line.push_str(&format!("  [{}]", first_line(d, 60)));
    }
    if let Some(s) = t.session.as_ref().filter(|s| *s != me) {
        line.push_str(&format!("  (other conversation {s})"));
    }
    if let Some(f) = &t.output_file {
        line.push_str(&format!("\n{:<14} output: {f}", ""));
    }
    line
}

// ---- task_list

fn task_list_spec() -> marlowe_model::ToolSpec {
    marlowe_agent::compact_specs::spec("task_list")
}

fn task_list_call(args: &Value, cx: &CallCtx) -> ToolOutcome {
    let Some(tasks) = cx.scope.services.get::<Tasks>() else { return ToolOutcome::error(NO_REGISTRY) };
    let all = args.get("all").and_then(Value::as_bool).unwrap_or(false);
    let running_only = args.get("running").and_then(Value::as_bool).unwrap_or(false);
    // Every conversation's running work is Matthew's, so it is listed; only the finished work of
    // other conversations waits for `all`.
    let mut list = tasks.0.list(None);
    if !all {
        list.retain(|t| !t.status.is_terminal() || t.session.as_ref().is_none_or(|owner| *owner == cx.session));
    }
    if running_only {
        list.retain(|t| !t.status.is_terminal());
    }
    let now = now_ms();
    let active = list.iter().filter(|t| !t.status.is_terminal()).count();
    let mut out = String::new();
    if list.is_empty() {
        out.push_str("No background tasks.");
    } else {
        out.push_str(&format!("{} task{} ({active} active)\n", list.len(), if list.len() == 1 { "" } else { "s" }));
        out.push_str(&list.iter().map(|t| task_line(t, &cx.session, now)).collect::<Vec<_>>().join("\n"));
    }
    let mut count = list.len();
    // Schedules (this session's and the durable ones) and the sessions `send_message` reaches
    // are tasks of a kind too; the registry may not carry them.
    if let Some(bg) = cx.scope.services.get::<marlowe_agent::Background>() {
        let shown: std::collections::HashSet<&str> = list.iter().map(|t| t.id.as_str()).collect();
        let schedules: Vec<String> = bg
            .0
            .cron_list(&cx.session)
            .iter()
            .filter(|s| !shown.contains(s.id.as_str()))
            .map(|s| {
                let what = match (&s.cron, s.once_at_ms, s.every_secs) {
                    (Some(c), _, _) => format!("cron `{c}`"),
                    (None, Some(_), _) => "once".to_string(),
                    (None, None, n) => format!("every {}", marlowe_agent::commands::describe_interval(n)),
                };
                let next = s.next_at_ms.map(cron::describe_ms).unwrap_or_else(|| "-".into());
                let scope = if s.durable { "durable" } else { "session" };
                let missed = if s.missed > 0 { format!(" (missed {})", s.missed) } else { String::new() };
                format!("{:<18} {what:<22} next {next}  {scope}{missed}  {}", s.id, first_line(&s.text, 60))
            })
            .collect();
        if !schedules.is_empty() {
            count += schedules.len();
            out.push_str(&format!("\nschedules:\n{}", schedules.join("\n")));
        }
        let sessions: Vec<String> = bg
            .0
            .list_agents(&cx.session)
            .iter()
            .filter(|r| r.kind == "session")
            .map(|r| {
                let ws = r.workspace.as_deref().map(|w| format!("  [{}]", first_line(w, 50))).unwrap_or_default();
                format!("{:<22} {:<10} {}{ws}", r.id, r.status, first_line(&r.title, 70))
            })
            .collect();
        if !sessions.is_empty() {
            count += sessions.len();
            out.push_str(&format!("\nsessions you can send_message to:\n{}", sessions.join("\n")));
        }
    }
    if count == 0 {
        return ToolOutcome::ok("No background tasks.", "no tasks");
    }
    ToolOutcome::ok(out, format!("{count} tasks"))
}

// ---- task_output

/// Most of an output window one call returns.
const OUTPUT_WINDOW: usize = 64 * 1024;

fn task_output_spec() -> marlowe_model::ToolSpec {
    marlowe_agent::compact_specs::spec("task_output")
}

/// Whether `id` (`job_j3` or `j3`) names a job of the calling session in the host's own table.
fn own_job(tools: &BuiltinTools, id: &str, cx: &CallCtx) -> bool {
    let raw = id.strip_prefix("job_").unwrap_or(id);
    tools.state::<crate::jobs::HostJobs>().find(raw).is_some_and(|j| j.session == cx.session.0)
}

fn task_output_call(tools: &BuiltinTools, args: &Value, cx: &CallCtx) -> ToolOutcome {
    let Some(id) = str_arg(args, "id").map(str::trim).filter(|s| !s.is_empty()) else {
        return ToolOutcome::error("`id` is required: a task id from task_list, e.g. job_j3");
    };
    let watch = args.get("watch").and_then(Value::as_bool).unwrap_or(false);
    if own_job(tools, id, cx) {
        let mut a = json!({"id": id});
        for (from, to) in [("offset", "since"), ("filter", "filter"), ("until", "until_regex")] {
            if let Some(v) = args.get(from).filter(|v| !v.is_null()) {
                a[to] = v.clone();
            }
        }
        return if watch { crate::jobs::job_watch(tools, &a, cx) } else { crate::jobs::job_output(tools, &a, cx) };
    }
    if watch {
        return ToolOutcome::error(format!(
            "`watch` follows a background shell job (job_...); {id} is not one of this session's. Read it without `watch`"
        ));
    }
    // Without the registry only the host's own jobs exist: say which ones.
    let Some(tasks) = cx.scope.services.get::<Tasks>() else { return crate::jobs::job_output(tools, &json!({"id": id}), cx) };
    let since = int_arg(args, "offset").filter(|s| *s >= 0).map(|s| s as u64);
    let filter = str_arg(args, "filter");
    match tasks.0.output(id, since, filter) {
        Err(e) => ToolOutcome::error(e),
        Ok(chunk) => {
            let mut text = chunk.text;
            let mut next = chunk.next_since;
            let mut note = "";
            if text.len() > OUTPUT_WINDOW {
                let mut cut = OUTPUT_WINDOW;
                while !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                let cut = text[..cut].rfind('\n').map_or(cut, |i| i + 1);
                text.truncate(cut);
                if filter.is_some() {
                    note = "\n[more matching lines were cut; narrow the filter]";
                } else {
                    next = since.unwrap_or(0) + cut as u64;
                    note = "\n[the window was cut here; read on from the next offset]";
                }
            }
            let state = if chunk.done { "finished" } else { "still running" };
            let bytes = text.len();
            let shown = if text.is_empty() { "(nothing new)".to_string() } else { text };
            ToolOutcome::ok(format!("{shown}{note}\n[next offset {next}; {id} {state}]"), format!("{bytes} bytes"))
        }
    }
}

// ---- task_stop

fn task_stop_spec() -> marlowe_model::ToolSpec {
    marlowe_agent::compact_specs::spec("task_stop")
}

fn task_stop_call(tools: &BuiltinTools, args: &Value, cx: &CallCtx) -> ToolOutcome {
    let Some(id) = str_arg(args, "id").map(str::trim).filter(|s| !s.is_empty()) else {
        return ToolOutcome::error("`id` is required: a task id from task_list, e.g. job_j3");
    };
    if own_job(tools, id, cx) {
        return crate::jobs::job_kill(tools, &json!({"id": id}), cx);
    }
    let Some(tasks) = cx.scope.services.get::<Tasks>() else { return crate::jobs::job_kill(tools, &json!({"id": id}), cx) };
    // A schedule: deleted through the daemon unless the registry holds it as a task.
    if id.starts_with("cron_") && tasks.0.get(id).is_none() {
        let Some(bg) = cx.scope.services.get::<marlowe_agent::Background>() else { return ToolOutcome::error(NO_DAEMON) };
        return match bg.0.cron_delete(&cx.session, id) {
            Ok(()) => ToolOutcome::ok(format!("Deleted {id}."), format!("{id} deleted")),
            Err(e) => ToolOutcome::error(e),
        };
    }
    if let Some(owner) = tasks.0.get(id).and_then(|t| t.session).filter(|s| *s != cx.session) {
        // Another session's task: the user decides.
        let Some(q) = cx.scope.services.get::<Questions>() else {
            return ToolOutcome::error(format!("{id} belongs to session {owner}; only that session or the user may stop it"));
        };
        let ask = Question {
            question: format!("Stop {id}, which belongs to session {owner}?"),
            header: "Stop task".into(),
            options: vec!["Stop it".into(), "Leave it running".into()],
            multi_select: false,
            option_details: Vec::new(),
        };
        match q.0.ask(&cx.session, &[ask], &cx.cancel) {
            Ok(a) if a.first().is_some_and(|a| a.starts_with("Stop")) => {}
            _ => return ToolOutcome::error(format!("{id} was left running: stopping another session's task needs the user's yes")),
        }
    }
    match tasks.0.stop(id) {
        Ok(t) => {
            let output = t.output_file.map(|f| format!(" Its output is in {f}.")).unwrap_or_default();
            ToolOutcome::ok(format!("{id} {}.{output}", t.status.as_str()), format!("{id} stopped"))
        }
        Err(e) => ToolOutcome::error(e),
    }
}

// ---- monitor

fn monitor_spec() -> marlowe_model::ToolSpec {
    marlowe_agent::compact_specs::spec("monitor")
}

fn monitor_consequence(tools: &BuiltinTools, call: &ToolCallReq, exposure: &Exposure) -> Consequence {
    // A re-arm without a command repeats one that was already allowed.
    if str_arg(&call.args, "command").is_none_or(|c| c.trim().is_empty()) {
        return Consequence::Free;
    }
    super::shell::command_consequence(tools, call, exposure)
}

// ---- schedule

fn schedule_spec() -> marlowe_model::ToolSpec {
    marlowe_agent::compact_specs::spec("schedule")
}

fn schedule_consequence(_t: &BuiltinTools, call: &ToolCallReq, _e: &Exposure) -> Consequence {
    if call.args.get("durable").and_then(Value::as_bool).unwrap_or(false) {
        let when = str_arg(&call.args, "cron").or_else(|| str_arg(&call.args, "at")).unwrap_or("?");
        let prompt = str_arg(&call.args, "prompt").unwrap_or("");
        return Consequence::Ask { summary: format!("schedule (survives restarts) {when}: {}", first_line(prompt, 80)) };
    }
    Consequence::Free
}

fn schedule_call(args: &Value, cx: &CallCtx) -> ToolOutcome {
    let Some(bg) = cx.scope.services.get::<marlowe_agent::Background>() else { return ToolOutcome::error(NO_DAEMON) };
    let prompt = str_arg(args, "prompt").map(str::trim).unwrap_or("");
    if prompt.is_empty() {
        return ToolOutcome::error("`prompt` is required: what to do when it fires");
    }
    let cron_text = str_arg(args, "cron").map(str::trim).filter(|s| !s.is_empty());
    let at_text = str_arg(args, "at").map(str::trim).filter(|s| !s.is_empty());
    let delay = int_arg(args, "delay_s");
    let durable = args.get("durable").and_then(Value::as_bool).unwrap_or(false);
    let mut req = CronRequest { prompt: prompt.to_string(), durable, tainted: cx.tainted, ..CronRequest::default() };
    let mut note = String::new();
    let mut woken = None;
    match (cron_text, at_text, delay) {
        (Some(c), None, None) => {
            let expr = match CronExpr::parse(c) {
                Ok(e) => e,
                Err(e) => return ToolOutcome::error(e),
            };
            if cron::next_fire_ms(&expr, now_ms()).is_none() {
                return ToolOutcome::error(format!("`{c}` never fires"));
            }
            req.cron = Some(expr.as_str().to_string());
        }
        (None, Some(t), None) => {
            let Some(at) = cron::parse_local_time(t) else {
                return ToolOutcome::error(format!("`at` must be a local time like 2026-10-12T09:00, not `{t}`"));
            };
            if at <= now_ms() {
                return ToolOutcome::error(format!("`{t}` is in the past"));
            }
            req.once_at_ms = Some(at);
        }
        (None, None, Some(d)) => {
            // A self-paced loop continues with this: call it again while there is more to do.
            let (secs, clamped) = clamp_wakeup(d);
            req.once_at_ms = Some(now_ms() + secs as i64 * 1000);
            req.wakeup = Some(str_arg(args, "reason").unwrap_or("").trim().to_string());
            woken = Some(secs);
            if clamped {
                note = format!(" (clamped from {d} s to the {secs} s limit)");
            }
        }
        _ => return ToolOutcome::error("give exactly one of `delay_s` (60-3600), `at` (local time) or `cron` (5 fields)"),
    }
    match bg.0.cron_create(&cx.session, req) {
        Ok(s) => {
            let when = s.next_at_ms.map(cron::describe_ms).unwrap_or_else(|| "unknown".into());
            let scope = if s.durable { "durable: survives restarts" } else { "dies with this session" };
            match woken {
                Some(secs) => ToolOutcome::ok(format!("Scheduled {}: you will be woken in {secs} s{note} ({scope}).", s.id), format!("wake in {secs} s")),
                None => ToolOutcome::ok(format!("Scheduled {}: next {when} ({scope}).", s.id), format!("{} scheduled", s.id)),
            }
        }
        Err(e) => ToolOutcome::error(e),
    }
}

// ---- watch

fn watch_spec() -> marlowe_model::ToolSpec {
    marlowe_agent::compact_specs::spec("watch")
}

fn watch_action(args: &Value) -> Result<WatchAction, String> {
    let path = str_arg(args, "path").map(str::trim).filter(|p| !p.is_empty());
    match str_arg(args, "op").or_else(|| str_arg(args, "action")).map(|a| a.trim().to_ascii_lowercase()).as_deref() {
        Some("list") => Ok(WatchAction::List),
        Some("add") => path.map(|p| WatchAction::Add(p.to_string())).ok_or_else(|| "`path` is required to add a watch".into()),
        Some("remove") => path.map(|p| WatchAction::Remove(p.to_string())).ok_or_else(|| "`path` is required to remove a watch".into()),
        _ => Err("`op` is add, remove or list".into()),
    }
}

fn watch_consequence(tools: &BuiltinTools, call: &ToolCallReq, _e: &Exposure) -> Consequence {
    // A watch only observes, but it is kept across sessions and it reads files: inside the
    // workspace it is free; anywhere else the user agrees to it.
    match watch_action(&call.args) {
        Ok(WatchAction::Add(p)) if !tools.inside_root(&p) => {
            Consequence::Ask { summary: format!("watch folder {}", first_line(&p, 100)) }
        }
        _ => Consequence::Free,
    }
}

fn watch_call(tools: &BuiltinTools, args: &Value, cx: &CallCtx) -> ToolOutcome {
    let Some(bg) = cx.scope.services.get::<marlowe_agent::Background>() else { return ToolOutcome::error(NO_DAEMON) };
    let action = match watch_action(args) {
        Ok(a) => a,
        Err(e) => return ToolOutcome::error(e),
    };
    // A relative path is the workspace's, as `watch_consequence` judged it, never the daemon's cwd.
    let root = tools.root_for(cx);
    let absolute = |p: &str| crate::paths::resolve(root.as_deref(), p).to_string_lossy().into_owned();
    let action = match action {
        WatchAction::Add(p) => WatchAction::Add(absolute(&p)),
        WatchAction::Remove(p) => WatchAction::Remove(absolute(&p)),
        WatchAction::List => WatchAction::List,
    };
    let verb = match &action {
        WatchAction::List => "watching",
        WatchAction::Add(p) => {
            if !std::path::Path::new(p).is_dir() {
                return ToolOutcome::error(format!("{p} is not a folder"));
            }
            "now watching"
        }
        WatchAction::Remove(_) => "after removing, watching",
    };
    match bg.0.watch(action) {
        Err(e) => ToolOutcome::error(e),
        Ok(state) => {
            if state.roots.is_empty() {
                return ToolOutcome::ok("The Sentinel is watching no folders.", "none");
            }
            let mut out = format!("{verb} {} folder{}:", state.roots.len(), if state.roots.len() == 1 { "" } else { "s" });
            for r in &state.roots {
                out.push_str(&format!("\n  {}", r.path));
                if r.runs > 0 {
                    out.push_str(&format!("  ({} run{})", r.runs, if r.runs == 1 { "" } else { "s" }));
                }
                if let Some(p) = &r.problem {
                    out.push_str(&format!("  [not watched: {p}]"));
                }
            }
            ToolOutcome::ok(out, format!("{} watched", state.roots.len()))
        }
    }
}

pub fn tools() -> Vec<ToolDef> {
    vec![
        ToolDef::new(task_list_spec(), ToolGroup::Tasks, Effects::READ, |_, a, cx| task_list_call(a, cx)).enabled(live),
        ToolDef::new(task_output_spec(), ToolGroup::Tasks, Effects::READ, task_output_call)
            .describe(|c| describe_by(c, "id"))
            .also_with(ToolGroup::Shell)
            .enabled(job_or_live),
        ToolDef::new(
            task_stop_spec(),
            ToolGroup::Tasks,
            Effects { reads: true, writes: false, runs: true, network: false },
            task_stop_call,
        )
        .describe(|c| describe_by(c, "id"))
        .also_with(ToolGroup::Shell)
        .enabled(job_or_live),
        ToolDef::new(monitor_spec(), ToolGroup::Tasks, Effects::RUN, |t, a, cx| crate::monitor::start(t, a, cx))
            .path_args(&["cwd"])
            .consequence(monitor_consequence)
            .crew_consequence(super::shell::crew_shell_consequence)
            .describe(|c| describe_by(c, "command"))
            .enabled(live),
        ToolDef::new(schedule_spec(), ToolGroup::Tasks, Effects::RUN, |_, a, cx| schedule_call(a, cx))
            .consequence(schedule_consequence)
            .describe(|c| describe_by(c, "prompt"))
            .enabled(live),
        ToolDef::new(watch_spec(), ToolGroup::Tasks, Effects::READ, |t, a, cx| watch_call(t, a, cx))
            .consequence(watch_consequence)
            .describe(|c| describe_by(c, "path"))
            .enabled(live),
    ]
}

/// Loop middleware for this group: none on the host; the daemon's injection drain is its own.
pub fn middleware(tools: &BuiltinTools) -> Vec<Arc<dyn marlowe_agent::ToolMiddleware>> {
    super::no_middleware(tools)
}

/// No block: each tool's description says what it is for.
pub fn persona_block() -> Option<&'static str> {
    None
}

/// Turns the group on for a host the binary builds (the conductor's).
pub fn configure(tools: BuiltinTools, _setup: &super::HostSetup<'_>) -> BuiltinTools {
    tools.with_state(TasksGroup { enabled: true })
}

/// Every workspace: background work is part of coding. (`configure` is what turns the tools on,
/// so a daemon-less host never offers them.)
pub fn detect(root: Option<&std::path::Path>) -> bool {
    root.is_some()
}

#[cfg(test)]
mod tests;
