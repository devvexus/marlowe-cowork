//! The VS Code extension's real server code (`ide/vscode`, run by `node` over a scripted editor)
//! found through its lock file and used through the hub: rows U14 and U18 from the Marlowe side.
//!
//! Needs `node` and the extension built once (`cd ide/vscode && npm install && npm run build`).
//! Without them each test says so and returns; set `MARLOWE_REQUIRE_IDE_TESTS=1` to make that a
//! failure (the parity walk does).

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use marlowe_mcp::client::ConnectOpts;
use marlowe_mcp::config::Paths;
use marlowe_mcp::hub::{Hub, HubOpts, SlotStatus};
use marlowe_mcp::oauth::MemoryTokens;
use marlowe_os::child::{self, ChildProc, ChildSpec};
use serde_json::{json, Value};

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("marlowe-ide-it-{tag}-{}-{}", std::process::id(), marlowe_mcp::util::random_token(6)));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn devserver() -> Option<PathBuf> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ide/vscode/out/src/testing/devserver.js");
    p.is_file().then_some(p)
}

/// A running extension server over a scripted editor, stopped on drop.
struct Editor {
    proc: Option<ChildProc>,
    home: PathBuf,
    folder: PathBuf,
}

impl Drop for Editor {
    fn drop(&mut self) {
        if let Some(mut p) = self.proc.take() {
            drop(p.child_mut().stdin.take());
            p.kill_tree();
            let _ = p.child_mut().wait();
        }
    }
}

fn start_editor(tag: &str) -> Option<Editor> {
    let required = std::env::var("MARLOWE_REQUIRE_IDE_TESTS").is_ok_and(|v| v == "1");
    let skip = |why: &str| {
        assert!(!required, "MARLOWE_REQUIRE_IDE_TESTS=1 and {why}");
        eprintln!("skipped: {why}");
        None
    };
    let Some(script) = devserver() else { return skip("ide/vscode/out is not built (cd ide/vscode && npm install && npm run build)") };
    let (home, folder) = (tmp(&format!("{tag}-home")), tmp(&format!("{tag}-ws")));
    let mut cmd = Command::new("node");
    cmd.arg(&script).arg(&home).arg(&folder).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    let Ok(mut proc) = child::spawn(&mut cmd, &ChildSpec::new()) else { return skip("node is not on PATH") };
    let stdout = proc.child_mut().stdout.take().unwrap();
    let mut line = String::new();
    BufReader::new(stdout).read_line(&mut line).unwrap();
    let v: Value = serde_json::from_str(line.trim()).unwrap_or_else(|e| panic!("devserver said {line:?}: {e}"));
    assert!(Path::new(v["lockFile"].as_str().unwrap()).is_file());
    Some(Editor { proc: Some(proc), home, folder })
}

fn hub_for(ed: &Editor, workspace: &Path) -> Arc<Hub> {
    let opts = HubOpts::new(Paths::new(&ed.home, Some(workspace)), ConnectOpts::new(Arc::new(MemoryTokens::default())));
    let hub = Hub::start(opts);
    assert!(hub.wait_settled(Duration::from_secs(30)), "the editor did not settle");
    hub
}

fn no_cancel() -> bool {
    false
}

#[test]
fn parity_u14_the_editor_is_found_by_its_lock_file_and_its_tools_are_namespaced() {
    let Some(ed) = start_editor("found") else { return };
    let hub = hub_for(&ed, &ed.folder);
    let st = hub.statuses();
    assert_eq!(st.len(), 1, "{st:?}");
    assert_eq!((st[0].name.as_str(), &st[0].status, st[0].tools), ("ide", &SlotStatus::Connected, 6));
    let names: Vec<String> = hub.tools().into_iter().map(|t| t.exposed).collect();
    for t in ["get_context", "get_diagnostics", "open_file", "open_diff", "close_diffs", "execute_code"] {
        assert!(names.contains(&format!("mcp__ide__{t}")), "{t} in {names:?}");
    }
    // The extension publishes the same consequences this build decides on.
    let lock_file = std::fs::read_dir(ed.home.join("ide")).unwrap().next().unwrap().unwrap().path();
    let lock: Value = serde_json::from_str(&std::fs::read_to_string(lock_file).unwrap()).unwrap();
    for (tool, level) in lock["consequences"].as_object().unwrap() {
        assert_eq!(level, &json!(marlowe_mcp::ide::consequence_of(tool)), "{tool}");
    }
}

#[test]
fn parity_u14_reads_are_free_and_running_code_asks() {
    let Some(ed) = start_editor("asks") else { return };
    let hub = hub_for(&ed, &ed.folder);
    let asks = |t: &str| hub.find(&format!("mcp__ide__{t}")).unwrap().asks();
    assert!(!asks("get_context") && !asks("get_diagnostics") && !asks("open_file") && !asks("open_diff") && !asks("close_diffs"));
    assert!(asks("execute_code"), "a notebook cell runs code on the machine");
}

#[test]
fn parity_u14_the_selection_diagnostics_and_a_diff_come_through_the_hub() {
    let Some(ed) = start_editor("tools") else { return };
    let hub = hub_for(&ed, &ed.folder);
    let ctx: Value = serde_json::from_str(&hub.call("mcp__ide__get_context", json!({}), &no_cancel).unwrap().text).unwrap();
    assert_eq!(ctx["selection"]["text"], "x = 1");
    assert_eq!(ctx["selection"]["startLine"], 5);
    assert_eq!(ctx["openEditors"].as_array().unwrap().len(), 1);
    let d = hub.call("mcp__ide__get_diagnostics", json!({}), &no_cancel).unwrap();
    assert!(d.text.contains("main.py:7:1 error [pyright reportUndefinedVariable] Name `y` is not defined"), "{}", d.text);
    let diff = hub.call("mcp__ide__open_diff", json!({"path": "main.py", "new_contents": "x = 2\n"}), &no_cancel).unwrap();
    assert!(!diff.is_error && diff.text.contains("is open in the editor"), "{}", diff.text);
    let outside_path = if cfg!(windows) { "C:\\Windows\\win.ini" } else { "/etc/hostname" };
    let outside = hub.call("mcp__ide__open_file", json!({"path": outside_path}), &no_cancel).unwrap();
    assert!(outside.is_error && outside.text.contains("outside the open workspace"), "{}", outside.text);
}

#[test]
fn parity_u18_execute_code_returns_text_and_an_image_for_the_hub() {
    let Some(ed) = start_editor("exec") else { return };
    let hub = hub_for(&ed, &ed.folder);
    let two = hub.call("mcp__ide__execute_code", json!({"code": "1+1"}), &no_cancel).unwrap();
    assert_eq!((two.text.as_str(), two.is_error, two.images.len()), ("2", false, 0));
    let plot = hub.call("mcp__ide__execute_code", json!({"code": "plot()"}), &no_cancel).unwrap();
    assert_eq!(plot.images.len(), 1, "{}", plot.text);
    assert_eq!(plot.images[0].0, "image/png");
    assert!(!plot.images[0].1.is_empty());
    let boom = hub.call("mcp__ide__execute_code", json!({"code": "boom()"}), &no_cancel).unwrap();
    assert!(boom.is_error && boom.text.contains("ValueError"), "{}", boom.text);
}

#[test]
fn parity_u14_the_start_of_a_turn_gets_the_selection_as_a_note() {
    let Some(ed) = start_editor("note") else { return };
    let hub = hub_for(&ed, &ed.folder);
    let note = marlowe_mcp::ide::context_note(&hub, Duration::from_secs(5)).expect("a note");
    assert!(note.contains("main.py") && note.contains("x = 1") && note.contains("selected"), "{note}");
    // No editor connected: no note, and no wait.
    let elsewhere = tmp("nonote");
    let none = Hub::start(HubOpts::new(Paths::new(&ed.home, Some(elsewhere.as_path())), ConnectOpts::new(Arc::new(MemoryTokens::default()))));
    let t0 = std::time::Instant::now();
    assert!(marlowe_mcp::ide::context_note(&none, Duration::from_secs(5)).is_none());
    assert!(t0.elapsed() < Duration::from_secs(1));
}

#[test]
fn a_workspace_the_editor_does_not_have_open_gets_no_ide_server() {
    let Some(ed) = start_editor("other") else { return };
    let elsewhere = tmp("elsewhere");
    let opts = HubOpts::new(Paths::new(&ed.home, Some(elsewhere.as_path())), ConnectOpts::new(Arc::new(MemoryTokens::default())));
    let hub = Hub::start(opts);
    assert!(hub.wait_settled(Duration::from_secs(5)));
    assert!(hub.is_empty(), "{:?}", hub.statuses());
}

#[test]
fn strict_mode_leaves_the_editor_out() {
    let Some(ed) = start_editor("strict") else { return };
    let mut opts = HubOpts::new(Paths::new(&ed.home, Some(ed.folder.as_path())), ConnectOpts::new(Arc::new(MemoryTokens::default())));
    opts.strict = true;
    let hub = Hub::start(opts);
    assert!(hub.wait_settled(Duration::from_secs(5)));
    assert!(hub.is_empty(), "--strict-mcp-config means only the run's own servers");
}
