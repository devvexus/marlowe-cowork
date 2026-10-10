//! The shell group (track B): bash, powershell, python, and their runner ([`run_shell`],
//! [`shell_outcome`]). Every call runs through the host's workspace port
//! (`BuiltinTools::workspace_port().exec`), so K's `DockerExec` runs them in its container; on
//! this machine that is `marlowe_os::child` (scrubbed environment, kill-on-close Job Object).
//! B adds `run_in_background`, job_output / job_watch / job_kill (`marlowe-exec/src/jobs/` over
//! `Workspace::spawn`, job table in `BuiltinTools::state`) and the failure-aware summary
//! (`summarize/`). E's shell parser and tripwire replace the crew's `classify_shell` behind the
//! same `crew_consequence` hook.

use std::time::Duration;

use marlowe_agent::{CallCtx, Consequence, Effects, Exposure, Mutation, SandboxView, ToolCallReq, ToolCtx, ToolGroup, ToolOutcome};
use marlowe_model::ToolSpec;
use marlowe_os::workspace::ExecRequest;
use serde_json::{json, Value};

use super::{describe_by, spec, ToolDef};
use crate::shell::{self, ShellLimits, ShellRun};
use crate::{first_line, int_arg, more_lines, paths, str_arg, text, BuiltinTools, Interpreter};

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
    ]
}

/// Runs a bash, powershell or python call to completion through the host's workspace port.
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
    let mut limits = tools.limits;
    if let Some(ms) = int_arg(args, "timeout_ms").filter(|m| *m > 0) {
        limits = limits.with_timeout(ms as u64);
    }
    let mut req = ExecRequest::from_command(&cmd);
    req.spec.env_remove.extend(tools.scrub.iter().cloned());
    req.spec.env.extend(tools.child_env.iter().cloned());
    req.spec.cwd = Some(dir);
    req.timeout = Duration::from_millis(limits.timeout_ms);
    req.max_output_bytes = limits.max_output_bytes;
    match tools.workspace_port().exec(&req, &|| cx.cancel.is_cancelled()) {
        Err(e) => ToolOutcome::error(format!("could not start {}: {e}", kind.tool_name())),
        Ok(out) => {
            let mut run = ShellRun::from_exec(out);
            tools.held.settle(&mut run, tools.leftovers);
            shell_outcome(run, limits)
        }
    }
}

/// The model-facing result of a finished shell run: output (head and tail past the cap), what
/// stopped it, and a one-line summary.
pub(crate) fn shell_outcome(run: ShellRun, limits: ShellLimits) -> ToolOutcome {
    let mut out = if run.text.trim().is_empty() {
        "(no output)".to_string()
    } else {
        text::head_and_tail(&run.text, shell::MAX_SHELL_RESULT_BYTES)
    };
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
            "\n[the harness stopped this command after it produced more than {} bytes on one \
             stream. The output above is a prefix; the command did not finish.]",
            limits.max_output_bytes
        ));
        summary = "output cap".to_string();
    } else if run.code != 0 {
        out.push_str(&format!("\n[exit {}]", run.code));
        summary = format!("exit {}", run.code);
    } else {
        summary = format!("{} lines", run.text.lines().count());
    }
    let failed = run.code != 0 || run.timed_out || run.flooded || run.cancelled;
    ToolOutcome { content: out, is_error: failed, summary, parts: Vec::new() }
}

/// Shells always ask here (E's rules and modes decide in front of this).
fn command_consequence(_tools: &BuiltinTools, call: &ToolCallReq, _exposure: &Exposure) -> Consequence {
    let command = str_arg(&call.args, "command").unwrap_or("");
    Consequence::Ask { summary: format!("{}: {}{}", call.name, first_line(command, 100), more_lines(command)) }
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
fn crew_shell_consequence(
    _tools: &BuiltinTools,
    call: &ToolCallReq,
    exposure: &Exposure,
    sandbox: &dyn SandboxView,
) -> Consequence {
    match sandbox.classify_shell(str_arg(&call.args, "command").unwrap_or(""), exposure.tainted) {
        Consequence::Free => Consequence::Free,
        Consequence::Ask { summary } => Consequence::Ask { summary: format!("{}: {summary}", call.name) },
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
        "Run a command in bash (Git Bash on Windows). stdin is closed; output is bounded; the \
         command is killed at the timeout (default 120000 ms).",
        json!({
            "command": {"type": "string"},
            "cwd": {"type": "string", "description": "Working directory; default is the workspace."},
            "timeout_ms": {"type": "integer"}
        }),
        &["command"],
    )
}

fn powershell_spec() -> ToolSpec {
    spec(
        "powershell",
        "Run a command in Windows PowerShell (-NoProfile -NonInteractive). stdin is closed; \
         output is bounded; the command is killed at the timeout (default 120000 ms).",
        json!({
            "command": {"type": "string"},
            "cwd": {"type": "string"},
            "timeout_ms": {"type": "integer"}
        }),
        &["command"],
    )
}

fn python_spec() -> ToolSpec {
    spec(
        "python",
        "Run Python with the user's interpreter: either an inline snippet (`code`) or a script \
         file (`path`). Give exactly one. Output is bounded; killed at the timeout.",
        json!({
            "code": {"type": "string", "description": "Inline source to run."},
            "path": {"type": "string", "description": "Script file to run."},
            "args": {"type": "array", "items": {"type": "string"}, "description": "Command-line arguments."},
            "cwd": {"type": "string"},
            "timeout_ms": {"type": "integer"}
        }),
        &[],
    )
}

/// Loop middleware for this group, built once per host next to its state (see `specs/mod.rs`).
pub fn middleware(tools: &crate::BuiltinTools) -> Vec<std::sync::Arc<dyn marlowe_agent::ToolMiddleware>> {
    super::no_middleware(tools)
}

/// This group's `persona/blocks/<group>.md` (`include_str!`) once it exists; appended only while
/// the group is exposed and has a live tool.
pub fn persona_block() -> Option<&'static str> {
    None
}

/// Per-host setup from the profile home (state via `BuiltinTools::with_state`).
pub fn configure(tools: crate::BuiltinTools, setup: &super::HostSetup<'_>) -> crate::BuiltinTools {
    super::no_configure(tools, setup)
}

/// Whether a local model gets this group in `root`. Always exposed to local models (a core group).
pub fn detect(_root: Option<&std::path::Path>) -> bool {
    false
}
