//! Deep research v2 through the supervisor: the plan waits on the user (`PlanProposed`, the run
//! `needs_you`), `approve_plan` answers it, stage progress reaches the event sink, and a rejected
//! plan ends the run cancelled with the reason. A scripted provider and an in-memory web; no
//! network, no inference.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use marlowe_agent::{Consequence, ToolCallReq, ToolCtx, ToolHost, ToolOutcome};
use marlowe_brains::{Brains, Library, MemoryStore, ProviderFactory};
use marlowe_crew::playbook::research_v2::{FetchedPage, ResearchIo};
use marlowe_crew::*;
use marlowe_model::{
    fold_events, CancelToken, ChatRequest, Completion, ModelInfo, Provider, ProviderError, ProviderKind, Role,
    StopReason, StreamEvent, ToolSpec, Usage,
};
use marlowe_proto::{ApprovalDecision, ChannelId, PlanDecision, ResearchPlan, ResearchStage, SessionId, StageState};
use marlowe_research::{ChannelError, ChannelQuery, SourceRef};

const ONE: &str = "https://one.example/m";
const TWO: &str = "https://two.example/m";

/// Answers every role by what its prompt asks.
struct Scripted;

fn reply(prompt: &str) -> String {
    let ids = |tag: char| -> Vec<String> {
        prompt
            .lines()
            .filter_map(|l| {
                let rest = l.strip_prefix(tag)?;
                let d: String = rest.chars().take_while(char::is_ascii_digit).collect();
                (!d.is_empty() && rest[d.len()..].starts_with([':', ' '])).then_some(d)
            })
            .collect()
    };
    if prompt.contains("Draft the research plan") {
        r#"{"sub_questions": [{"question": "What top-1 accuracy does model M reach on ImageNet?", "channels": ["web"]}],
            "breadth": 2, "depth": 0, "source_budget": 2}"#
            .into()
    } else if prompt.contains("Group the claims") {
        r#"[{"statement": "Model M reaches 91.2% top-1 on ImageNet", "supports": [1, 2]}]"#.into()
    } else if prompt.contains("Return the atomic claims") {
        let q = if prompt.contains(ONE) {
            "Model M reaches 91.2% top-1 accuracy"
        } else {
            "model M at 91.2% top-1 accuracy"
        };
        format!(r#"[{{"claim": "Model M reaches 91.2% top-1 on ImageNet", "quote": "{q}", "cites": ""}}]"#)
    } else if prompt.contains("Try to break each claim group") {
        ids('G').iter().map(|g| format!("G{g}: upheld")).collect::<Vec<_>>().join("\n")
    } else if prompt.contains("For each claim below") {
        ids('C').iter().map(|c| format!("C{c}: supported")).collect::<Vec<_>>().join("\n")
    } else if prompt.contains("Each numbered sentence below") {
        ids('S').iter().map(|s| format!("S{s}: supported")).collect::<Vec<_>>().join("\n")
    } else if prompt.contains("section for sub-question 1") {
        "Two independent benchmarks put model M at 91.2% top-1 on ImageNet [C1, C2].".into()
    } else {
        "Model M reaches 91.2% top-1 on ImageNet [C1, C2].".into()
    }
}

impl Provider for Scripted {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenRouter
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }

    fn stream(
        &self,
        req: &ChatRequest,
        _c: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<Completion, ProviderError> {
        let prompt = req.messages.iter().find(|m| m.role == Role::User).map(|m| m.joined_text()).unwrap_or_default();
        let evs = vec![
            StreamEvent::TextDelta(reply(&prompt)),
            StreamEvent::Usage(Usage { input: 100, output: 20, ..Default::default() }),
            StreamEvent::Stop(StopReason::EndTurn),
        ];
        for ev in &evs {
            on(ev.clone());
        }
        Ok(fold_events(&evs))
    }
}

struct Web;

impl ResearchIo for Web {
    fn channels(&self) -> Vec<ChannelId> {
        vec![ChannelId::Web]
    }

    fn search(&self, _c: ChannelId, _q: &ChannelQuery, _cancel: &CancelToken) -> Result<Vec<SourceRef>, ChannelError> {
        Ok(vec![
            SourceRef::new(ChannelId::Web, ONE, "Benchmark one"),
            SourceRef::new(ChannelId::Web, TWO, "Benchmark two"),
        ])
    }

    fn fetch(&self, urls: &[String], _n: usize, _cancel: &CancelToken) -> Vec<FetchedPage> {
        urls.iter()
            .map(|u| {
                let body = if u == ONE {
                    "Model M reaches 91.2% top-1 accuracy on ImageNet."
                } else {
                    "We measure model M at 91.2% top-1 accuracy on ImageNet as well."
                };
                FetchedPage {
                    url: u.clone(),
                    final_url: u.clone(),
                    title: Some("Benchmark".into()),
                    text: format!("# Benchmark\n\n{body}\n\n{}", "Setup details follow. ".repeat(12)),
                    links: Vec::new(),
                    meta: Vec::new(),
                    error: None,
                }
            })
            .collect()
    }
}

struct NoTools;

impl ToolHost for NoTools {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    fn call(&self, call: &ToolCallReq, _ctx: &ToolCtx) -> ToolOutcome {
        ToolOutcome::error(format!("{} is not available", call.name))
    }

    fn consequence(&self, _call: &ToolCallReq) -> Consequence {
        Consequence::Free
    }
}

struct Deny;

impl RunApprovals for Deny {
    fn ask(&self, _run: &RunId, _session: Option<&SessionId>, _req: marlowe_agent::ApprovalReq) -> ApprovalDecision {
        ApprovalDecision::Deny
    }
}

fn start(home: &Path) -> (Supervisor, Receiver<CrewEvent>) {
    start_with(home, true, false, CrewConfig::default())
}

/// `models: false` leaves the library empty, so no run can be given a model (as when admission
/// refuses every load); `gpu_busy` reports the GPU taken.
fn start_with(home: &Path, models: bool, gpu_busy: bool, cfg: CrewConfig) -> (Supervisor, Receiver<CrewEvent>) {
    let mut factory = ProviderFactory::new(Arc::new(MemoryStore::new()));
    factory.register(ProviderKind::OpenRouter, |_| Ok(Arc::new(Scripted) as Arc<dyn Provider>));
    let lib = Library::open_in_memory().unwrap();
    if models {
        lib.add(&ModelInfo::bare(ProviderKind::OpenRouter, "m")).unwrap();
    }
    let deps = CrewDeps {
        brains: Arc::new(Brains::new(lib, factory)),
        tools: Arc::new(|_: &Sandbox, _: Option<&Path>| Arc::new(NoTools) as Arc<dyn ToolHost>),
        approvals: Arc::new(Deny),
        mind: None,
        gpu_busy: Arc::new(move || gpu_busy),
        research: Some(Arc::new(Web)),
    };
    let (tx, rx) = crossbeam_channel::unbounded();
    (Supervisor::start(home, deps, cfg, tx).unwrap(), rx)
}

fn temp_home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("marlowe-crew-v2-{name}-{}", RunId::new_random().short()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn wait_for(sup: &Supervisor, id: &RunId, status: CrewRunStatus) -> CrewRunInfo {
    let end = Instant::now() + Duration::from_secs(30);
    loop {
        let info = sup.get(id).unwrap().unwrap();
        if info.status == status {
            return info;
        }
        assert!(Instant::now() < end, "timed out waiting for {status:?}; run is {:?} ({:?})", info.status, info.note);
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn dispatch(sup: &Supervisor, goal: &str) -> RunId {
    let args = DispatchArgs {
        role: "researcher".into(),
        goal: goal.into(),
        playbook: Some(Playbook::DeepResearch),
        workspace: None,
        budget: None,
        tainted: false,
    };
    sup.dispatch(Some(SessionId("s1".into())), args).unwrap().run_id
}

fn remove(dir: &Path) {
    for _ in 0..50 {
        if std::fs::remove_dir_all(dir).is_ok() || !dir.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn the_plan_waits_on_the_user_and_an_approved_run_reports_every_stage() {
    let home = temp_home("approve");
    let (sup, events) = start(&home);
    let id = dispatch(&sup, "What top-1 accuracy does model M reach on ImageNet?");

    let waiting = wait_for(&sup, &id, CrewRunStatus::NeedsYou);
    assert!(waiting.note.as_deref().is_some_and(|n| n.starts_with("research plan ready")), "{:?}", waiting.note);
    let (plan, quote) = sup.pending_plan(&id).expect("a plan awaits approval");
    assert_eq!(plan.sub_questions.len(), 1);
    assert!(quote.model_tokens > 0);
    let bad = ResearchPlan { breadth: 0, ..plan.clone() };
    let e = sup.approve_plan(&id, PlanDecision::Edit { plan: bad }).unwrap_err();
    assert!(e.to_string().contains("not runnable"), "{e}");
    assert!(sup.pending_plan(&id).is_some(), "a bad edit leaves the plan waiting");
    sup.approve_plan(&id, PlanDecision::Approve).unwrap();

    let done = wait_for(&sup, &id, CrewRunStatus::Done);
    // Approving the plan approved its quote: the researcher card's 800k tokens already cover this
    // small one, so only a quote above the card would raise it; it never falls.
    let card_tokens = waiting.budget.tokens.unwrap();
    assert_eq!(done.budget.tokens, Some(card_tokens.max(quote.model_tokens + quote.model_tokens / 4)));
    assert!(done.budget.steps.unwrap() >= waiting.budget.steps.unwrap());
    let result = done.result.expect("a result");
    assert_eq!(result.confidence, Confidence::High, "{}", result.summary);
    assert!(result.artifacts.iter().any(|a| a.ends_with("report.md")));
    assert!(done.tainted, "channel results and pages taint the run");
    assert!(sup.approve_plan(&id, PlanDecision::Approve).is_err(), "nothing waits any more");

    let evs: Vec<CrewEvent> = events.try_iter().collect();
    assert!(evs
        .iter()
        .any(|e| matches!(e, CrewEvent::PlanProposed { run, session: Some(s), .. } if *run == id && s.0 == "s1")));
    for stage in ResearchStage::ALL {
        assert!(
            evs.iter().any(|e| matches!(e, CrewEvent::Research { progress, .. } if progress.stage == stage && progress.state == StageState::Done)),
            "{stage:?} reported done"
        );
    }
    let (_, steps) = sup.transcript(&id).unwrap();
    assert!(
        steps.iter().any(|s| s.action == "plan proposed" && s.role == "playbook"),
        "data checkpoints are transcript rows"
    );
    sup.shutdown();
    drop(sup);
    remove(&home);
}

#[test]
fn a_rejected_plan_ends_the_run_cancelled_with_the_reason() {
    let home = temp_home("reject");
    let (sup, _events) = start(&home);
    let id = dispatch(&sup, "Survey every vision model ever");
    wait_for(&sup, &id, CrewRunStatus::NeedsYou);
    sup.approve_plan(&id, PlanDecision::Reject { reason: Some("too broad".into()) }).unwrap();
    let run = wait_for(&sup, &id, CrewRunStatus::Cancelled);
    assert_eq!(run.note.as_deref(), Some("plan rejected: too broad"));
    assert!(run.result.is_none());
    sup.shutdown();
    drop(sup);
    remove(&home);
}

/// The PC crashed with a plan waiting (ui-preview5): the stored run is still `needs_you` and the
/// plan lived only in memory. The next start has it pending again from its checkpoint, with no
/// second draft, and approving it runs the research.
#[test]
fn a_plan_waiting_through_a_crash_is_asked_for_again_on_the_next_start() {
    let home = temp_home("crash");
    let (sup, _events) = start(&home);
    let id = dispatch(&sup, "What top-1 accuracy does model M reach on ImageNet?");
    let waiting = wait_for(&sup, &id, CrewRunStatus::NeedsYou);
    let (plan, _) = sup.pending_plan(&id).expect("a plan awaits approval");
    sup.shutdown();
    drop(sup);
    // A crash leaves the record as it was while waiting; a clean stop would have marked it running.
    let store = RunStore::open(&home.join("cowork.db")).unwrap();
    let mut run = store.get(&id).unwrap().unwrap();
    run.status = CrewRunStatus::NeedsYou;
    run.note = waiting.note.clone();
    store.save(&run).unwrap();
    let drafts = |s: &RunStore| s.steps(&id).unwrap().iter().filter(|r| r.key == "v2:plan").count();
    let before = drafts(&store);
    drop(store);

    let (sup, events) = start(&home);
    wait_for(&sup, &id, CrewRunStatus::NeedsYou);
    let (again, _) = sup.pending_plan(&id).expect("the plan is waiting again, not paused");
    assert_eq!(again, plan, "the checkpointed proposal, not a new draft");
    assert!(events.try_iter().any(|e| matches!(e, CrewEvent::PlanProposed { run, .. } if run == id)));
    sup.approve_plan(&id, PlanDecision::Approve).unwrap();
    wait_for(&sup, &id, CrewRunStatus::Done);
    sup.shutdown();
    drop(sup);
    let store = RunStore::open(&home.join("cowork.db")).unwrap();
    assert_eq!(drafts(&store), before, "no second model call drafted the plan");
    drop(store);
    remove(&home);
}

/// A waiting plan holds no concurrency slot: with one run per provider, a second research run
/// still drafts its plan while the first waits, and neither has a thread.
#[test]
fn a_waiting_plan_frees_its_slot_for_the_next_run() {
    let home = temp_home("slot");
    let cfg = CrewConfig { per_provider: 1, ..CrewConfig::default() };
    let (sup, _events) = start_with(&home, true, false, cfg);
    let a = dispatch(&sup, "What top-1 accuracy does model M reach on ImageNet?");
    let b = dispatch(&sup, "What top-5 accuracy does model M reach on ImageNet?");
    wait_for(&sup, &a, CrewRunStatus::NeedsYou);
    wait_for(&sup, &b, CrewRunStatus::NeedsYou);
    assert!(sup.pending_plan(&a).is_some() && sup.pending_plan(&b).is_some());
    let end = Instant::now() + Duration::from_secs(10);
    while sup.running() > 0 {
        assert!(Instant::now() < end, "a parked run keeps no thread");
        std::thread::sleep(Duration::from_millis(10));
    }
    sup.approve_plan(&b, PlanDecision::Approve).unwrap();
    wait_for(&sup, &b, CrewRunStatus::Done);
    assert!(sup.pending_plan(&a).is_some(), "a still waits");
    sup.shutdown();
    drop(sup);
    remove(&home);
}

/// After a crash, every waiting plan is back from its checkpoint without a model, the GPU or a
/// slot: with the GPU busy and no model to be had, both plans are pending again, both runs
/// `needs_you`, and none was failed or queued for want of a model.
#[test]
fn every_waiting_plan_is_back_after_a_crash_with_no_model_and_the_gpu_busy() {
    let home = temp_home("crash2");
    let (sup, _events) = start(&home);
    let a = dispatch(&sup, "What top-1 accuracy does model M reach on ImageNet?");
    let b = dispatch(&sup, "What top-5 accuracy does model M reach on ImageNet?");
    wait_for(&sup, &a, CrewRunStatus::NeedsYou);
    wait_for(&sup, &b, CrewRunStatus::NeedsYou);
    let (plan_a, _) = sup.pending_plan(&a).unwrap();
    let (plan_b, _) = sup.pending_plan(&b).unwrap();
    // A crash: no shutdown, the store as it was.
    drop(sup);

    let (sup, events) = start_with(&home, false, true, CrewConfig::default());
    std::thread::sleep(Duration::from_millis(1500)); // the scheduler has run
    let listed = sup.list(50).unwrap();
    for (id, plan) in [(&a, &plan_a), (&b, &plan_b)] {
        let info = listed.iter().find(|r| &r.id == id).expect("listed");
        assert_eq!(info.status, CrewRunStatus::NeedsYou, "{:?}", info.note);
        assert_eq!(sup.pending_plan(id).map(|(p, _)| p).as_ref(), Some(plan));
    }
    let proposed: Vec<RunId> = events
        .try_iter()
        .filter_map(|e| match e {
            CrewEvent::PlanProposed { run, .. } => Some(run),
            _ => None,
        })
        .collect();
    assert!(proposed.contains(&a) && proposed.contains(&b), "{proposed:?}");
    // Rejecting a parked plan ends its run at once, with no thread.
    sup.approve_plan(&a, PlanDecision::Reject { reason: None }).unwrap();
    assert_eq!(sup.get(&a).unwrap().unwrap().status, CrewRunStatus::Cancelled);
    assert_eq!(sup.get(&a).unwrap().unwrap().note.as_deref(), Some("plan rejected"));
    sup.shutdown();
    drop(sup);
    remove(&home);
}
