//! The shell group (track B): bash, powershell, python, `job_output` / `job_watch` / `job_kill`,
//! and the runner ([`run_shell`], [`shell_outcome`]).
//!
//! Every call starts through the host's workspace port, so K's `DockerExec` runs them in its
//! container; on this machine that is `marlowe_os::child` (scrubbed environment, kill-on-close
//! Job Object). A call is a job (`marlowe-exec/src/jobs/`): a foreground call waits for it and
//! can be detached with Ctrl+B, `run_in_background` answers with the job's id at once. The model
//! sees at most 30 KB of output, chosen failure-first (`summarize/`), with the whole log on disk.
//! E's shell parser and tripwire replace the crew's `classify_shell` behind the same
//! `crew_consequence` hook.

use std::sync::Arc;
use std::time::Duration;

use marlowe_agent::{
    CallCtx, Consequence, Effects, Exposure, Mutation, SandboxView, ToolCallReq, ToolCtx, ToolGroup, ToolMiddleware,
    ToolOutcome,
};
use marlowe_model::ToolSpec;
use marlowe_os::workspace::ExecRequest;
use serde_json::{json, Value};

use super::{describe_by, spec, ToolDef};
use crate::jobs::{self, FgEnd, FgRun, HostJobs, JobsMiddleware};
use crate::shell::{self, ShellLimits, ShellRun};
use crate::summarize;
use crate::{first_line, int_arg, more_lines, paths, str_arg, BuiltinTools, Interpreter};

/// Stopping your own job: a process action, but never a write to the user's files.
const KILL: Effects = Effects { reads: true, writes: false, runs: true, network: false };

pub fn tools() -> Vec<ToolDef> {
    vec![
        ToolDef::new(bash_spec(), ToolGroup::Shell, Effects::RUN, |t, a, c| run_shell(t, Interpreter::Bash, a, c))
            .path_args(&["cwd"])
            .consequence(command_consequence)
            .crew_consequence(crew_shell_consequence)
            .describe(|c| describe_by(c, "command"))
            .mutation(under_cwd),
        ToolDef::new(powershell_spec(), ToolGroup::Shell, Effects::RUN, |t, a, c| {
            run_shell(t, Interpreter::PowerShell, a, c)
        })
        .path_args(&["cwd"])
        .consequence(command_consequence)
        .crew_consequence(crew_shell_consequence)
        .describe(|c| describe_by(c, "command"))
        .mutation(under_cwd),
        ToolDef::new(python_spec(), ToolGroup::Shell, Effects::RUN, |t, a, c| run_shell(t, Interpreter::Python, a, c))
            .path_args(&["path", "cwd"])
            .consequence(python_consequence)
            .crew_consequence(crew_python_consequence)
            .describe(|c| describe_by(c, "code"))
            .mutation(under_cwd),
        ToolDef::new(job_output_spec(), ToolGroup::Shell, Effects::READ, jobs::job_output)
            .describe(|c| describe_by(c, "id")),
        ToolDef::new(job_watch_spec(), ToolGroup::Shell, Effects::READ, jobs::job_watch)
            .describe(|c| describe_by(c, "id")),
        ToolDef::new(job_kill_spec(), ToolGroup::Shell, KILL, jobs::job_kill).describe(|c| describe_by(c, "id")),
    ]
}

/// A flag argument, as a bool or `"true"`.
fn flag(args: &Value, key: &str) -> bool {
    args.get(key).is_some_and(|v| match v {
        Value::Bool(b) => *b,
        Value::String(s) => s.trim().eq_ignore_ascii_case("true"),
        _ => false,
    })
}

/// What the model is shown of a finished run.
struct OutputOpts {
    /// `raw: true`: cut the middle instead of choosing failure lines.
    raw: bool,
    /// The run's log on disk, if it has one.
    log: Option<std::path::PathBuf>,
    /// Where to write a log when the run has none and the output was cut.
    spill: Option<Arc<HostJobs>>,
    /// The run's text is a condensed form of a big log; this is the log's true line count.
    total_lines: Option<usize>,
}

/// Runs a bash, powershell or python call through the host's workspace port: to completion, in
/// the background, or until the user detaches it.
pub(crate) fn run_shell(tools: &BuiltinTools, kind: Interpreter, args: &Value, cx: &CallCtx) -> ToolOutcome {
    // Where relative script paths and `cwd` resolve: the pinned shell dir, else the root.
    let root = tools.shell_root(cx);
    let built = match kind {
        Interpreter::Bash | Interpreter::PowerShell => {
            let Some(script) = str_arg(args, "command").filter(|c| !c.trim().is_empty()) else {
                return ToolOutcome::error("`command` is required");
            };
            if kind == Interpreter::Bash {
                shell::bash_command(script)
            } else {
                Ok(shell::powershell_command(script))
            }
        }
        Interpreter::Python => {
            let extra: Vec<String> = args
                .get("args")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            match (str_arg(args, "code"), str_arg(args, "path")) {
                (Some(code), None) => shell::python_command(shell::PythonTarget::Code(code), &extra),
                (None, Some(path)) => {
                    let abs = paths::resolve(root.as_deref(), path);
                    shell::python_command(shell::PythonTarget::Script(&abs.to_string_lossy()), &extra)
                }
                _ => return ToolOutcome::error("give exactly one of `code` or `path`"),
            }
        }
    };
    let cmd = match built {
        Ok(c) => c,
        Err(e) => return ToolOutcome::error(e.to_string()),
    };
    let dir = match str_arg(args, "cwd") {
        Some(c) => paths::resolve(root.as_deref(), c),
        None => root.clone().unwrap_or_else(|| paths::resolve(None, ".")),
    };
    if !dir.is_dir() {
        return ToolOutcome::error(format!("cwd {} is not a directory", dir.display()));
    }
    if let Some(pin) = tools.shell_dir.as_deref().filter(|p| !paths::inside(p, &dir)) {
        return ToolOutcome::error(format!(
            "cwd {} is outside the sandbox {}; commands run inside it",
            dir.display(),
            pin.display()
        ));
    }
    let explicit_timeout = int_arg(args, "timeout_ms").filter(|m| *m > 0);
    let mut limits = tools.limits;
    if let Some(ms) = explicit_timeout {
        limits = limits.with_timeout(ms as u64);
    }
    let mut req = ExecRequest::from_command(&cmd);
    req.spec.env_remove.extend(tools.scrub.iter().cloned());
    req.spec.env.extend(tools.child_env.iter().cloned());
    req.spec.cwd = Some(dir);
    req.timeout = Duration::from_millis(limits.timeout_ms);
    req.max_output_bytes = limits.max_output_bytes;

    let display = match kind {
        Interpreter::Python => match (str_arg(args, "code"), str_arg(args, "path")) {
            (Some(code), _) => format!("python: {}", first_line(code, 80)),
            (None, p) => format!("python {}", p.unwrap_or("")),
        },
        _ => first_line(str_arg(args, "command").unwrap_or(""), 100),
    };
    let jobs = tools.state::<HostJobs>();
    jobs.bind(tools);
    let raw = flag(args, "raw");

    if flag(args, "run_in_background") {
        let timeout = explicit_timeout.map(|_| Duration::from_millis(limits.timeout_ms));
        return match jobs::run_background(tools, &jobs, cx, req, display, timeout) {
            Ok(o) => o,
            Err(e) => ToolOutcome::error(format!("could not start {}: {e}", kind.tool_name())),
        };
    }

    match jobs::run_foreground(tools, &jobs, cx, req.clone(), display.clone(), limits) {
        Ok(fg) => foreground_outcome(tools, &jobs, fg, &display, limits, raw),
        // A port that cannot run background processes (a container without them) still runs the
        // call to completion; it just cannot be detached.
        Err(e) if e.kind() == std::io::ErrorKind::Unsupported => run_to_completion(tools, &jobs, kind, req, cx, limits, raw),
        Err(e) => ToolOutcome::error(format!("could not start {}: {e}", kind.tool_name())),
    }
}

fn foreground_outcome(
    tools: &BuiltinTools,
    jobs: &Arc<HostJobs>,
    fg: FgRun,
    display: &str,
    limits: ShellLimits,
    raw: bool,
) -> ToolOutcome {
    let FgRun { end, code, text, total_lines, log, job, leftover } = fg;
    let mut run = ShellRun {
        code,
        text,
        timed_out: end == FgEnd::TimedOut,
        flooded: end == FgEnd::Flooded,
        cancelled: end == FgEnd::Cancelled,
        leftover,
    };
    tools.held.settle(&mut run, tools.leftovers);
    if end == FgEnd::Detached {
        let body = summarize::summarize(&summarize::clean(&run.text), false, 8_000);
        let shown = if body.text.is_empty() { "(no output yet)".to_string() } else { body.text };
        let lines = total_lines.unwrap_or_else(|| run.text.lines().count());
        return ToolOutcome::ok(
            format!(
                "[moved to the background by the user as job {job}: `{display}`. It keeps running and you will be told when it \
                 exits. job_output / job_watch / job_kill take id \"{job}\".]\noutput so far:\n{shown}\n{}",
                summarize::pointer(&log, lines)
            ),
            format!("detached · {job}"),
        );
    }
    shell_outcome(run, limits, OutputOpts { raw, log: Some(log), spill: Some(Arc::clone(jobs)), total_lines })
}

/// The pre-job path: `Workspace::exec` to completion (no detach, no live log).
fn run_to_completion(
    tools: &BuiltinTools,
    jobs: &Arc<HostJobs>,
    kind: Interpreter,
    req: ExecRequest,
    cx: &CallCtx,
    limits: ShellLimits,
    raw: bool,
) -> ToolOutcome {
    match tools.workspace_port().exec(&req, &|| cx.cancel.is_cancelled()) {
        Err(e) => ToolOutcome::error(format!("could not start {}: {e}", kind.tool_name())),
        Ok(out) => {
            let mut run = ShellRun::from_exec(out);
            tools.held.settle(&mut run, tools.leftovers);
            shell_outcome(run, limits, OutputOpts { raw, log: None, spill: Some(Arc::clone(jobs)), total_lines: None })
        }
    }
}

/// The model-facing result of a finished shell run: at most 30 KB of output (failure-first, or
/// head and tail with `raw`), a pointer to the whole log when something was left out, what
/// stopped it, and a one-line summary.
fn shell_outcome(run: ShellRun, limits: ShellLimits, opts: OutputOpts) -> ToolOutcome {
    let failed = run.code != 0 || run.timed_out || run.flooded || run.cancelled;
    let cleaned = summarize::clean(&run.text);
    let shown = if opts.raw {
        summarize::middle_cut(&cleaned, summarize::BUDGET)
    } else {
        summarize::summarize(&cleaned, failed, summarize::BUDGET)
    };
    let mut out = if cleaned.trim().is_empty() { "(no output)".to_string() } else { shown.text };
    let total_lines = opts.total_lines.unwrap_or(shown.total_lines);
    // A condensed log has had lines left out whatever the summary kept of it.
    if shown.cut || opts.total_lines.is_some() {
        let log = opts.log.clone().or_else(|| opts.spill.as_ref().and_then(|j| j.spill_log(&run.text)));
        match log {
            Some(path) => out.push_str(&format!("\n{}", summarize::pointer(&path, total_lines))),
            None => out.push_str("\n[the rest of the output was not kept]"),
        }
    } else if let Some(path) = &opts.log {
        // Nothing was left out: the log adds nothing the result does not carry.
        let _ = std::fs::remove_file(path);
    }
    let summary;
    if run.cancelled {
        out.push_str("\n[the command was stopped because the turn was interrupted.]");
        summary = "interrupted".to_string();
    } else if run.timed_out {
        out.push_str(&format!(
            "\n[the harness stopped this command after {} ms. The output above is what it had \
             produced by then; the command did not finish.]",
            limits.timeout_ms
        ));
        summary = "timed out".to_string();
    } else if run.flooded {
        out.push_str(&format!(
            "\n[the harness stopped this command after it produced more than {} bytes of output. The \
             output above is a prefix; the command did not finish.]",
            limits.max_output_bytes
        ));
        summary = "output cap".to_string();
    } else if run.code != 0 {
        out.push_str(&format!("\n[exit {}]", run.code));
        summary = format!("exit {}", run.code);
    } else {
        summary = format!("{} lines", opts.total_lines.unwrap_or_else(|| run.text.lines().count()));
    }
    ToolOutcome { content: out, is_error: failed, summary, parts: Vec::new() }
}

/// Shells always ask here (E's rules and modes decide in front of this).
pub(super) fn command_consequence(_tools: &BuiltinTools, call: &ToolCallReq, _exposure: &Exposure) -> Consequence {
    let command = str_arg(&call.args, "command").unwrap_or("");
    let bg = if flag(&call.args, "run_in_background") { " (background)" } else { "" };
    Consequence::Ask { summary: format!("{}{bg}: {}{}", call.name, first_line(command, 100), more_lines(command)) }
}

fn python_consequence(_tools: &BuiltinTools, call: &ToolCallReq, _exposure: &Exposure) -> Consequence {
    let args = &call.args;
    Consequence::Ask {
        summary: match (str_arg(args, "code"), str_arg(args, "path")) {
            (Some(code), _) => format!("python: {}{}", first_line(code, 100), more_lines(code)),
            (None, Some(path)) => format!("python {}", first_line(path, 100)),
            _ => "python".to_string(),
        },
    }
}

/// In a crew sandbox a shell command is classified by the sandbox's policy with the run's taint.
pub(super) fn crew_shell_consequence(
    _tools: &BuiltinTools,
    call: &ToolCallReq,
    exposure: &Exposure,
    sandbox: &dyn SandboxView,
) -> Consequence {
    match sandbox.classify_shell(str_arg(&call.args, "command").unwrap_or(""), exposure.tainted) {
        Consequence::Free => Consequence::Free,
        Consequence::Ask { summary } => Consequence::Ask { summary: format!("{}: {summary}", call.name) },
        Consequence::AskEvery { summary } => Consequence::AskEvery { summary: format!("{}: {summary}", call.name) },
    }
}

/// In a crew sandbox python runs like a shell: its code (inline, or a script of at most 512 KiB
/// in the sandbox) is classified. After untrusted content it asks, as outside the sandbox.
fn crew_python_consequence(
    tools: &BuiltinTools,
    call: &ToolCallReq,
    exposure: &Exposure,
    sandbox: &dyn SandboxView,
) -> Consequence {
    if exposure.tainted {
        return tools.consequence_of(call, exposure);
    }
    let arg = |k: &str| str_arg(&call.args, k);
    let code = match (arg("code"), arg("path")) {
        (Some(code), _) => Some(code.to_string()),
        (None, Some(p)) => std::fs::read(sandbox.root().join(p))
            .ok()
            .filter(|b| b.len() <= 512 * 1024)
            .map(|b| String::from_utf8_lossy(&b).into_owned()),
        _ => None,
    };
    match code.map(|c| sandbox.classify_code(&c)) {
        Some(Consequence::Ask { summary }) => Consequence::Ask { summary: format!("python: {summary}") },
        Some(Consequence::AskEvery { summary }) => Consequence::AskEvery { summary: format!("python: {summary}") },
        _ => Consequence::Free,
    }
}

/// A command may change anything under its working directory.
fn under_cwd(tools: &BuiltinTools, call: &ToolCallReq, ctx: &ToolCtx) -> Mutation {
    let root = tools.shell_root(ctx);
    let dir = match str_arg(&call.args, "cwd") {
        Some(c) => paths::resolve(root.as_deref(), c),
        None => root.unwrap_or_else(|| paths::resolve(None, ".")),
    };
    Mutation::Under(dir)
}

fn bash_spec() -> ToolSpec {
    spec(
        "bash",
        "Run a command in bash (Git Bash on Windows). stdin is closed; the command is killed at the timeout \
         (default 120000 ms, at most 600000). At most 30 KB of output comes back: when there is more, the lines that \
         explain a failure and the end are kept, and `[full output: path, N lines]` names the whole log (read it with \
         `read` or `grep`); `raw` cuts the middle instead. For a server, a watcher or a long build set \
         `run_in_background`: you get a job id at once and are told when it exits.",
        shell_props(json!({"command": {"type": "string"}})),
        &["command"],
    )
}

fn powershell_spec() -> ToolSpec {
    spec(
        "powershell",
        "Run a command in Windows PowerShell (-NoProfile -NonInteractive). stdin is closed; the command is killed at \
         the timeout (default 120000 ms, at most 600000). At most 30 KB of output comes back, failure lines first, \
         with `[full output: path, N lines]` for the whole log; `raw` cuts the middle instead. `run_in_background` \
         returns a job id at once.",
        shell_props(json!({"command": {"type": "string"}})),
        &["command"],
    )
}

fn python_spec() -> ToolSpec {
    spec(
        "python",
        "Run Python with the user's interpreter: either an inline snippet (`code`) or a script file (`path`). Give \
         exactly one. Killed at the timeout; at most 30 KB of output comes back (see `bash`); `run_in_background` \
         returns a job id at once.",
        shell_props(json!({
            "code": {"type": "string", "description": "Inline source to run."},
            "path": {"type": "string", "description": "Script file to run."},
            "args": {"type": "array", "items": {"type": "string"}, "description": "Command-line arguments."}
        })),
        &[],
    )
}

/// The properties every shell tool shares, after its own.
fn shell_props(own: Value) -> Value {
    let mut props = own;
    let shared = json!({
        "cwd": {"type": "string", "description": "Working directory; default is the workspace."},
        "timeout_ms": {"type": "integer", "description": "Kill the command after this long (default 120000, at most 600000)."},
        "run_in_background": {"type": "boolean", "description": "Start it as a background job and return its id at once."},
        "description": {"type": "string", "description": "What the command does, in a few words."},
        "raw": {"type": "boolean", "description": "Cut a long output in the middle instead of choosing the lines about failures."}
    });
    if let (Some(p), Some(s)) = (props.as_object_mut(), shared.as_object()) {
        p.extend(s.clone());
    }
    props
}

fn job_output_spec() -> ToolSpec {
    spec(
        "job_output",
        "Read a background job's output. Without `since` you get what is new since your last read; `since` is a byte \
         offset (the `next since` of an earlier read) for an exact window. `filter` keeps only lines matching a regular \
         expression. The result ends with the job's status and the next offset.",
        json!({
            "id": {"type": "string", "description": "The job id, e.g. \"j3\"."},
            "since": {"type": "integer", "description": "Byte offset to read from."},
            "filter": {"type": "string", "description": "Regular expression; only matching lines are returned."}
        }),
        &["id"],
    )
}

fn job_watch_spec() -> ToolSpec {
    spec(
        "job_watch",
        "Follow a running background job: its new output lines reach you as notices between your steps while you work \
         on something else, until a line matches `until_regex` (then you are told which, and the watch ends) or the job \
         exits. `filter` limits it to matching lines. The job's exit is reported even without a watch.",
        json!({
            "id": {"type": "string", "description": "The job id, e.g. \"j3\"."},
            "until_regex": {"type": "string", "description": "Stop watching at the first line matching this."},
            "filter": {"type": "string", "description": "Only report lines matching this."}
        }),
        &["id"],
    )
}

fn job_kill_spec() -> ToolSpec {
    spec(
        "job_kill",
        "Stop a background job and everything it started.",
        json!({"id": {"type": "string", "description": "The job id, e.g. \"j3\"."}}),
        &["id"],
    )
}

/// Loop middleware for this group, built once per host next to its state (see `specs/mod.rs`):
/// job notices for the model, and the call id Ctrl+B names.
pub fn middleware(tools: &crate::BuiltinTools) -> Vec<Arc<dyn ToolMiddleware>> {
    let jobs = tools.state::<HostJobs>();
    jobs.bind(tools);
    jobs.note_delivering();
    vec![Arc::new(JobsMiddleware { jobs })]
}

/// This group's `persona/blocks/<group>.md` (`include_str!`) once it exists; appended only while
/// the group is exposed and has a live tool.
pub fn persona_block() -> Option<&'static str> {
    None
}

/// Per-host setup from the profile home: job logs live in `<home>\jobs`.
/// Tells the host which session it serves when it is built, so the user's `!` and Ctrl+B reach
/// this session's job table without a claim (track B's finding 2).
pub fn note_session(tools: &crate::BuiltinTools, session: &str) {
    tools.state::<HostJobs>().note_session(session);
}

pub fn configure(tools: crate::BuiltinTools, setup: &super::HostSetup<'_>) -> crate::BuiltinTools {
    // The logs are the harness's own files: reading one back to see what a summary left out is not an untrusted read.
    let jobs = setup.home.join("jobs");
    let tools = tools.with_read_root(jobs.clone()).with_state(HostJobs::new(jobs));
    // Registered now, not at the first turn: the user's `!` works in a session that has not run one.
    tools.state::<HostJobs>().bind(&tools);
    tools
}

/// Whether a local model gets this group in `root`. Always exposed to local models (a core group).
pub fn detect(_root: Option<&std::path::Path>) -> bool {
    false
}
