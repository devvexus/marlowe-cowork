//! The `manage_project` tool over a real project service whose model never answers: the tool's
//! words, its guards and its approvals. The engine and the service are tested in `marlowe-project`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use marlowe_agent::{CallCtx, Consequence, Exposure, ExposureProfile, PersonaBlock, SessionScope, Services, ToolCallReq, ToolCtx, ToolGroup, ToolHost};
use marlowe_model::{CancelToken, ChatRequest, Completion, ModelInfo, Provider, ProviderError, ProviderKind, StreamEvent};
use marlowe_project::engine::Unlimited;
use marlowe_project::research::{self, NoMemory, PageFetch, ResearchDeps, SystemResolver, Web};
use marlowe_project::{ActorConfig, Ended, EngineConfig, ProjectEvent, ProjectHost, ProjectService, ProjectState, Registry, RunParts, ServiceConfig};
use marlowe_proto::SessionId;
use serde_json::{json, Value};

use crate::specs::HostSetup;
use crate::BuiltinTools;

/// A model that waits until it is cancelled.
struct Silent;

impl Provider for Silent {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Ollama
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }

    fn stream(&self, _req: &ChatRequest, cancel: &CancelToken, _on: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        while !cancel.is_cancelled() {
            std::thread::sleep(Duration::from_millis(5));
        }
        Err(ProviderError::cancelled())
    }
}

struct NoWeb;

impl Web for NoWeb {
    fn channels(&self) -> Vec<marlowe_research::ChannelId> {
        Vec::new()
    }

    fn search(&self, _c: marlowe_research::ChannelId, _q: &marlowe_research::ChannelQuery, _cancel: &CancelToken) -> Result<Vec<marlowe_research::SourceRef>, marlowe_research::ChannelError> {
        unreachable!("nothing is searched before a plan is approved")
    }
}

struct NoPages;

impl PageFetch for NoPages {
    fn read(&self, _url: &str, _admit: research::Admit<'_>, _cancel: &CancelToken) -> research::Fetched {
        unreachable!("nothing is read before a plan is approved")
    }
}

struct TestHost {
    root: PathBuf,
}

impl ProjectHost for TestHost {
    fn parts(&self, _state: &ProjectState) -> Result<RunParts, String> {
        let deps = Arc::new(ResearchDeps::new(self.root.clone(), self.root.join("output"), Arc::new(NoWeb), Arc::new(NoPages), Arc::new(SystemResolver), Arc::new(NoMemory)));
        let mut cfg = EngineConfig::new("m");
        cfg.poll = Duration::from_millis(5);
        Ok(RunParts { provider: Arc::new(Silent), gate: Arc::new(Unlimited), toolbox: Arc::new(research::toolbox(&deps)), config: cfg })
    }

    fn event(&self, _event: ProjectEvent) {}

    fn ended(&self, _ended: Ended) {}
}

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("marlowe-projtool-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn host(home: &Path, ws: &Path) -> BuiltinTools {
    let t = BuiltinTools::new(Some(ws.to_path_buf()));
    crate::specs::workflow::configure(t, &HostSetup { home, workspace: Some(ws) })
}

fn service(home: &Path) -> ProjectService {
    let root = home.join("projects");
    let cfg = ServiceConfig { root: root.clone(), db: Some(home.join("cowork.db")), registry: Arc::new(Registry::builtin()), actor: ActorConfig::default() };
    ProjectService::start(cfg, Arc::new(TestHost { root })).unwrap()
}

fn req(args: Value) -> ToolCallReq {
    ToolCallReq { id: "c1".into(), name: "manage_project".into(), args }
}

fn cx(ws: &Path, turn: u32, svc: Option<&ProjectService>) -> CallCtx {
    let services = match svc {
        Some(s) => Services::new().with(s.clone()),
        None => Services::new(),
    };
    CallCtx::new(ToolCtx::new(SessionId("s1".into()), turn, Some(ws.to_path_buf()), CancelToken::new(), false), SessionScope { services, ..SessionScope::detached() })
}

fn plan() -> Value {
    json!({
        "objective": "Find the newest open models from Moonshot AI and say what is confirmed and what is rumoured.",
        "questions": ["What is the newest Kimi model?", "Is a successor announced?"],
        "fields": [{"key": "model_name"}, {"key": "status", "values": ["released", "announced", "rumoured"]}]
    })
}

fn wait(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(20);
    while Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

fn id_of(text: &str) -> String {
    let i = text.find("p_").expect("an id in the card");
    text[i..i + 10].to_string()
}

#[test]
fn the_tool_is_the_conductors_with_a_short_description_and_the_block_rides_with_it() {
    let (home, ws) = (temp("h1"), temp("w1"));
    let t = host(&home, &ws).with_exposure(ExposureProfile::Frontier, ToolGroup::ALL.to_vec());
    let spec = t.specs().into_iter().find(|s| s.name == "manage_project").expect("the conductor has it");
    assert!(spec.description.chars().count() <= 600, "{}", spec.description.chars().count());
    for word in ["create", "revise", "status", "list", "answer", "checkin", "cancel", "reopen"] {
        assert!(spec.description.contains(word), "{word}");
    }
    assert_eq!(t.manifest("manage_project").map(|m| m.group), Some(ToolGroup::Workflow));
    let blocks = t.persona_blocks();
    assert!(blocks.iter().any(|b| matches!(b, PersonaBlock::Tool { group: ToolGroup::Workflow, text } if text.contains("manage_project") && text.contains("never approve for them"))));
    // A crew host has neither the tool nor, with no live tool of the group but report_findings, a reason to say how to use it.
    let crew = crate::specs::workflow::crew_configure(BuiltinTools::new(Some(ws.clone())), &HostSetup { home: &home, workspace: Some(&ws) });
    let crew = crew.with_exposure(ExposureProfile::Frontier, ToolGroup::ALL.to_vec());
    assert!(!crew.specs().iter().any(|s| s.name == "manage_project"));
}

#[test]
fn without_a_service_the_call_says_so_and_a_missing_action_lists_the_actions() {
    let (home, ws) = (temp("h2"), temp("w2"));
    let t = host(&home, &ws);
    let out = t.call_scoped(&req(json!({"action": "list"})), &cx(&ws, 1, None));
    assert!(out.is_error && out.content.contains("no project service"), "{}", out.content);
    let svc = service(&home);
    let out = t.call_scoped(&req(json!({})), &cx(&ws, 1, Some(&svc)));
    assert!(out.is_error && out.content.contains("create, revise, status, list, answer"), "{}", out.content);
    let out = t.call_scoped(&req(json!({"action": "launch"})), &cx(&ws, 1, Some(&svc)));
    assert!(out.is_error && out.content.contains("`launch` is not one of"), "{}", out.content);
    svc.shutdown();
}

#[test]
fn create_returns_the_card_at_once_and_a_bad_plan_is_told_what_to_write() {
    let (home, ws) = (temp("h3"), temp("w3"));
    let t = host(&home, &ws);
    let svc = service(&home);
    let c = cx(&ws, 1, Some(&svc));
    let bad = t.call_scoped(&req(json!({"action": "create", "plan": {"objective": "x", "questions": []}})), &c);
    assert!(bad.is_error && bad.class == Some("project_plan") && bad.content.contains("plan.questions"), "{:?} {}", bad.class, bad.content);
    assert!(bad.content.lines().count() <= 4, "{}", bad.content);
    let none = t.call_scoped(&req(json!({"action": "create"})), &c);
    assert!(none.is_error && none.content.contains(r#"{"objective""#), "{}", none.content);
    let started = Instant::now();
    let out = t.call_scoped(&req(json!({"action": "create", "plan": plan()})), &c);
    assert!(started.elapsed() < Duration::from_secs(3), "the tool never waits for the project");
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("1. What is the newest Kimi model?") && out.content.contains("ask whether to go ahead") && out.content.contains("Nothing is read before the user approves"), "{}", out.content);
    let id = id_of(&out.content);
    wait("the project to park", || !svc.is_running(&id));
    let listed = t.call_scoped(&req(json!({"action": "list"})), &c);
    assert!(listed.content.contains(&id) && listed.content.contains("awaiting_plan"), "{}", listed.content);
    // With no id and one unfinished project, status means that one; the words are the code's.
    let status = t.call_scoped(&req(json!({"action": "status"})), &c);
    assert!(status.content.contains("Waiting for the user: card g_1 (plan). Answers: approve, cancel"), "{}", status.content);
    svc.shutdown();
}

#[test]
fn a_plan_sent_as_a_json_string_is_the_object_it_holds() {
    let (home, ws) = (temp("h4"), temp("w4"));
    let t = host(&home, &ws);
    let svc = service(&home);
    let out = t.call_scoped(&req(json!({"action": "create", "plan": plan().to_string()})), &cx(&ws, 1, Some(&svc)));
    assert!(!out.is_error, "{}", out.content);
    svc.shutdown();
}

#[test]
fn a_plan_cannot_be_approved_in_the_turn_that_made_it_and_yes_means_approve_after() {
    let (home, ws) = (temp("h5"), temp("w5"));
    let t = host(&home, &ws);
    let svc = service(&home);
    let out = t.call_scoped(&req(json!({"action": "create", "plan": plan()})), &cx(&ws, 4, Some(&svc)));
    let id = id_of(&out.content);
    wait("the project to park", || !svc.is_running(&id));
    let same = t.call_scoped(&req(json!({"action": "answer", "id": id, "answer": "approve"})), &cx(&ws, 4, Some(&svc)));
    assert!(same.is_error && same.content.contains("has not seen its plan"), "{}", same.content);
    let wrong = t.call_scoped(&req(json!({"action": "answer", "id": id, "answer": "steer"})), &cx(&ws, 5, Some(&svc)));
    assert!(wrong.is_error && wrong.class == Some("project_answer") && wrong.content.contains("approve, cancel"), "{}", wrong.content);
    let ok = t.call_scoped(&req(json!({"action": "answer", "id": id, "answer": "yes"})), &cx(&ws, 5, Some(&svc)));
    assert!(!ok.is_error && ok.content.contains("approved and running"), "{}", ok.content);
    let none = t.call_scoped(&req(json!({"action": "answer", "id": id})), &cx(&ws, 6, Some(&svc)));
    assert!(none.is_error && none.content.contains("approve, keep_researching, steer"), "{}", none.content);
    svc.shutdown();
}

#[test]
fn once_untrusted_content_is_in_the_conversation_an_answer_that_sends_the_project_on_asks() {
    let (home, ws) = (temp("h6"), temp("w6"));
    let t = host(&home, &ws);
    let clean = Exposure::default();
    let tainted = Exposure { tainted: true, ..Exposure::default() };
    let approve = req(json!({"action": "answer", "id": "p_0123abcd", "answer": "approve"}));
    assert_eq!(t.consequence_exposed(&approve, &clean), Consequence::Free);
    match t.consequence_exposed(&approve, &tainted) {
        Consequence::Ask { summary } => assert!(summary.contains("approve p_0123abcd") && summary.contains("public web pages"), "{summary}"),
        other => panic!("{other:?}"),
    }
    for free in [json!({"action": "answer", "id": "p_0123abcd", "answer": "cancel"}), json!({"action": "status"}), json!({"action": "list"}), json!({"action": "create", "plan": {}})] {
        assert_eq!(t.consequence_exposed(&req(free.clone()), &tainted), Consequence::Free, "{free}");
    }
    let more = req(json!({"action": "answer", "id": "p_0123abcd", "answer": "keep researching"}));
    assert!(t.consequence_exposed(&more, &tainted).asks());
}

#[test]
fn revise_cancel_and_reopen_speak_plainly() {
    let (home, ws) = (temp("h7"), temp("w7"));
    let t = host(&home, &ws);
    let svc = service(&home);
    let c = cx(&ws, 1, Some(&svc));
    let id = id_of(&t.call_scoped(&req(json!({"action": "create", "plan": plan()})), &c).content);
    wait("the project to park", || !svc.is_running(&id));
    let revised = t.call_scoped(&req(json!({"action": "revise", "plan": {"questions": ["What is the newest Kimi model and when did it ship?"]}})), &c);
    assert!(!revised.is_error && revised.content.contains("1. What is the newest Kimi model and when did it ship?") && revised.content.contains("still waiting"), "{}", revised.content);
    wait("the revised card", || !svc.is_running(&id));
    let no_keys = t.call_scoped(&req(json!({"action": "revise", "id": id})), &c);
    assert!(no_keys.is_error && no_keys.content.contains("only the keys to change"), "{}", no_keys.content);
    let cancelled = t.call_scoped(&req(json!({"action": "cancel", "id": id})), &c);
    assert!(!cancelled.is_error && cancelled.content.contains("cancelled"), "{}", cancelled.content);
    let again = t.call_scoped(&req(json!({"action": "status"})), &c);
    assert!(again.is_error && again.content.contains("no unfinished project"), "{}", again.content);
    let reopen = t.call_scoped(&req(json!({"action": "reopen", "id": id})), &c);
    assert!(reopen.is_error && reopen.content.contains("phase:"), "{}", reopen.content);
    let reopen = t.call_scoped(&req(json!({"action": "reopen", "id": id, "phase": "DataCollection"})), &c);
    assert!(reopen.is_error && reopen.content.contains("no archived state"), "{}", reopen.content);
    let missing = t.call_scoped(&req(json!({"action": "status", "id": "p_00000000"})), &c);
    assert!(missing.is_error && missing.class == Some("project_missing"), "{:?}", missing.class);
    svc.shutdown();
}

/// Found live: dawn answered "Research the three most popular Rust TUI libraries ..." with thirteen
/// web searches of its own. A research turn's first web calls are answered with the way to a
/// project, every web call of that step alike; the next step searches if the model still wants to.
#[test]
fn a_research_turn_is_routed_to_a_project_once() {
    use marlowe_agent::{CallFlow, Conversation, StepInfo, ToolMiddleware};
    let (home, ws) = (temp("route-h"), temp("route-w"));
    let svc = service(&home);
    let route = super::research_route();
    let mut convo = Conversation::new(SessionId("s1".into()), Some(ws.clone()));
    let mut text = "Research the three most popular Rust TUI libraries and compare them".to_string();
    route.before_turn(&mut convo, &mut text);
    let call = |name: &str| marlowe_model::ToolCall { id: "1".into(), name: name.into(), args: serde_json::json!({"query": "rust tui"}) };
    let c = cx(&ws, 1, Some(&svc));
    let bounced = |flow: CallFlow| matches!(flow, CallFlow::Done(ref o) if o.content.contains("research is a project"));
    assert!(bounced(route.before_approval(&mut call("web_search"), &c)));
    assert!(bounced(route.before_approval(&mut call("web_fetch"), &c)), "the same step's other web calls too");
    route.after_step(StepInfo { turn: 1, step: 1 }, &mut convo);
    assert!(matches!(route.before_approval(&mut call("web_search"), &c), CallFlow::Continue), "the next step searches");

    let mut text = "what is the weather in Paris".to_string();
    route.before_turn(&mut convo, &mut text);
    assert!(matches!(route.before_approval(&mut call("web_search"), &c), CallFlow::Continue), "not a research request");

    let mut text = "research the new Kimi models".to_string();
    route.before_turn(&mut convo, &mut text);
    assert!(matches!(route.before_approval(&mut call("web_search"), &cx(&ws, 2, None)), CallFlow::Continue), "no project service, no route");
}

/// Found live: told that a project replaces it, dawn still ran `team{name:"web-research"}`. With a
/// project service the superseded saved task is refused with the way to the project; without one
/// it is not refused for that reason.
#[test]
fn a_saved_team_task_a_project_replaces_points_at_the_project() {
    let (home, ws) = (temp("sup-h"), temp("sup-w"));
    let t = host(&home, &ws);
    let svc = service(&home);
    let call = ToolCallReq { id: "c1".into(), name: "team".into(), args: json!({"name": "web-research", "args": {"topic": "new Kimi models"}}) };
    let out = t.call_scoped(&call, &cx(&ws, 1, Some(&svc)));
    assert!(out.is_error && out.content.contains("replaced by a research project") && out.content.contains("manage_project"), "{}", out.content);
    let out = t.call_scoped(&call, &cx(&ws, 1, None));
    assert!(!out.content.contains("replaced by a research project"), "{}", out.content);
}

/// Found live (run 10): the `team` description still listed `web-research`, so a small model
/// picked it. Where projects run the listing omits what a project kind replaces; elsewhere not.
#[test]
fn the_team_listing_omits_a_task_a_project_replaces_only_where_projects_run() {
    let (home, ws) = (temp("lst-h"), temp("lst-w"));
    let listing = |t: &BuiltinTools| t.specs().into_iter().find(|s| s.name == "team").unwrap().description;
    let plain = listing(&host(&home, &ws));
    assert!(plain.contains("web-research"), "{plain}");
    let with = listing(&host(&home, &ws).with_state(crate::specs::workflow::ProjectsRun(true)));
    assert!(!with.contains("web-research") && with.contains("code-review"), "{with}");
    assert!(with.len() <= plain.len());
}
