//! Every shell child runs in a kill-on-close Job Object: a tree kill reaches orphaned
//! grandchildren, background processes follow the host's leftover policy, and crew hosts can
//! add environment and pin the working directory.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::*;
use marlowe_exec::{run_script, BuiltinTools, Interpreter, Leftovers, ShellLimits, SpawnOpts};
use serde_json::json;

fn fast(t: BuiltinTools) -> BuiltinTools {
    t.with_shell_limits(ShellLimits { timeout_ms: 1_500, max_output_bytes: 64 * 1024 })
}

fn posix(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

fn appears(p: &Path, within: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < within {
        if p.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    p.exists()
}

/// A bash command that leaves a background process writing `marker` after ~2 s.
fn background_writer(marker: &Path) -> String {
    format!("(sleep 2; echo up > '{}') >/dev/null 2>&1 & echo started", posix(marker))
}

#[test]
fn an_orphaned_grandchild_dies_on_timeout() {
    let ws = Ws::new("sj-orphan");
    let marker = ws.path("alive.txt");
    // The middle subshell exits at once, so the writer has no living parent in the tree.
    let cmd = format!("( (sleep 2; echo up > '{}') & ); sleep 30", posix(&marker));
    let o = call(&fast(ws.tools()), "bash", json!({"command": cmd}));
    assert_eq!(o.summary, "timed out", "{}", o.content);
    std::thread::sleep(Duration::from_millis(3_500));
    assert!(!marker.exists(), "the orphaned grandchild outlived the timeout");
}

#[cfg(windows)]
#[test]
fn a_powershell_started_process_dies_on_timeout() {
    let ws = Ws::new("sj-ps");
    let marker = ws.path("alive.txt");
    let cmd = format!(
        "Start-Process -WindowStyle Hidden cmd.exe -ArgumentList '/d /c ping -n 9 127.0.0.1 >nul & echo up> {}'; Start-Sleep -Seconds 30",
        marker.display()
    );
    let t = ws.tools().with_shell_limits(ShellLimits { timeout_ms: 3_000, max_output_bytes: 64 * 1024 });
    let o = call(&t, "powershell", json!({"command": cmd}));
    assert_eq!(o.summary, "timed out", "{}", o.content);
    // The started process would write ~8 s after launch.
    std::thread::sleep(Duration::from_millis(7_000));
    assert!(!marker.exists(), "the started process outlived the timeout");
}

#[test]
fn by_default_background_processes_outlive_the_call() {
    let ws = Ws::new("sj-release");
    let marker = ws.path("bg.txt");
    let t = fast(ws.tools());
    let o = call(&t, "bash", json!({"command": background_writer(&marker)}));
    assert!(o.content.contains("started"), "{}", o.content);
    assert_eq!(t.leftover_processes(), 0, "nothing is held by default");
    drop(t);
    assert!(appears(&marker, Duration::from_secs(15)), "the background process was killed");
}

#[test]
fn a_crew_host_kills_its_background_processes_when_dropped() {
    let ws = Ws::new("sj-held");
    let marker = ws.path("bg.txt");
    let t = fast(ws.tools()).with_leftovers(Leftovers::KillWithHost);
    let o = call(&t, "bash", json!({"command": background_writer(&marker)}));
    assert!(o.content.contains("started"), "{}", o.content);
    if cfg!(windows) {
        assert!(t.leftover_processes() >= 1, "the background process is held");
    }
    drop(t);
    std::thread::sleep(Duration::from_millis(3_500));
    assert!(!marker.exists(), "the background process outlived its host");
}

#[test]
fn kill_leftovers_stops_held_processes_now() {
    let ws = Ws::new("sj-kill");
    let marker = ws.path("bg.txt");
    let t = fast(ws.tools()).with_leftovers(Leftovers::KillWithHost);
    call(&t, "bash", json!({"command": background_writer(&marker)}));
    t.kill_leftovers();
    assert_eq!(t.leftover_processes(), 0);
    std::thread::sleep(Duration::from_millis(3_500));
    assert!(!marker.exists());
}

#[test]
fn child_env_reaches_every_interpreter_and_git_sees_the_no_push_url() {
    let ws = Ws::new("sj-env");
    let env = [
        ("GIT_CONFIG_COUNT".to_string(), "1".to_string()),
        ("GIT_CONFIG_KEY_0".to_string(), "remote.origin.pushurl".to_string()),
        ("GIT_CONFIG_VALUE_0".to_string(), "invalid://crew-cannot-push/".to_string()),
    ];
    let t = ws.tools().with_child_env(env);
    let o = call(&t, "bash", json!({"command": "echo \"[$GIT_CONFIG_KEY_0]\""}));
    assert_eq!(o.content.trim(), "[remote.origin.pushurl]");
    let o = call(&t, "powershell", json!({"command": "Write-Output \"[$env:GIT_CONFIG_COUNT]\""}));
    assert_eq!(o.content.trim(), "[1]", "{}", o.content);
    let o = call(&t, "bash", json!({"command": "git config --get remote.origin.pushurl"}));
    if o.content.contains("command not found") {
        eprintln!("skipped the git check: no git on PATH");
    } else {
        assert_eq!(o.content.trim(), "invalid://crew-cannot-push/", "{}", o.content);
    }
}

#[test]
fn a_pinned_shell_dir_is_the_default_cwd_and_bounds_cwd() {
    let ws = Ws::new("sj-pin");
    ws.put("sandbox/inner/x.txt", "x");
    ws.put("outside.txt", "o");
    let t = ws.tools().with_shell_dir(ws.path("sandbox"));
    let o = call(&t, "bash", json!({"command": "ls"}));
    assert_eq!(o.content.trim(), "inner", "{}", o.content);
    let o = call(&t, "bash", json!({"command": "ls", "cwd": "inner"}));
    assert_eq!(o.content.trim(), "x.txt", "{}", o.content);
    let o = call(&t, "bash", json!({"command": "ls", "cwd": ".."}));
    assert!(o.is_error && o.content.contains("outside the sandbox"), "{}", o.content);
    let o = call(&t, "bash", json!({"command": "ls", "cwd": ws.root.to_string_lossy()}));
    assert!(o.is_error && o.content.contains("outside the sandbox"), "{}", o.content);
}

#[test]
fn run_script_applies_cwd_env_and_scrub() {
    let ws = Ws::new("sj-script");
    ws.put("here.txt", "h");
    // Only this test touches this variable.
    std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-test-secret");
    let opts = SpawnOpts::scrubbed()
        .with_cwd(ws.root.clone())
        .with_env([("MARLOWE_CREW_TEST".to_string(), "yes".to_string())]);
    let limits = ShellLimits { timeout_ms: 20_000, max_output_bytes: 64 * 1024 };
    let run = run_script(
        Interpreter::Bash,
        "ls; echo \"[$MARLOWE_CREW_TEST] [${ANTHROPIC_API_KEY:-unset}]\"",
        &opts,
        limits,
        None,
    )
    .unwrap();
    assert_eq!(run.code, 0, "{}", run.text);
    assert!(run.text.contains("here.txt"), "{}", run.text);
    assert!(run.text.contains("[yes] [unset]"), "{}", run.text);
    assert!(run.leftover.is_none());
    let run = run_script(Interpreter::Bash, "sleep 30", &opts, ShellLimits { timeout_ms: 500, ..limits }, None)
        .unwrap();
    assert!(run.timed_out);
}
