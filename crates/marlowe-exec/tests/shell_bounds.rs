//! Shell tools are bounded in time and memory, honour interrupts, and run where they are told.

mod common;

use std::time::{Duration, Instant};

use common::*;
use marlowe_agent::ToolHost;
use marlowe_exec::{BuiltinTools, ShellLimits};
use serde_json::json;

fn fast(ws: &Ws) -> BuiltinTools {
    ws.tools().with_shell_limits(ShellLimits { timeout_ms: 600, max_output_bytes: 64 * 1024 })
}

#[test]
fn a_command_that_never_exits_is_stopped_and_the_call_returns() {
    let ws = Ws::new("sb-forever");
    let t = fast(&ws);
    let started = Instant::now();
    let o = call(&t, "bash", json!({"command": "sleep 30"}));
    assert!(started.elapsed() < Duration::from_secs(10), "took {:?}", started.elapsed());
    assert!(o.is_error);
    assert!(o.content.contains("harness stopped this command"), "{}", o.content);
    assert_eq!(o.summary, "timed out");
}

#[test]
fn the_per_call_timeout_is_read() {
    let ws = Ws::new("sb-timeout-arg");
    let t = ws.tools();
    let started = Instant::now();
    let o = call(&t, "bash", json!({"command": "sleep 30", "timeout_ms": 500}));
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(o.summary, "timed out");
}

#[test]
fn three_unbounded_commands_in_a_row_all_return() {
    let ws = Ws::new("sb-three");
    let t = fast(&ws);
    for _ in 0..3 {
        assert_eq!(call(&t, "bash", json!({"command": "sleep 30"})).summary, "timed out");
    }
}

#[test]
fn a_command_that_floods_is_capped_and_says_so() {
    let ws = Ws::new("sb-flood");
    let t = ws.tools().with_shell_limits(ShellLimits { timeout_ms: 20_000, max_output_bytes: 32 * 1024 });
    let started = Instant::now();
    let o = call(&t, "bash", json!({"command": "yes xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}));
    assert!(started.elapsed() < Duration::from_secs(15), "took {:?}", started.elapsed());
    assert!(o.is_error);
    assert!(o.content.contains("produced more than 32768 bytes"), "{}", &o.content[o.content.len().saturating_sub(300)..]);
    assert!(o.content.len() < 40_000, "result is bounded: {}", o.content.len());
}

#[test]
fn an_ordinary_command_finishes_normally() {
    let ws = Ws::new("sb-ok");
    let o = call(&ws.tools(), "bash", json!({"command": "echo hello; echo world"}));
    assert!(!o.is_error, "{}", o.content);
    assert_eq!(o.content.lines().collect::<Vec<_>>(), ["hello", "world"]);
    assert_eq!(o.summary, "2 lines");
    let o = call(&ws.tools(), "bash", json!({"command": "true"}));
    assert_eq!(o.content, "(no output)");
}

#[test]
fn stderr_and_a_failing_exit_code_both_survive() {
    let ws = Ws::new("sb-fail");
    let o = call(&ws.tools(), "bash", json!({"command": "echo out; echo err 1>&2; exit 3"}));
    assert!(o.is_error);
    assert!(o.content.contains("out") && o.content.contains("err"));
    assert!(o.content.contains("[exit 3]"));
    assert_eq!(o.summary, "exit 3");
}

#[test]
fn a_command_that_backgrounds_a_child_still_returns_promptly() {
    let ws = Ws::new("sb-bg");
    let started = Instant::now();
    let o = call(&fast(&ws), "bash", json!({"command": "sleep 20 >/dev/null 2>&1 & echo started"}));
    assert!(started.elapsed() < Duration::from_secs(10), "took {:?}", started.elapsed());
    assert!(o.content.contains("started"), "{}", o.content);
}

#[test]
fn the_process_tree_is_killed_with_the_shell() {
    let ws = Ws::new("sb-tree");
    let marker = ws.path("alive.txt");
    let t = fast(&ws);
    // The grandchild would write the marker after the timeout if it survived.
    let cmd = format!("(sleep 2; echo up > '{}') & sleep 30", marker.to_string_lossy().replace('\\', "/"));
    let o = call(&t, "bash", json!({"command": cmd}));
    assert_eq!(o.summary, "timed out");
    std::thread::sleep(Duration::from_millis(3_500));
    assert!(!marker.exists(), "the grandchild outlived the shell");
}

#[test]
fn an_interrupt_stops_a_running_command() {
    let ws = Ws::new("sb-cancel");
    let t = ws.tools();
    let c = ctx(Some(&ws.root));
    let cancel = c.cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(400));
        cancel.cancel();
    });
    let started = Instant::now();
    let o = t.call(&req("bash", json!({"command": "sleep 30"})), &c);
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(o.summary, "interrupted");
}

#[test]
fn bash_starts_in_the_workspace_and_cwd_overrides_it() {
    let ws = Ws::new("sb-cwd");
    ws.put("sub/marker.txt", "x");
    let t = ws.tools();
    let o = call(&t, "bash", json!({"command": "ls"}));
    assert!(o.content.contains("sub"), "{}", o.content);
    let o = call(&t, "bash", json!({"command": "ls", "cwd": "sub"}));
    assert_eq!(o.content.trim(), "marker.txt");
    let o = call(&t, "bash", json!({"command": "ls", "cwd": "missing"}));
    assert!(o.is_error && o.content.contains("not a directory"));
}

#[test]
fn quotes_in_a_command_survive_to_the_shell() {
    let ws = Ws::new("sb-quotes");
    let o = call(&ws.tools(), "bash", json!({"command": "echo \"hello world\" 'single q'"}));
    assert_eq!(o.content.trim(), "hello world single q");
}

#[test]
fn credential_variables_are_removed_from_every_child() {
    let ws = Ws::new("sb-scrub");
    // Only this test touches these variables.
    std::env::set_var("OPENROUTER_API_KEY", "sk-test-secret");
    std::env::set_var("HF_TOKEN", "hf-test-secret");
    let t = ws.tools();
    let o = call(&t, "bash", json!({"command": "echo \"[${OPENROUTER_API_KEY:-unset}] [${HF_TOKEN:-unset}]\""}));
    assert_eq!(o.content.trim(), "[unset] [unset]");
    let o = call(&t, "powershell", json!({"command": "Write-Output \"[$env:OPENROUTER_API_KEY]\""}));
    assert!(!o.content.contains("sk-test"), "{}", o.content);
    let o = call(&t, "python", json!({"code": "import os; print(os.environ.get('OPENROUTER_API_KEY', 'unset'))"}));
    assert_eq!(o.content.trim(), "unset", "{}", o.content);
}

#[cfg(windows)]
#[test]
fn powershell_runs_with_the_documented_flags() {
    let ws = Ws::new("sb-ps");
    let t = ws.tools();
    let o = call(&t, "powershell", json!({"command": "Write-Output (1+2); (Get-Location).Path"}));
    assert!(!o.is_error, "{}", o.content);
    assert!(o.content.starts_with("3"), "{}", o.content);
    assert!(o.content.contains(ws.root.file_name().unwrap().to_str().unwrap()), "cwd is the workspace");
    let o = call(&t, "powershell", json!({"command": "Write-Output 'héllo ✓'"}));
    assert!(o.content.contains("héllo ✓"), "utf-8 output: {}", o.content);
    let o = call(&t, "powershell", json!({"command": "exit 4"}));
    assert!(o.is_error && o.summary == "exit 4");
    let o = call(&t, "powershell", json!({"command": "Start-Sleep -Seconds 30", "timeout_ms": 700}));
    assert_eq!(o.summary, "timed out");
}

#[test]
fn python_runs_snippets_and_scripts() {
    let ws = Ws::new("sb-py");
    let t = ws.tools();
    let o = call(&t, "python", json!({"code": "print(6*7)"}));
    if o.is_error && o.content.contains("no python interpreter") {
        eprintln!("skipped: no python on this machine");
        return;
    }
    assert_eq!(o.content.trim(), "42", "{}", o.content);
    ws.put("s.py", "import sys, os\nprint('args', sys.argv[1:])\nprint(os.getcwd() == os.path.realpath(os.getcwd()))\n");
    let o = call(&t, "python", json!({"path": "s.py", "args": ["a", "b"]}));
    assert!(o.content.contains("args ['a', 'b']"), "{}", o.content);
    let o = call(&t, "python", json!({"code": "import sys; print('é✓'); sys.exit(2)"}));
    assert!(o.is_error && o.summary == "exit 2" && o.content.contains("é✓"), "{}", o.content);
    let o = call(&t, "python", json!({"code": "raise ValueError('boom')"}));
    assert!(o.is_error && o.content.contains("ValueError: boom"));
    assert!(call(&t, "python", json!({})).is_error);
    assert!(call(&t, "python", json!({"code": "1", "path": "s.py"})).is_error);
    let o = call(&t, "python", json!({"code": "import time; time.sleep(30)", "timeout_ms": 700}));
    assert_eq!(o.summary, "timed out");
}

/// The `bash` tool runs Git Bash, never the WSL launcher first on a default PATH: `uname` says
/// MINGW or MSYS, not Linux, and the process sees this machine's drive.
#[cfg(windows)]
#[test]
fn the_bash_tool_is_git_bash_not_the_wsl_launcher() {
    if marlowe_os::bash::git_bash().is_none() {
        eprintln!("skipped: Git Bash is not installed here");
        return;
    }
    let ws = Ws::new("sb-git-bash");
    let o = call(&ws.tools(), "bash", json!({"command": "uname -s"}));
    assert!(!o.is_error, "{}", o.content);
    let os = o.content.trim().to_ascii_uppercase();
    assert!(os.contains("MINGW") || os.contains("MSYS"), "bash ran outside Git Bash: {os}");
}

#[test]
fn a_missing_workspace_is_named_plainly_so_the_model_stops_guessing() {
    let ws = Ws::new("sb-missing-ws");
    let gone = ws.path("MSFFN");
    let t = marlowe_exec::BuiltinTools::new(Some(gone.clone()));
    for (tool, args) in [("bash", json!({"command": "echo hi"})), ("bash", json!({"command": "echo hi", "cwd": "sub"}))] {
        let o = call(&t, tool, args);
        assert!(o.is_error);
        let want = format!("this session's workspace {} does not exist — set it in the workspace picker", gone.display());
        assert!(o.content.contains(&want), "{}", o.content);
    }
    // A folder that exists under a real workspace keeps the ordinary wording.
    let t = ws.tools();
    let o = call(&t, "bash", json!({"command": "echo hi", "cwd": "nope"}));
    assert!(o.content.contains("is not a directory") && !o.content.contains("workspace"), "{}", o.content);
}
