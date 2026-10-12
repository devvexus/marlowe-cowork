//! The compiled `marlowe -p` against the scripted provider (debug builds only: release builds do
//! not compile the scripted provider). Exit codes are read from the real process, not a function.
#![cfg(debug_assertions)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{json, Value};

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("marlowe-bin-{tag}-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Env {
    home: PathBuf,
    ws: PathBuf,
    script: PathBuf,
}

fn env(tag: &str, rules: Value) -> Env {
    let root = dir(tag);
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let script = root.join("script.json");
    std::fs::write(&script, json!({ "rules": rules }).to_string()).unwrap();
    Env { home: root.join("home"), ws, script }
}

fn marlowe(e: &Env, args: &[&str], stdin: Option<&str>) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_marlowe"));
    c.args(args)
        .current_dir(&e.ws)
        .env("MARLOWE_HOME", &e.home)
        .env("MARLOWE_SCRIPTED_PROVIDER", &e.script)
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = c.spawn().unwrap();
    if let Some(text) = stdin {
        child.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
    }
    child.wait_with_output().unwrap()
}

fn print(e: &Env, extra: &[&str], prompt: &str) -> Output {
    let mut args = vec!["-p", "--standalone", "--no-memory", "--model", "ollama/scripted"];
    args.extend_from_slice(extra);
    args.push(prompt);
    marlowe(e, &args, None)
}

fn text(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap_or(-1)
}

#[test]
fn parity_h1_text_goes_to_stdout_alone_and_the_exit_code_is_0() {
    let e = env("text", json!([{"name": "any", "reply": "scripted hello"}]));
    let o = print(&e, &[], "hi");
    assert_eq!(code(&o), 0, "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(text(&o).trim(), "scripted hello");
}

#[test]
fn parity_h1_stdin_is_appended_to_the_prompt() {
    let e = env("stdin", json!([{"name": "seen", "last": ["PIPED-DATA"], "reply": "saw it"}, {"name": "no", "reply": "missed"}]));
    let o = marlowe(&e, &["-p", "--standalone", "--no-memory", "--model", "ollama/scripted", "review this:"], Some("PIPED-DATA\n"));
    assert_eq!(code(&o), 0, "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(text(&o).trim(), "saw it");
}

#[test]
fn parity_h2_json_is_one_object_and_stream_json_ends_with_a_result() {
    let e = env("json", json!([{"name": "any", "reply": "ok"}]));
    let o = print(&e, &["--output-format", "json"], "hi");
    let v: Value = serde_json::from_str(text(&o).trim()).unwrap_or_else(|err| panic!("{err}: {}", text(&o)));
    assert_eq!((v["result"].as_str(), v["is_error"].as_bool(), v["exit_code"].as_i64()), (Some("ok"), Some(false), Some(0)));

    let o = print(&e, &["--output-format", "stream-json"], "hi");
    let lines: Vec<Value> = text(&o).lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.first().unwrap()["subtype"], "init");
    assert_eq!(lines.last().unwrap()["type"], "result");
}

#[test]
fn parity_h3_a_usage_error_is_exit_1_and_names_the_flag() {
    let e = env("usage", json!([]));
    let o = marlowe(&e, &["-p", "--no-such-flag", "x"], None);
    assert_eq!(code(&o), 1);
    assert!(String::from_utf8_lossy(&o.stderr).contains("--no-such-flag"), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(text(&o).is_empty(), "stdout stays clean");
}

fn push_script() -> Value {
    json!([
        {"name": "call", "tool_results": 0, "reply": "trying", "calls": [{"name": "bash", "args": {"command": "git push origin main"}}]},
        {"name": "after", "reply": "I could not push, so I stopped"}
    ])
}

#[test]
fn parity_h3_fail_on_ask_is_exit_2() {
    let e = env("ask", push_script());
    let o = print(&e, &["--fail-on-ask", "--output-format", "json"], "push it");
    let v: Value = serde_json::from_str(text(&o).trim()).unwrap_or_else(|err| panic!("{err}: {} / {}", text(&o), String::from_utf8_lossy(&o.stderr)));
    assert_eq!(code(&o), 2, "{v}");
    assert_eq!(v["subtype"], "error_approval_needed");
}

#[test]
fn parity_h3_dont_ask_denies_and_the_model_recovers_to_exit_0() {
    let e = env("dontask", push_script());
    let o = print(&e, &["--permission-mode", "dont_ask", "--output-format", "json"], "push it");
    assert_eq!(code(&o), 0, "{} / {}", text(&o), String::from_utf8_lossy(&o.stderr));
    let v: Value = serde_json::from_str(text(&o).trim()).unwrap();
    assert_eq!(v["result"], "I could not push, so I stopped");
    assert_eq!(v["permission_denials"].as_array().unwrap().len(), 1, "{v}");
}

#[test]
fn parity_x15_the_mcp_subcommand_writes_the_files_and_lists_them() {
    let e = env("mcp", json!([]));
    let o = marlowe(&e, &["mcp", "add", "-s", "project", "files", "--", "node", "server.js"], None);
    assert_eq!(code(&o), 0, "{}", String::from_utf8_lossy(&o.stderr));
    assert!(Path::new(&e.ws).join(".mcp.json").is_file());
    let o = marlowe(&e, &["mcp", "list"], None);
    assert!(text(&o).contains("files: node server.js  [project stdio]"), "{}", text(&o));
    let o = marlowe(&e, &["mcp", "remove", "files"], None);
    assert_eq!(code(&o), 0);
}

#[test]
fn parity_u13_doctor_json_lists_the_report_fields_without_a_key_value() {
    let e = env("doctor", json!([]));
    let o = marlowe(&e, &["doctor", "--json"], None);
    let v: Value = serde_json::from_str(&text(&o)).unwrap_or_else(|err| panic!("{err}: {}", text(&o)));
    let names: Vec<&str> = v["checks"].as_array().unwrap().iter().filter_map(|c| c["name"].as_str()).collect();
    for want in ["install", "daemon", "openrouter key", "ollama", "rust-analyzer", "pyright", "gh", "docker", "git"] {
        assert!(names.contains(&want), "{want} in {names:?}");
    }
}

fn init_servers(o: &Output) -> Vec<String> {
    let first: Value = serde_json::from_str(text(o).lines().next().unwrap_or("{}")).unwrap_or_else(|err| panic!("{err}: {} / {}", text(o), String::from_utf8_lossy(&o.stderr)));
    assert_eq!(first["subtype"], "init", "{first}");
    let mut names: Vec<String> = first["mcp_servers"].as_array().unwrap().iter().filter_map(|s| s["name"].as_str().map(str::to_string)).collect();
    names.sort();
    names
}

#[test]
fn parity_h4_strict_mcp_config_leaves_the_servers_of_the_home_out_of_the_session() {
    let e = env("strict", json!([{"name": "any", "reply": "ok"}]));
    std::fs::create_dir_all(&e.home).unwrap();
    std::fs::write(e.home.join("mcp.json"), json!({"mcpServers": {"userside": {"command": "marlowe-no-such-program"}}}).to_string()).unwrap();
    let cfg = json!({"mcpServers": {"runside": {"command": "marlowe-no-such-program"}}}).to_string();

    let o = print(&e, &["--output-format", "stream-json", "--mcp-config", &cfg], "hi");
    assert_eq!(code(&o), 0, "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(init_servers(&o), ["runside", "userside"], "without --strict-mcp-config both are there");

    let o = print(&e, &["--output-format", "stream-json", "--strict-mcp-config", "--mcp-config", &cfg], "hi");
    assert_eq!(code(&o), 0, "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(init_servers(&o), ["runside"], "strict: the run's servers alone");
}

/// The shipped daemon applies the per-run tool filters and the appended prompt, so the flags run
/// (a daemon that does not is `tests_flags.rs`'s fake, where they are refused, not ignored).
#[test]
fn parity_h4_the_flags_the_daemon_applies_are_accepted() {
    let e = env("applied", json!([{"name": "any", "reply": "ok"}]));
    for flag in [["--disallowed-tools", "Write"], ["--append-system-prompt", "be terse"]] {
        let o = print(&e, &flag, "hi");
        assert_eq!(code(&o), 0, "{flag:?}: {}", String::from_utf8_lossy(&o.stderr));
        assert_eq!(text(&o).trim(), "ok");
    }
}
