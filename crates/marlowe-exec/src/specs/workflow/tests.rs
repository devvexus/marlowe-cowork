//! The `workflow` and `report_findings` tools (WF1, WF9, G13) with a fake launcher.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use marlowe_agent::{CallCtx, Consequence, ExposureProfile, SessionScope, Services, ToolCallReq, ToolCtx, ToolGroup, ToolHost};
use marlowe_model::CancelToken;
use marlowe_proto::SessionId;
use marlowe_workflow::service::{FindingsSink, LaunchRequest, Launched, Waited};
use marlowe_workflow::{Meta, WorkflowLauncher, Workflows};
use serde_json::{json, Value};

use crate::specs::HostSetup;
use crate::BuiltinTools;

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("marlowe-wftool-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn host(home: &Path, ws: &Path) -> BuiltinTools {
    let t = BuiltinTools::new(Some(ws.to_path_buf()));
    super::configure(t, &HostSetup { home, workspace: Some(ws) })
}

fn req(name: &str, args: Value) -> ToolCallReq {
    ToolCallReq { id: "c1".into(), name: name.into(), args }
}

#[derive(Default)]
struct FakeLauncher {
    launched: Mutex<Vec<LaunchRequest>>,
}

impl WorkflowLauncher for FakeLauncher {
    fn launch(&self, req: LaunchRequest) -> Result<Launched, String> {
        let name = req.name.clone().unwrap_or_else(|| "inline".into());
        self.launched.lock().unwrap().push(req);
        Ok(Launched { run_id: "r1".into(), script_path: "C:/h/workflows/runs/r1/script.star".into(), name, meta: Meta::default() })
    }

    fn wait(&self, _run: &str, _cancel: &CancelToken) -> Waited {
        Waited::Done { value: json!({"answer": 42}), tainted: false }
    }
}

fn cx(ws: &Path, services: Services) -> CallCtx {
    CallCtx::new(
        ToolCtx::new(SessionId("s1".into()), 1, Some(ws.to_path_buf()), CancelToken::new(), false),
        SessionScope { services, ..SessionScope::detached() },
    )
}

const SCRIPT: &str = "META = {\"name\": \"two\", \"description\": \"Two agents\", \"phases\": [\"a\", \"b\"]}\ndef main(args):\n    return [agent(\"x\"), agent(\"y\")]\n";

#[test]
fn parity_wf1_first_run_of_a_digest_asks_with_its_meta_then_is_free() {
    let (home, ws) = (temp("home1"), temp("ws1"));
    let t = host(&home, &ws);
    let call = req("workflow", json!({"script": SCRIPT}));
    match t.consequence(&call) {
        Consequence::Ask { summary } => {
            assert!(summary.contains("Two agents") && summary.contains("a → b") && summary.contains("first run"), "{summary}")
        }
        other => panic!("{other:?}"),
    }
    let p = marlowe_workflow::service::prepare(&marlowe_workflow::service::roots(&home, Some(&ws), vec![]), Some(SCRIPT), None, None).unwrap();
    marlowe_workflow::service::trust(&home, &p).unwrap();
    assert_eq!(t.consequence(&call), Consequence::Free, "an approved digest runs free");
    // Built-ins are trusted; a script that cannot run asks nothing (the call returns the errors).
    assert_eq!(t.consequence(&req("workflow", json!({"name": "code-review"}))), Consequence::Free);
    assert_eq!(t.consequence(&req("workflow", json!({"script": "def main(:"}))), Consequence::Free);
}

#[test]
fn parity_wf1_the_call_returns_the_run_id_at_once_or_waits_when_asked() {
    let (home, ws) = (temp("home2"), temp("ws2"));
    let t = host(&home, &ws);
    let fake = Arc::new(FakeLauncher::default());
    let services = Services::new().with(Workflows(fake.clone()));
    let out = t.call_scoped(&req("workflow", json!({"script": SCRIPT, "args": {"k": 1}})), &cx(&ws, services.clone()));
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("\"run_id\":\"r1\"") && out.content.contains("script_path"), "{}", out.content);
    let sent = fake.launched.lock().unwrap()[0].clone();
    assert_eq!(sent.args, json!({"k": 1}));
    assert!(sent.background);
    let out = t.call_scoped(&req("workflow", json!({"name": "code-review", "background": false})), &cx(&ws, services));
    assert!(out.content.contains("\"answer\": 42"), "{}", out.content);
}

#[test]
fn parity_wf1_validation_errors_come_back_and_nothing_runs() {
    let (home, ws) = (temp("home3"), temp("ws3"));
    let t = host(&home, &ws);
    let fake = Arc::new(FakeLauncher::default());
    let services = Services::new().with(Workflows(fake.clone()));
    let bad = "NAME = \"x\"\nMETA = {\"name\": NAME, \"description\": \"d\"}\ndef main(args):\n    return agent(\"never\")\n";
    let out = t.call_scoped(&req("workflow", json!({"script": bad, "validate_only": true})), &cx(&ws, services.clone()));
    assert!(out.is_error && out.content.contains("line 2") && out.content.contains("pure literal"), "{}", out.content);
    let ok = t.call_scoped(&req("workflow", json!({"script": SCRIPT, "validate_only": true})), &cx(&ws, services));
    assert!(!ok.is_error && ok.content.contains("\"description\":\"Two agents\""), "{}", ok.content);
    assert!(fake.launched.lock().unwrap().is_empty(), "validation launched nothing");
}

#[test]
fn parity_wf2_workflow_is_the_conductors_only() {
    let (home, ws) = (temp("home4"), temp("ws4"));
    let crew = super::crew_configure(BuiltinTools::new(Some(ws.clone())), &HostSetup { home: &home, workspace: Some(&ws) });
    let names: Vec<String> = crew.specs().into_iter().map(|s| s.name).collect();
    assert!(!names.contains(&"workflow".to_string()) && names.contains(&"report_findings".to_string()), "{names:?}");
    let conductor: Vec<String> = host(&home, &ws).specs().into_iter().map(|s| s.name).collect();
    assert!(conductor.contains(&"workflow".to_string()));
}

#[test]
fn parity_wf9_the_block_rides_only_with_the_live_group() {
    let (home, ws) = (temp("home5"), temp("ws5"));
    let with = host(&home, &ws);
    assert!(with.persona_blocks().iter().any(|b| b.text().contains("## Workflows")));
    let all_but: Vec<ToolGroup> = ToolGroup::ALL.iter().copied().filter(|g| *g != ToolGroup::Workflow).collect();
    let without = host(&home, &ws).with_exposure(ExposureProfile::Frontier, all_but);
    assert!(!without.persona_blocks().iter().any(|b| b.text().contains("## Workflows")));
    // The deferred schema carries the API on its own.
    let spec = with.specs().into_iter().find(|s| s.name == "workflow").unwrap();
    assert!(spec.description.contains("pipeline(") && spec.description.contains("def main(args)"));
}

#[test]
fn parity_g13_invalid_findings_are_refused_with_a_retry_message_and_valid_ones_stored() {
    let (home, ws) = (temp("home6"), temp("ws6"));
    let t = host(&home, &ws);
    let seen = Arc::new(Mutex::new(0usize));
    let s2 = seen.clone();
    let services = Services::new().with(FindingsSink(Arc::new(move |_s, f| *s2.lock().unwrap() += f.len())));
    let bad = t.call_scoped(&req("report_findings", json!({"findings": [{"lens": "x", "severity": "urgent", "file": "a", "claim": "c", "evidence": "e"}]})), &cx(&ws, services.clone()));
    assert!(bad.is_error && bad.content.contains("send the whole list again") && bad.content.contains("severity"), "{}", bad.content);
    let good = json!({"findings": [{"lens": "correctness", "severity": "high", "file": "src/a.rs", "line": 3, "claim": "off by one", "evidence": "i <= n"}]});
    let ok = t.call_scoped(&req("report_findings", good), &cx(&ws, services));
    assert!(!ok.is_error, "{}", ok.content);
    assert_eq!(*seen.lock().unwrap(), 1, "the panel was told");
    assert_eq!(marlowe_workflow::findings::load(&home, "s1").len(), 1);
    assert_eq!(super::findings_key("crew:abc"), "run-abc");
}
