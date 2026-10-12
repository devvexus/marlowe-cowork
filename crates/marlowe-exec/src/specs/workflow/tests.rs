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
    // A subagent the conductor spawns sees the conductor's host through its type's filter: never
    // the workflow tool, whatever the type (CC gives subagents no Workflow tool either).
    let conductor: Arc<dyn ToolHost> = Arc::new(host(&home, &ws));
    for ty in marlowe_agent::subagent::builtin_types() {
        let sub = marlowe_agent::subagent::FilteredTools::new(conductor.clone(), &ty);
        assert!(!sub.admits("workflow"), "{} admits workflow", ty.name);
        assert!(!sub.specs().iter().any(|s| s.name == "workflow"), "{} lists workflow", ty.name);
    }
}

#[test]
fn parity_wf4_a_waited_on_workflow_taints_the_conversation() {
    let (home, ws) = (temp("home7"), temp("ws7"));
    let t = host(&home, &ws);
    // The returned value is built by agents that read the web and the repository.
    assert!(t.taints(&req("workflow", json!({"name": "code-review", "background": false}))));
    // In the background the result comes back through the relay, which carries its own taint.
    assert!(!t.taints(&req("workflow", json!({"name": "code-review"}))));
    assert!(!t.taints(&req("workflow", json!({"name": "code-review", "background": true}))));
}

/// The daemon's launch rule (`may_run`, then `trust_approved`) over the real resolver.
struct BindingLauncher {
    home: PathBuf,
    ws: PathBuf,
    ran: Mutex<Vec<String>>,
}

impl WorkflowLauncher for BindingLauncher {
    fn launch(&self, req: LaunchRequest) -> Result<Launched, String> {
        let roots = marlowe_workflow::service::roots(&self.home, Some(&self.ws), vec![]);
        let p = marlowe_workflow::service::prepare(&roots, req.script.as_deref(), req.script_path.as_deref(), req.name.as_deref())?;
        marlowe_workflow::service::may_run(&self.home, &p, req.approved_digest.as_deref())?;
        self.ran.lock().unwrap().push(p.source.clone());
        marlowe_workflow::service::trust_approved(&self.home, &p, req.approved_digest.as_deref()).unwrap();
        Ok(Launched { run_id: "r1".into(), script_path: "wf.star".into(), name: p.name, meta: p.meta })
    }

    fn wait(&self, _run: &str, _cancel: &CancelToken) -> Waited {
        Waited::Done { value: json!(null), tainted: false }
    }
}

const OTHER: &str = "META = {\"name\": \"other\", \"description\": \"Something else\"}
def main(args):
    return agent(\"z\")
";

#[test]
fn parity_wf1_the_launch_runs_the_approved_digest_or_nothing() {
    let (home, ws) = (temp("home8"), temp("ws8"));
    let t = host(&home, &ws);
    let launcher = Arc::new(BindingLauncher { home: home.clone(), ws: ws.clone(), ran: Mutex::new(Vec::new()) });
    let services = Services::new().with(Workflows(launcher.clone()));
    let call = req("workflow", json!({"script_path": "wf.star"}));
    let roots = marlowe_workflow::service::roots(&home, Some(&ws), vec![]);
    let trusted = |src: &str| {
        let p = marlowe_workflow::service::prepare(&roots, Some(src), None, None).unwrap();
        marlowe_workflow::service::is_trusted(&home, &p)
    };

    // 1. A batch [write wf.star, workflow{script_path}]: the consequence finds no file (nothing to
    //    approve), the write lands, and the launch refuses what nobody saw.
    assert_eq!(t.consequence(&call), Consequence::Free);
    std::fs::write(ws.join("wf.star"), SCRIPT).unwrap();
    let out = t.call_scoped(&call, &cx(&ws, services.clone()));
    assert!(out.is_error && out.content.contains("approve this version"), "{}", out.content);
    assert!(launcher.ran.lock().unwrap().is_empty() && !trusted(SCRIPT));

    // 2. The user approves the META of the file as it is; a write in the same batch replaces it.
    assert!(matches!(t.consequence(&call), Consequence::Ask { .. }));
    std::fs::write(ws.join("wf.star"), OTHER).unwrap();
    let out = t.call_scoped(&call, &cx(&ws, services.clone()));
    assert!(out.is_error, "{}", out.content);
    assert!(launcher.ran.lock().unwrap().is_empty() && !trusted(OTHER) && !trusted(SCRIPT), "neither version is trusted");

    // 3. Approved and unchanged: it runs, and that digest (only) is trusted from now on.
    assert!(matches!(t.consequence(&call), Consequence::Ask { .. }));
    let out = t.call_scoped(&call, &cx(&ws, services.clone()));
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(launcher.ran.lock().unwrap().as_slice(), [OTHER.to_string()]);
    assert!(trusted(OTHER) && !trusted(SCRIPT));
    assert_eq!(t.consequence(&call), Consequence::Free);

    // 4. A call that never went through the check has no approval at all.
    std::fs::write(ws.join("wf.star"), SCRIPT).unwrap();
    let out = t.call_scoped(&call, &cx(&ws, services));
    assert!(out.is_error && launcher.ran.lock().unwrap().len() == 1, "{}", out.content);
}

#[test]
fn parity_wf9_the_description_carries_the_api_and_no_block_is_needed() {
    let (home, ws) = (temp("home5"), temp("ws5"));
    let with = host(&home, &ws);
    assert!(!with.persona_blocks().iter().any(|b| b.text().contains("## Workflows")), "the workflow group has no persona block");
    let all_but: Vec<ToolGroup> = ToolGroup::ALL.iter().copied().filter(|g| *g != ToolGroup::Workflow).collect();
    let without = host(&home, &ws).with_exposure(ExposureProfile::Frontier, all_but);
    assert!(!without.specs().iter().any(|s| s.name == "workflow"));
    // The description carries the API on its own.
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

// ---------------------------------------------------------------- .star files are validated as they are written

mod star_files {
    use super::*;
    use crate::specs::workflow::star::StarValidator;
    use marlowe_agent::todo::{VerifyState, WorkingSet};
    use marlowe_agent::{Conversation, FinalFlow, Mutation, StepInfo, ToolMiddleware, ToolOutcome};
    use marlowe_model::ToolCall;

    fn written(star: &StarValidator, cx: &CallCtx, id: &str, path: &Path) -> ToolOutcome {
        let call = ToolCall { id: id.into(), name: "write".into(), args: json!({"path": path}) };
        let paths = Mutation::Paths(vec![path.to_path_buf()]);
        star.pre_mutation(&call, cx, &paths).unwrap();
        let mut out = ToolOutcome::ok("wrote it", "wrote");
        star.after_call(&call, cx, &mut out);
        out
    }

    #[test]
    fn a_written_star_file_is_validated_and_its_errors_join_the_result() {
        let ws = temp("star1");
        let cx = cx(&ws, Services::new());
        let star = StarValidator::default();
        let good = ws.join("two.star");
        std::fs::write(&good, SCRIPT).unwrap();
        let out = written(&star, &cx, "a", &good);
        assert!(out.content.starts_with("wrote it") && out.content.contains("is a valid workflow"), "{}", out.content);

        // The kind of file a model writes when it has not read the API: no META, a while loop.
        let bad = ws.join("bad.star");
        std::fs::write(&bad, "def main(args):
    while True:
        agent(\"x\")
").unwrap();
        let out = written(&star, &cx, "b", &bad);
        assert!(out.content.contains("<workflow-errors>") && out.content.contains("not a valid workflow"), "{}", out.content);
        assert!(!out.is_error, "the write itself succeeded");

        // Other files, failed writes and calls without paths are left alone.
        let txt = ws.join("notes.txt");
        std::fs::write(&txt, "x").unwrap();
        assert_eq!(written(&star, &cx, "c", &txt).content, "wrote it");
        let call = ToolCall { id: "d".into(), name: "write".into(), args: json!({}) };
        star.pre_mutation(&call, &cx, &Mutation::Paths(vec![bad.clone()])).unwrap();
        let mut failed = ToolOutcome::error("disk full");
        star.after_call(&call, &cx, &mut failed);
        assert_eq!(failed.content, "disk full");
    }

    #[test]
    fn the_stop_check_counts_a_validated_star_file_as_checked_and_an_invalid_one_as_not() {
        let ws = temp("star2");
        std::fs::write(ws.join("Cargo.toml"), "[package]
name = \"x\"
").unwrap();
        let cx = cx(&ws, Services::new());
        let (star, set) = (StarValidator::default(), WorkingSet::new());
        let (good, bad) = (ws.join("two.star"), ws.join("bad.star"));
        std::fs::write(&good, SCRIPT).unwrap();
        std::fs::write(&bad, "this is not starlark (
").unwrap();
        let mut convo = Conversation::new(SessionId("s1".into()), Some(ws.clone()));
        let finish = |convo: &mut Conversation, cx: &CallCtx| {
            convo.ext = std::mem::take(&mut *cx.scope.ext.lock().unwrap());
            let flow = set.before_final(StepInfo { turn: 1, step: 1 }, convo, "done");
            *cx.scope.ext.lock().unwrap() = std::mem::take(&mut convo.ext);
            flow
        };
        for (path, id) in [(&good, "g"), (&bad, "b")] {
            let call = ToolCall { id: id.into(), name: "write".into(), args: json!({}) };
            let paths = Mutation::Paths(vec![path.clone()]);
            set.pre_mutation(&call, &cx, &paths).unwrap();
        }
        assert_eq!(cx.scope.with_ext(|e| e.get_or_default::<VerifyState>().dirty.len()), 2);
        // Validating the good file clears it; the bad one still holds the reminder.
        written(&star, &cx, "g", &good);
        written(&star, &cx, "b", &bad);
        let dirty = cx.scope.with_ext(|e| e.get_or_default::<VerifyState>().dirty.clone());
        assert_eq!(dirty.into_iter().collect::<Vec<_>>(), [bad.clone()]);
        assert!(matches!(finish(&mut convo, &cx), FinalFlow::Continue(t) if t.contains("bad.star")));
        // Fixed and validated: nothing is left to check.
        std::fs::write(&bad, SCRIPT).unwrap();
        written(&star, &cx, "b2", &bad);
        assert!(matches!(finish(&mut convo, &cx), FinalFlow::Accept));
    }

    #[test]
    fn the_group_installs_the_validator_and_the_description_says_when_to_write_a_file() {
        let (home, ws) = (temp("home5"), temp("ws5"));
        let t = host(&home, &ws);
        assert!(t.middleware().iter().any(|m| m.name() == "star-validator"));
        let d = t.specs().into_iter().find(|s| s.name == "workflow").unwrap().description;
        assert!(d.contains("pass `script` inline") && d.contains("save a .star only when asked"), "{d}");
    }
}
