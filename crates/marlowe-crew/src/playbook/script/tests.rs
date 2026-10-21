//! Workflow runs on the crew: structured output at the tool layer and the finalisation call
//! (WF3), the agent's tool list (WF2), the journal and in-place resume (WF5, BG3). Every model is
//! scripted; nothing reaches a network or a GPU.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use marlowe_agent::{Consequence, ToolCallReq, ToolCtx, ToolHost, ToolOutcome};
use marlowe_model::{
    fold_events, CancelToken, ChatRequest, Completion, Message, ModelInfo, Provider, ProviderError, ProviderKind, Role, StopReason,
    StreamEvent, ToolSpec,
};
use marlowe_proto::{RunId, SandboxKind, WorkflowRef};
use serde_json::{json, Value};

use super::*;
use crate::playbook::{AgentRunner, ModelResolver, StepModel};
use crate::role::{RoleCard, Roles};
use crate::sandbox::Sandbox;

type Respond = dyn Fn(&ChatRequest, usize) -> Vec<StreamEvent> + Send + Sync;

struct Model {
    respond: Box<Respond>,
    calls: AtomicUsize,
    requests: Mutex<Vec<ChatRequest>>,
    local: bool,
}

impl Model {
    fn new(local: bool, respond: impl Fn(&ChatRequest, usize) -> Vec<StreamEvent> + Send + Sync + 'static) -> Arc<Model> {
        Arc::new(Model { respond: Box::new(respond), calls: AtomicUsize::new(0), requests: Mutex::new(Vec::new()), local })
    }

    fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Provider for Model {
    fn kind(&self) -> ProviderKind {
        if self.local {
            ProviderKind::Ollama
        } else {
            ProviderKind::OpenRouter
        }
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![])
    }

    fn stream(&self, req: &ChatRequest, _cancel: &CancelToken, on: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        self.requests.lock().unwrap().push(req.clone());
        let evs = (self.respond)(req, self.calls.fetch_add(1, Ordering::SeqCst));
        for ev in &evs {
            on(ev.clone());
        }
        Ok(fold_events(&evs))
    }
}

fn info(local: bool) -> ModelInfo {
    let mut m = ModelInfo::bare(if local { ProviderKind::Ollama } else { ProviderKind::OpenRouter }, "scripted");
    m.supports_tools = true;
    m.local = local;
    m
}

fn answer(t: &str) -> Vec<StreamEvent> {
    vec![StreamEvent::TextDelta(t.to_string()), StreamEvent::Stop(StopReason::EndTurn)]
}

fn call(name: &str, args: Value) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolCallStart { index: 0, id: String::new(), name: name.into() },
        StreamEvent::ToolArgsDelta { index: 0, json: args.to_string() },
        StreamEvent::ToolCallEnd { index: 0 },
        StreamEvent::Stop(StopReason::ToolUse),
    ]
}

fn last_tool_result(req: &ChatRequest) -> Option<String> {
    // The whole message as JSON: tool results are not text parts.
    req.messages.last().filter(|m| m.role == Role::Tool).map(|m| serde_json::to_string(m).unwrap_or_default())
}

fn prompt_of(req: &ChatRequest) -> String {
    req.messages.iter().find(|m| m.role == Role::User).map(Message::joined_text).unwrap_or_default()
}

struct NoTools;

impl ToolHost for NoTools {
    fn specs(&self) -> Vec<ToolSpec> {
        ["read", "grep", "agent", "workflow", "dispatch", "report_findings"]
            .map(|n| ToolSpec { name: n.into(), description: n.into(), input_schema: json!({"type": "object"}) })
            .to_vec()
    }
    fn call(&self, call: &ToolCallReq, _ctx: &ToolCtx) -> ToolOutcome {
        ToolOutcome::ok(format!("{} ran", call.name), "ok")
    }
    fn consequence(&self, _call: &ToolCallReq) -> Consequence {
        Consequence::Free
    }
}

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("marlowe-wfcrew-{name}-{}", RunId::new_random().short()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn runner(model: Arc<Model>, root: &Path) -> AgentRunner {
    let local = model.local;
    let resolver: ModelResolver = Arc::new(move |_card: &RoleCard| Ok(StepModel { provider: model.clone(), model: info(local) }));
    let sandbox = Sandbox { kind: SandboxKind::Scratch, root: root.to_path_buf(), origin: None, branch: None, base: None, repo_digest: None, frozen: Vec::new(), target_dir: None };
    AgentRunner::new(RunId::new_random(), Roles::builtin(), resolver, Arc::new(NoTools), sandbox, None)
}

fn job_script(home: &Path, run: &RunId, source: &str, args: Value, resumed_from: Option<RunId>) -> WorkflowRef {
    let dir = run_dir(home, run);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("script.star");
    std::fs::write(&path, source).unwrap();
    WorkflowRef { name: Some("t".into()), script_path: path.display().to_string(), digest: marlowe_workflow::digest(source), args, foreground: false, resumed_from }
}

fn go(r: &AgentRunner, home: &Path, run: &RunId, wf: &WorkflowRef) -> (Result<marlowe_proto::Dispatch, CrewError>, Vec<ScriptEvent>) {
    let events = Mutex::new(Vec::new());
    let job = ScriptJob { home, run, session: None, workspace: None, workflow: wf, plugins: vec![] };
    let out = run_workflow(r, job, &|e| events.lock().unwrap().push(e));
    (out, events.into_inner().unwrap())
}

const SCHEMA_SCRIPT: &str = "META = {\"description\": \"one structured answer\"}\nN = {\"type\": \"object\", \"properties\": {\"n\": {\"type\": \"integer\"}}, \"required\": [\"n\"]}\ndef main(args):\n    return agent(\"count the files\", schema = N)\n";

#[test]
fn parity_wf3_an_invalid_answer_is_retried_into_an_object() {
    // 1: a wrong answer; 2: the corrected one after the tool's errors; 3: the turn ends.
    let model = Model::new(false, |req, i| match i {
        0 => {
            assert!(req.tools.iter().any(|t| t.name == STRUCTURED_OUTPUT && t.input_schema["required"][0] == "n"));
            call(STRUCTURED_OUTPUT, json!({"n": "three"}))
        }
        1 => {
            let r = last_tool_result(req).unwrap();
            assert!(r.contains("/n: expected integer") && r.contains("3 of 3 retries left"), "{r}");
            call(STRUCTURED_OUTPUT, json!({"n": 3}))
        }
        _ => answer("done"),
    });
    let home = temp("wf3a");
    let r = runner(model.clone(), &home);
    let run = RunId::new_random();
    let wf = job_script(&home, &run, SCHEMA_SCRIPT, json!({}), None);
    let (out, _) = go(&r, &home, &run, &wf);
    let d = out.unwrap();
    assert!(d.summary.contains("\"n\": 3"), "{}", d.summary);
    assert_eq!(model.requests().len(), 3);
    assert!(model.requests().iter().all(|q| q.format.is_none()), "no finalisation needed");
}

#[test]
fn parity_wf3_three_retries_then_none() {
    // Four invalid calls: the first three get the errors back and a retry, the fourth is refused.
    let model = Model::new(false, |req, i| {
        if req.format.is_some() {
            return answer("still not a number");
        }
        match i {
            0..=3 => {
                if i > 0 {
                    let r = last_tool_result(req).unwrap();
                    assert!(r.contains(&format!("{} of 3 retries left", 4 - i)), "{i}: {r}");
                }
                call(STRUCTURED_OUTPUT, json!({"n": format!("three{i}")}))
            }
            4 => {
                let r = last_tool_result(req).unwrap();
                assert!(r.contains("No more attempts"), "{r}");
                answer("done")
            }
            _ => answer("done"),
        }
    });
    let home = temp("wf3c");
    let r = runner(model.clone(), &home);
    let run = RunId::new_random();
    let wf = job_script(&home, &run, SCHEMA_SCRIPT, json!({}), None);
    let (out, _) = go(&r, &home, &run, &wf);
    assert!(out.unwrap().summary.contains("null"));
    let turns = model.requests().iter().filter(|q| q.format.is_none()).count();
    assert_eq!(turns, 6, "four invalid calls, the refusal, and one crew-guard nudge before the turn ends");
}

#[test]
fn parity_wf3_an_agent_without_a_valid_call_gets_one_finalisation_call_with_format() {
    let model = Model::new(true, |req, _| {
        if req.format.is_some() {
            assert!(req.tools.is_empty(), "tools off in the finalisation call");
            return answer("{\"n\": 7}");
        }
        answer("I counted seven files.")
    });
    let home = temp("wf3b");
    let r = runner(model.clone(), &home);
    let run = RunId::new_random();
    let wf = job_script(&home, &run, SCHEMA_SCRIPT, json!({}), None);
    let (out, _) = go(&r, &home, &run, &wf);
    assert!(out.unwrap().summary.contains("\"n\": 7"));
    let reqs = model.requests();
    let fin = reqs.iter().find(|q| q.format.is_some()).expect("a finalisation request");
    assert_eq!(fin.format.as_ref().unwrap()["required"][0], "n");
    assert!(prompt_of(fin).contains("I counted seven files."));
    // The journal says it was a repair.
    let lines = marlowe_workflow::journal::load(&journal_path(&home, &run)).unwrap();
    let agents = marlowe_workflow::journal::agents(&lines);
    assert!(agents[0].result.note.as_deref().unwrap_or("").contains("repair"));
}

#[test]
fn parity_wf2_workflow_agents_cannot_spawn() {
    let card = Roles::builtin().get(WORKFLOW_ROLE).cloned().expect("the workflow role card");
    for t in ["agent", "workflow", "dispatch", "send_message"] {
        assert!(!card.allows(t), "a workflow agent must not have `{t}`");
    }
    let model = Model::new(false, |req, _| {
        let names: Vec<&str> = req.tools.iter().map(|t| t.name.as_str()).collect();
        assert!(!names.contains(&"agent") && !names.contains(&"workflow") && !names.contains(&"dispatch"), "{names:?}");
        answer("ok")
    });
    let home = temp("wf2");
    let r = runner(model.clone(), &home);
    let run = RunId::new_random();
    let src = "META = {\"description\": \"d\"}\ndef main(args):\n    return agent(\"hi\")\n";
    let wf = job_script(&home, &run, src, json!({}), None);
    go(&r, &home, &run, &wf).0.unwrap();
    assert_eq!(model.requests().len(), 1);
}

#[test]
fn a_local_workflow_runs_as_many_agents_at_once_as_the_crew_has_slots() {
    let (now, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (n, p) = (now.clone(), peak.clone());
    let model = Model::new(true, move |_, _| {
        p.fetch_max(n.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(150));
        n.fetch_sub(1, Ordering::SeqCst);
        answer("ok")
    });
    let home = temp("local-slots");
    let mut r = runner(model, &home);
    r.local_slots = Arc::new(|_| 3);
    let run = RunId::new_random();
    let src = "META = {\"description\": \"six\"}
def main(args):
    return parallel([lambda: agent(\"a\"), lambda: agent(\"b\"), lambda: agent(\"c\"), lambda: agent(\"d\"), lambda: agent(\"e\"), lambda: agent(\"f\")])
";
    let wf = job_script(&home, &run, src, json!({}), None);
    go(&r, &home, &run, &wf).0.unwrap();
    assert_eq!(peak.load(Ordering::SeqCst), marlowe_workflow::default_concurrency(3), "the crew's slots, not one at a time and not the cloud's eight");
    assert!(peak.load(Ordering::SeqCst) > 1);
}

#[test]
fn parity_bg3_the_journal_has_one_line_per_finished_agent_and_resume_runs_only_the_rest() {
    let model = Model::new(false, |req, _| answer(&format!("did {}", prompt_of(req))));
    let home = temp("bg3");
    let r = runner(model.clone(), &home);
    let run = RunId::new_random();
    let src = "META = {\"description\": \"four\"}\ndef main(args):\n    return [agent(p) for p in [\"a\", \"b\", \"c\", \"d\"]]\n";
    let wf = job_script(&home, &run, src, json!({}), None);
    let (out, events) = go(&r, &home, &run, &wf);
    out.unwrap();
    let lines = marlowe_workflow::journal::load(&journal_path(&home, &run)).unwrap();
    assert_eq!(marlowe_workflow::journal::agents(&lines).len(), 4);
    assert!(matches!(lines.last(), Some(marlowe_workflow::Line::Result { .. })));
    assert!(events.iter().any(|e| matches!(e, ScriptEvent::Progress(i) if i.agents.len() == 4)));
    assert!(progress(&run).is_some());

    // Killed after two: the same run resumes in place from its journal, two calls live.
    let journal = journal_path(&home, &run);
    let kept: Vec<String> = std::fs::read_to_string(&journal).unwrap().lines().take(2).map(str::to_string).collect();
    std::fs::write(&journal, kept.join("\n") + "\n").unwrap();
    let before = model.requests().len();
    let (out, _) = go(&r, &home, &run, &wf);
    let d = out.unwrap();
    assert_eq!(model.requests().len() - before, 2, "only the two unfinished agents ran");
    assert!(d.summary.contains("2 from cache"), "{}", d.summary);
}

#[test]
fn parity_wf5_resume_from_another_run_keeps_the_unchanged_prefix() {
    let model = Model::new(false, |req, _| answer(&format!("did {}", prompt_of(req))));
    let home = temp("wf5");
    let r = runner(model.clone(), &home);
    let first = RunId::new_random();
    let src = |third: &str| format!("META = {{\"description\": \"four\"}}\ndef main(args):\n    return [agent(p) for p in [\"a\", \"b\", \"{third}\", \"d\"]]\n");
    let wf = job_script(&home, &first, &src("c"), json!({}), None);
    go(&r, &home, &first, &wf).0.unwrap();
    let second = RunId::new_random();
    let wf2 = job_script(&home, &second, &src("C"), json!({}), Some(first.clone()));
    let before = model.requests().len();
    go(&r, &home, &second, &wf2).0.unwrap();
    let live: Vec<String> = model.requests()[before..].iter().map(prompt_of).collect();
    assert_eq!(live.len(), 2, "{live:?}");
    assert!(live[0].starts_with('C') && live[1].starts_with('d'), "{live:?}");
}

struct Recording(Mutex<Vec<String>>);

impl RunObserver for Recording {
    fn agent_start(&self, _run: &RunId, call: &AgentCall) -> Option<String> {
        self.0.lock().unwrap().push(format!("start {}", call.index));
        Some("HOOK CONTEXT: keep it short".into())
    }
    fn agent_stop(&self, _run: &RunId, call: &AgentCall, result: &AgentResult) {
        self.0.lock().unwrap().push(format!("stop {} {:?}", call.index, result.status));
    }
}

#[test]
fn parity_x22_every_workflow_agent_fires_start_and_stop_hooks() {
    let model = Model::new(false, |req, _| {
        assert!(prompt_of(req).contains("HOOK CONTEXT"), "SubagentStart context reaches the agent");
        answer("ok")
    });
    let home = temp("x22");
    let r = runner(model, &home);
    let run = RunId::new_random();
    let rec = Arc::new(Recording(Mutex::new(Vec::new())));
    set_observer(&run, rec.clone());
    let src = "META = {\"description\": \"two\"}\ndef main(args):\n    return parallel([lambda: agent(\"a\"), lambda: agent(\"b\")])\n";
    let wf = job_script(&home, &run, src, json!({}), None);
    go(&r, &home, &run, &wf).0.unwrap();
    let mut seen = rec.0.lock().unwrap().clone();
    seen.sort();
    assert_eq!(seen, ["start 0", "start 1", "stop 0 Done", "stop 1 Done"]);
}

/// A run nobody set hooks for (started from the TUI, or resumed in place after a restart) gets
/// them from its home's factory, for its own session; the registry forgets them when it ends.
#[test]
fn parity_x22_a_run_without_hooks_gets_its_sessions_from_the_factory() {
    let model = Model::new(false, |_, _| answer("ok"));
    let home = temp("x22f");
    let r = runner(model, &home);
    let run = RunId::new_random();
    let rec = Arc::new(Recording(Mutex::new(Vec::new())));
    let asked = Arc::new(Mutex::new(Vec::new()));
    let (given, seen) = (rec.clone(), asked.clone());
    set_observer_factory(
        &home,
        Arc::new(move |run: &RunId, session: Option<&marlowe_proto::SessionId>, _ws: Option<&Path>| {
            seen.lock().unwrap().push((run.clone(), session.cloned()));
            Some(given.clone() as Arc<dyn RunObserver>)
        }),
    );
    let src = "META = {\"description\": \"one\"}\ndef main(args):\n    return agent(\"a\")\n";
    let wf = job_script(&home, &run, src, json!({}), None);
    let session = marlowe_proto::SessionId("x22f-s".into());
    let job = ScriptJob { home: &home, run: &run, session: Some(&session), workspace: None, workflow: &wf, plugins: vec![] };
    run_workflow(&r, job, &|_| {}).unwrap();
    assert_eq!(rec.0.lock().unwrap().as_slice(), ["start 0", "stop 0 Done"]);
    assert_eq!(asked.lock().unwrap().as_slice(), [(run.clone(), Some(session))]);
    assert!(!registry().observers.contains_key(&run) && !registry().controls.contains_key(&run), "pruned when the run ended");
}

#[test]
fn parity_wf8_a_workflow_shares_the_sessions_token_pool() {
    let model = Model::new(false, |_, _| {
        vec![
            StreamEvent::TextDelta("x".into()),
            StreamEvent::Usage(marlowe_model::Usage { output: 40, ..Default::default() }),
            StreamEvent::Stop(StopReason::EndTurn),
        ]
    });
    let home = temp("wf8");
    let r = runner(model, &home);
    let run = RunId::new_random();
    let session = marlowe_proto::SessionId("pool-s".into());
    let pool = marlowe_workflow::pools::set(&session, 100);
    pool.add(30); // the main loop spent some first
    let src = "META = {\"description\": \"loop\"}\ndef main(args):\n    n = 0\n    for i in range(10):\n        if budget.remaining() < 40:\n            break\n        agent(\"step\")\n        n += 1\n    return {\"n\": n, \"total\": budget.total}\n";
    let wf = job_script(&home, &run, src, json!({}), None);
    let events = Mutex::new(Vec::new());
    let job = ScriptJob { home: &home, run: &run, session: Some(&session), workspace: None, workflow: &wf, plugins: vec![] };
    let d = run_workflow(&r, job, &|e| events.lock().unwrap().push(e)).unwrap();
    assert!(d.summary.contains("\"n\": 1") && d.summary.contains("\"total\": 100"), "{}", d.summary);
    assert_eq!(pool.spent(), 70);
    marlowe_workflow::pools::clear(&session);
}

#[test]
fn a_changed_script_is_refused() {
    let model = Model::new(false, |_, _| answer("x"));
    let home = temp("digest");
    let r = runner(model, &home);
    let run = RunId::new_random();
    let mut wf = job_script(&home, &run, "META = {\"description\": \"d\"}\ndef main(args):\n    return 1\n", json!({}), None);
    wf.digest = "0".repeat(64);
    let e = go(&r, &home, &run, &wf).0.unwrap_err().to_string();
    assert!(e.contains("changed since it was approved"), "{e}");
}

#[test]
fn findings_in_the_return_value_are_stored_and_reported() {
    let model = Model::new(false, |_, _| answer("x"));
    let home = temp("findings");
    let r = runner(model, &home);
    let run = RunId::new_random();
    let src = "META = {\"description\": \"d\"}\ndef main(args):\n    return {\"findings\": [{\"lens\": \"correctness\", \"severity\": \"high\", \"file\": \"a.rs\", \"line\": 2, \"claim\": \"c\", \"evidence\": \"e\"}]}\n";
    let wf = job_script(&home, &run, src, json!({}), None);
    let (out, events) = go(&r, &home, &run, &wf);
    out.unwrap();
    assert!(events.iter().any(|e| matches!(e, ScriptEvent::Findings(f) if f.len() == 1)));
    assert_eq!(marlowe_workflow::findings::load(&home, &format!("run-{}", run.as_str())).len(), 1);
}
