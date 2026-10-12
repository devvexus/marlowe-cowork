//! Playbook tests: every model is scripted, every repository is a temp git repo. No network, no
//! inference.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use marlowe_agent::{Consequence, ToolCallReq, ToolCtx, ToolHost, ToolOutcome};
use marlowe_model::{
    fold_events, CancelToken, ChatRequest, Completion, Message, ModelInfo, Provider, ProviderError, ProviderKind, Role,
    StopReason, StreamEvent, ToolSpec,
};
use marlowe_proto::{Confidence, RunId, SandboxKind};
use serde_json::{json, Value};

use super::*;
use crate::role::{RoleCard, Roles};
use crate::sandbox::Sandbox;

// ---------------------------------------------------------------- scripted models

type Respond = dyn Fn(&ChatRequest, usize) -> Vec<StreamEvent> + Send + Sync;

/// Answers each request with `respond(request, call index)`; records every request.
struct Scripted {
    respond: Box<Respond>,
    calls: AtomicUsize,
    requests: Mutex<Vec<ChatRequest>>,
    /// Requests in flight now, and the most ever at once.
    inflight: AtomicUsize,
    peak: AtomicUsize,
    delay: Duration,
}

impl Scripted {
    fn new(respond: impl Fn(&ChatRequest, usize) -> Vec<StreamEvent> + Send + Sync + 'static) -> Arc<Scripted> {
        Arc::new(Scripted {
            respond: Box::new(respond),
            calls: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
            inflight: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            delay: Duration::ZERO,
        })
    }

    /// Replies from a fixed queue, in order.
    fn queue(steps: Vec<Vec<StreamEvent>>) -> Arc<Scripted> {
        let steps = Mutex::new(std::collections::VecDeque::from(steps));
        Scripted::new(move |req, _| {
            steps.lock().unwrap().pop_front().unwrap_or_else(|| panic!("the script ran out at: {:?}", req.messages.last()))
        })
    }

    fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Provider for Scripted {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenRouter
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![model()])
    }

    fn stream(
        &self,
        req: &ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<Completion, ProviderError> {
        let now = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        self.requests.lock().unwrap().push(req.clone());
        std::thread::sleep(self.delay);
        let evs = (self.respond)(req, self.calls.fetch_add(1, Ordering::SeqCst));
        self.inflight.fetch_sub(1, Ordering::SeqCst);
        for ev in &evs {
            if cancel.is_cancelled() {
                return Err(ProviderError::cancelled());
            }
            on(ev.clone());
        }
        Ok(fold_events(&evs))
    }
}

fn model() -> ModelInfo {
    let mut m = ModelInfo::bare(ProviderKind::OpenRouter, "scripted");
    m.supports_tools = true;
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

/// The text of the request's first user message (the step prompt).
fn prompt_of(req: &ChatRequest) -> String {
    req.messages.iter().find(|m| m.role == Role::User).map(Message::joined_text).unwrap_or_default()
}

/// Whether the request already carries a tool result (the second call of a tool-using step).
fn after_tool(req: &ChatRequest) -> bool {
    req.messages.last().is_some_and(|m| m.role == Role::Tool)
}

/// One scripted provider per role.
fn resolver(by_role: Vec<(&str, Arc<Scripted>)>) -> ModelResolver {
    let map: Vec<(String, Arc<Scripted>)> = by_role.into_iter().map(|(r, p)| (r.to_string(), p)).collect();
    Arc::new(move |card: &RoleCard| {
        let p = map
            .iter()
            .find(|(r, _)| *r == card.name)
            .map(|(_, p)| p.clone())
            .ok_or_else(|| CrewError::UnknownRole(card.name.clone()))?;
        Ok(StepModel { provider: p, model: model() })
    })
}

// ---------------------------------------------------------------- tools

/// read / write / glob / grep on real files, and a canned web_search that taints.
struct TestTools {
    searches: AtomicUsize,
}

impl TestTools {
    fn new() -> Arc<TestTools> {
        Arc::new(TestTools { searches: AtomicUsize::new(0) })
    }
}

fn spec(name: &str) -> ToolSpec {
    ToolSpec { name: name.into(), description: format!("{name} tool"), input_schema: json!({"type": "object"}) }
}

impl ToolHost for TestTools {
    fn specs(&self) -> Vec<ToolSpec> {
        ["read", "write", "edit", "glob", "grep", "bash", "web_search", "web_fetch", "dispatch"].map(spec).to_vec()
    }

    fn call(&self, call: &ToolCallReq, _ctx: &ToolCtx) -> ToolOutcome {
        let path = call.args.get("path").and_then(Value::as_str).unwrap_or("");
        match call.name.as_str() {
            "write" => {
                let content = call.args.get("content").and_then(Value::as_str).unwrap_or("");
                match std::fs::write(path, content) {
                    Ok(()) => ToolOutcome::ok("written", "1 file"),
                    Err(e) => ToolOutcome::error(e.to_string()),
                }
            }
            "read" => match std::fs::read_to_string(path) {
                Ok(t) => ToolOutcome::ok(t, "read"),
                Err(e) => ToolOutcome::error(e.to_string()),
            },
            "web_search" => {
                self.searches.fetch_add(1, Ordering::SeqCst);
                ToolOutcome::ok("1. Paper A — https://arxiv.org/abs/2401.00001", "1 result")
            }
            other => ToolOutcome::error(format!("{other} is not scripted")),
        }
    }

    fn consequence(&self, _call: &ToolCallReq) -> Consequence {
        Consequence::Free
    }

    fn taints(&self, call: &ToolCallReq) -> bool {
        call.name == "web_search" || call.name == "web_fetch"
    }
}

// ---------------------------------------------------------------- fixtures

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("marlowe-playbook-{name}-{}", RunId::new_random().short()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn scratch(root: &Path) -> Sandbox {
    Sandbox { kind: SandboxKind::Scratch, root: root.to_path_buf(), origin: None, branch: None, base: None, repo_digest: None, frozen: Vec::new(), target_dir: None }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").args(args).current_dir(dir).output().expect("git runs");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A repo with one commit holding `value.txt` = `1`, and a worktree on `crew/<run>` cut from it.
fn repo_with_worktree(name: &str, run: &RunId) -> (PathBuf, Sandbox) {
    let base = temp_dir(name);
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "Test"]);
    git(&repo, &["config", "user.email", "test@example.invalid"]);
    git(&repo, &["config", "core.autocrlf", "false"]);
    std::fs::write(repo.join("value.txt"), line("1")).unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "start"]);
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let wt = base.join("worktrees").join(run.as_str());
    let branch = crate::sandbox::branch_name(run);
    git(&repo, &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", &branch]);
    let sandbox = Sandbox {
        kind: SandboxKind::Worktree,
        root: wt,
        origin: Some(repo.clone()),
        branch: Some(branch),
        base: Some(head),
        repo_digest: crate::sandbox::repo_digest(&repo),
        frozen: Vec::new(),
        target_dir: None,
    };
    (repo, sandbox)
}

/// A line of `value.txt`. CRLF on Windows: `findstr /x` does not match LF-only lines.
fn line(v: &str) -> String {
    if cfg!(windows) {
        format!("{v}\r\n")
    } else {
        format!("{v}\n")
    }
}

/// Passes when `value.txt` holds a line that is exactly `2`.
fn value_is_two() -> String {
    if cfg!(windows) {
        "findstr /x 2 value.txt".into()
    } else {
        "grep -qx 2 value.txt".into()
    }
}

/// Records the hooks' calls.
#[derive(Default)]
struct Recorder {
    finished: Mutex<Vec<(String, usize, bool)>>,
    progress: Mutex<Vec<String>>,
    stored: Mutex<Vec<(String, StepResult)>>,
}

impl StepHooks for Recorder {
    fn completed(&self, key: &str) -> Option<StepResult> {
        self.stored.lock().unwrap().iter().find(|(k, _)| k == key).map(|(_, r)| r.clone())
    }

    fn finished(&self, spec: &StepSpec, messages: &[Message], result: &StepResult) {
        self.finished.lock().unwrap().push((spec.key.clone(), messages.len(), result.tainted));
    }

    fn progress(&self, action: &str) {
        self.progress.lock().unwrap().push(action.to_string());
    }
}

/// Writes the Result memory a finished run leaves (`result_memory`) into a model-free Mind under
/// `home`, and reads it back.
fn remember_result(
    home: &Path,
    role: &str,
    playbook: Playbook,
    workspace: Option<PathBuf>,
    tainted: bool,
    d: &Dispatch,
) -> marlowe_mind::RecallHit {
    let cfg = marlowe_mind::MindConfig { models_dir: Some(home.join("no-models")), ..Default::default() };
    let mind = marlowe_mind::Mind::open_with(&home.join("mind"), cfg).unwrap();
    let card = Roles::builtin().get(role).unwrap().clone();
    let mut run = crate::Run::new(None, &card, "goal", Some(playbook), workspace, Default::default());
    run.tainted = tainted;
    let mem = crate::result_memory(&run, d);
    assert_eq!(mem.kind, marlowe_mind::Kind::Result);
    let id = mind.remember(mem).unwrap().id;
    mind.get(&id).unwrap().expect("the Result memory is stored")
}

// ---------------------------------------------------------------- deep_research

const SUBS: [&str; 3] = ["How fast is X?", "What does Y cost?", "Who maintains Z?"];
const CLAIM_X: &str = "X runs at 40 tokens/s on one GPU ([Paper A](https://arxiv.org/abs/2401.00001)) (load-bearing)";
const CLAIM_Y: &str = "Y costs $3 per million tokens ([Blog B](https://blog.example/y-pricing)) (load-bearing)";
const CLAIM_Z: &str = "Z is maintained by the Q lab ([Repo Z](https://github.com/q/z))";

/// A research runner and the scripted parts behind it.
struct Research {
    runner: AgentRunner,
    researcher: Arc<Scripted>,
    scout: Arc<Scripted>,
    reviewer: Arc<Scripted>,
    hooks: Arc<Recorder>,
    tools: Arc<TestTools>,
}

fn research_runner(sandbox: Sandbox) -> Research {
    let researcher = Scripted::new(|req, _| {
        let p = prompt_of(req);
        if p.contains("Plan the investigation") {
            answer(&format!("Plan:\n1. {}\n2. {}\n3. {}", SUBS[0], SUBS[1], SUBS[2]))
        } else {
            answer(
                "# X, Y and Z\n\nX is fast enough: 40 tokens/s on one GPU \
                 ([Paper A](https://arxiv.org/abs/2401.00001)). Z is maintained by the Q lab \
                 ([Repo Z](https://github.com/q/z)), though that rests on one source.\n\n\
                 ## Cost\n\nNo verified figure for Y survived review.\n\nConfidence: high",
            )
        }
    });
    let mut scout = Scripted::new(|req, _| {
        let p = prompt_of(req);
        if p.contains(SUBS[0]) {
            if after_tool(req) {
                answer(&format!("1. {CLAIM_X}"))
            } else {
                call("web_search", json!({"query": "X throughput"}))
            }
        } else if p.contains(SUBS[1]) {
            answer(&format!("1. {CLAIM_Y}"))
        } else {
            answer(&format!("Found one source.\n\n1. {CLAIM_Z}"))
        }
    });
    Arc::get_mut(&mut scout).unwrap().delay = Duration::from_millis(80);
    let reviewer = Scripted::new(|_, _| {
        answer("1. supported\n2. refuted: the post quotes a promotional price; the pricing page says $5\n3. contested: one source only")
    });
    let hooks = Arc::new(Recorder::default());
    let tools = TestTools::new();
    let mut runner = AgentRunner::new(
        RunId::new_random(),
        Roles::builtin(),
        resolver(vec![("researcher", researcher.clone()), ("scout", scout.clone()), ("reviewer", reviewer.clone())]),
        tools.clone(),
        sandbox,
        None,
    );
    runner.hooks = hooks.clone();
    Research { runner, researcher, scout, reviewer, hooks, tools }
}

#[test]
fn deep_research_writes_a_cited_report_and_drops_the_refuted_claim() {
    let dir = temp_dir("research");
    let Research { runner, researcher, scout, reviewer, hooks, tools } = research_runner(scratch(&dir));
    let cfg = PlaybookConfig::default();

    let d = run(Some(Playbook::DeepResearch), "researcher", "Should we use X, Y or Z?", &runner, &cfg).unwrap();

    // The report is on disk in the sandbox and is the dispatch's artifact.
    let report_path = dir.join(REPORT_FILE);
    let report = std::fs::read_to_string(&report_path).unwrap();
    assert!(report.starts_with("# X, Y and Z") && report.contains("Confidence: high"));
    assert_eq!(d.artifacts, vec![report_path.display().to_string()]);

    // Three scouts ran, two at a time at most (cloud provider limit), one searched the web.
    assert_eq!(scout.requests().len(), 4, "three scouts, one with a tool round trip");
    assert!(scout.peak.load(Ordering::SeqCst) <= 2, "per-provider limit holds");
    assert!(scout.peak.load(Ordering::SeqCst) >= 2, "scouts ran concurrently");
    assert_eq!(tools.searches.load(Ordering::SeqCst), 1);

    // The reviewer saw every claim, numbered.
    let verify = prompt_of(&reviewer.requests()[0]);
    for (i, c) in [CLAIM_X, CLAIM_Y, CLAIM_Z].iter().enumerate() {
        assert!(verify.contains(&format!("{}. {c}", i + 1)), "claim {} in the verify prompt", i + 1);
    }

    // The synthesis was given the surviving claims, the contested one flagged, the refuted one
    // only under "do not use".
    let synth = prompt_of(&researcher.requests()[1]);
    let (kept, dropped) = synth.split_once("Refuted and dropped").expect("a dropped section");
    assert!(kept.contains(CLAIM_X));
    assert!(kept.contains(&format!("{CLAIM_Z} [contested: one source only]")));
    assert!(!kept.contains(CLAIM_Y), "the refuted claim is not offered as evidence");
    assert!(dropped.contains(CLAIM_Y) && dropped.contains("the pricing page says $5"));

    // Citations come from the report; the refuted claim's source is not among them.
    let targets: Vec<&str> = d.citations.iter().map(|c| c.url_or_path.as_str()).collect();
    assert_eq!(targets, ["https://arxiv.org/abs/2401.00001", "https://github.com/q/z"]);
    assert_eq!(d.confidence, Confidence::High);
    assert!(d.summary.starts_with("X is fast enough"));
    assert!(d.summary.contains("1 of 3 claims supported, 1 contested or unverified, 1 refuted and dropped; 3 scouts."));
    assert!(d.diff.is_none());

    // Every agent step reached the hooks; the searching scout is the tainted one.
    let finished = hooks.finished.lock().unwrap().clone();
    let keys: Vec<&str> = finished.iter().map(|(k, _, _)| k.as_str()).collect();
    for k in ["plan", "scout:1", "scout:2", "scout:3", "verify", "synthesize"] {
        assert!(keys.contains(&k), "{k} finished");
    }
    assert!(finished.iter().any(|(k, _, t)| k == "scout:1" && *t));
    assert!(finished.iter().all(|(k, _, t)| k == "scout:1" || !*t));
    assert!(hooks.progress.lock().unwrap().iter().any(|p| p == "scout 2/3"));

    // Steps are narrowed: plan and synthesis see read-only tools, nobody sees dispatch.
    let plan_tools: Vec<String> = researcher.requests()[0].tools.iter().map(|t| t.name.clone()).collect();
    assert_eq!(plan_tools, ["read", "glob", "grep"]);
    for req in scout.requests().iter().chain(reviewer.requests().iter()) {
        assert!(!req.tools.iter().any(|t| t.name == "dispatch" || t.name == "write"));
    }

    // The finished run's Result memory: the finding with its sources, at untrusted trust because
    // a scout read the web.
    let hit = remember_result(&dir, "researcher", Playbook::DeepResearch, None, true, &d);
    assert!(hit.text.contains("X is fast enough") && hit.text.contains("https://arxiv.org/abs/2401.00001"));
    assert!(!hit.text.contains("blog.example"));
    assert_eq!(hit.trust, marlowe_mind::Trust::Untrusted);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_resumed_research_run_skips_completed_steps() {
    let dir = temp_dir("resume");
    let Research { runner, researcher, hooks, .. } = research_runner(scratch(&dir));
    hooks.stored.lock().unwrap().push((
        "plan".into(),
        StepResult { text: format!("1. {}\n2. {}\n3. {}", SUBS[0], SUBS[1], SUBS[2]), ..Default::default() },
    ));
    let d = deep_research(&runner, "Should we use X, Y or Z?", &PlaybookConfig::default()).unwrap();
    assert_eq!(researcher.requests().len(), 1, "only the synthesis called the model");
    assert!(prompt_of(&researcher.requests()[0]).contains("Write the report"));
    assert_eq!(d.confidence, Confidence::High);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_empty_plan_falls_back_to_the_goal_and_a_long_one_is_capped() {
    let dir = temp_dir("cap");
    let researcher = Scripted::new(|req, _| {
        let p = prompt_of(req);
        if p.contains("Plan the investigation") && p.contains("Is W real?") {
            answer("I would rather just look it up.")
        } else if p.contains("Plan the investigation") {
            answer(&(1..=8).map(|i| format!("{i}. Question {i}?")).collect::<Vec<_>>().join("\n"))
        } else {
            answer("Nothing solid.\n\nConfidence: high")
        }
    });
    let scout = Scripted::new(|_, _| answer("Found nothing; searched arXiv and the web."));
    let reviewer = Scripted::queue(vec![]);
    let runner = AgentRunner::new(
        RunId::new_random(),
        Roles::builtin(),
        resolver(vec![("researcher", researcher.clone()), ("scout", scout.clone()), ("reviewer", reviewer.clone())]),
        TestTools::new(),
        scratch(&dir),
        None,
    );
    let d = deep_research(&runner, "Is W real?", &PlaybookConfig::default()).unwrap();
    assert_eq!(scout.requests().len(), 1);
    assert!(prompt_of(&scout.requests()[0]).contains("Your sub-question: Is W real?"));
    assert!(reviewer.requests().is_empty(), "no claims, nothing to verify");
    assert_eq!(d.confidence, Confidence::Low, "no supported claim caps confidence");
    assert!(d.summary.contains("1 sub-question(s) came back empty"));

    // An eight-item plan gets `max_scouts` scouts, the first ones.
    let cfg = PlaybookConfig { max_scouts: 4, ..PlaybookConfig::default() };
    deep_research(&runner, "Survey everything.", &cfg).unwrap();
    let asked: Vec<String> = scout.requests()[1..].iter().map(prompt_of).collect();
    assert_eq!(asked.len(), 4);
    for i in 1..=4 {
        assert!(asked.iter().any(|p| p.contains(&format!("Your sub-question: Question {i}?"))));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn verdict_lines_parse_leniently() {
    let v = parse_verdicts(
        "Verdicts:\n1. supported\n- 2) **refuted**: Table 2 says 71.2\n**3.** Contested — holds on GLUE only\n\
         4. unverified (paywalled)\n5. looks fine\n2. supported\n",
    );
    assert_eq!(v.get(&1), Some(&Verdict::Supported));
    assert_eq!(v.get(&2), Some(&Verdict::Refuted("Table 2 says 71.2".into())), "first verdict wins");
    assert_eq!(v.get(&3), Some(&Verdict::Contested("holds on GLUE only".into())));
    assert_eq!(v.get(&4), Some(&Verdict::Unverified("paywalled".into())));
    assert_eq!(v.get(&5), None);
    assert_eq!(first_paragraph("# T\n\nThe answer is\nyes.\n\nMore.", 100), "The answer is yes.");
    assert_eq!(first_paragraph("word word word", 9), "word word…");
    assert_eq!(min_confidence(Confidence::High, Confidence::Medium), Confidence::Medium);
}

// ---------------------------------------------------------------- build

#[test]
fn build_iterates_a_failing_test_to_green_and_hands_back_a_diff() {
    let run_id = RunId::new_random();
    let (repo, sandbox) = repo_with_worktree("build", &run_id);
    let file = sandbox.root.join("value.txt").display().to_string();
    let engineer = Scripted::queue(vec![
        answer("Plan: set value.txt to 2; the check is the test command."),
        call("write", json!({"path": file, "content": line("3")})),
        answer("Set value.txt to 3."),
        call("write", json!({"path": file, "content": line("2")})),
        answer("The test wanted 2, not 3. value.txt now holds 2."),
    ]);
    let reviewer = Scripted::queue(vec![answer("The change matches the goal. No blocking issues.")]);
    let hooks = Arc::new(Recorder::default());
    let mut runner = AgentRunner::new(
        run_id.clone(),
        Roles::builtin(),
        resolver(vec![("engineer", engineer.clone()), ("reviewer", reviewer.clone())]),
        TestTools::new(),
        sandbox.clone(),
        Some(repo.clone()),
    );
    runner.hooks = hooks.clone();
    let cfg = PlaybookConfig { test_command: Some(value_is_two()), ..PlaybookConfig::default() };

    let d = run(Some(Playbook::Build), "engineer", "Make value.txt hold 2.", &runner, &cfg).unwrap();

    // The failing test went back to the engineer with its output, once.
    let reqs = engineer.requests();
    assert_eq!(reqs.len(), 5);
    let fix = prompt_of(&reqs[3]);
    assert!(fix.contains(&format!("`{}` failed (exit 1)", value_is_two())), "{fix}");
    let keys: Vec<String> = hooks.finished.lock().unwrap().iter().map(|(k, _, _)| k.clone()).collect();
    assert_eq!(keys, ["plan", "implement", "test:1", "review"]);

    // The reviewer read the real diff.
    let review = prompt_of(&reviewer.requests()[0]);
    assert!(review.contains("-1") && review.contains("+2"), "{review}");
    assert!(review.contains("green after 1 fix round"));

    // The dispatch carries the diff summary; the original checkout is untouched.
    let diff = d.diff.clone().expect("a diff summary");
    assert_eq!(diff.branch, format!("crew/{run_id}"));
    assert_eq!(diff.files, ["value.txt"]);
    assert_eq!((diff.insertions, diff.deletions), (1, 1));
    assert_eq!(d.confidence, Confidence::High);
    assert!(d.summary.starts_with("The test wanted 2"));
    assert!(d.summary.contains("green after 1 fix round") && d.summary.contains("needs a merge decision"));
    assert_eq!(d.artifacts, vec![sandbox.root.join("value.txt").display().to_string()]);
    assert_eq!(std::fs::read_to_string(repo.join("value.txt")).unwrap(), line("1"));

    // An untainted build is remembered as the crew's inference, scoped to the repo.
    let home = temp_dir("build-mind");
    let hit = remember_result(&home, "engineer", Playbook::Build, Some(repo.clone()), false, &d);
    assert!(hit.text.contains("The test wanted 2") && hit.text.contains("Confidence: high"));
    assert_eq!(hit.trust, marlowe_mind::Trust::Inferred);
    assert_eq!(hit.scope, marlowe_mind::Scope::Project(repo.display().to_string()));
    let _ = sandbox.discard();
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn build_stops_red_after_max_fixes_and_says_so() {
    let run_id = RunId::new_random();
    let (repo, sandbox) = repo_with_worktree("red", &run_id);
    let file = sandbox.root.join("value.txt").display().to_string();
    let mut script = vec![answer("Plan."), call("write", json!({"path": file, "content": line("3")})), answer("Done.")];
    for i in 0..2 {
        script.push(answer(&format!("Tried again ({i}).")));
    }
    script.push(answer("It is 3 because I believe 3 is right."));
    let engineer = Scripted::queue(script);
    let reviewer = Scripted::queue(vec![answer("1. value.txt:1 holds 3, the goal says 2. Blocking.")]);
    let runner = AgentRunner::new(
        run_id,
        Roles::builtin(),
        resolver(vec![("engineer", engineer.clone()), ("reviewer", reviewer)]),
        TestTools::new(),
        sandbox.clone(),
        Some(repo),
    );
    let cfg = PlaybookConfig { max_test_iterations: 2, test_command: Some(value_is_two()), ..PlaybookConfig::default() };
    let d = build(&runner, "Make value.txt hold 2.", &cfg).unwrap();
    assert_eq!(d.confidence, Confidence::Low);
    assert!(d.summary.contains("still failing after 2 fix rounds"), "{}", d.summary);
    assert!(d.summary.contains("Last failing output"));
    assert!(d.summary.contains("blocking issues"));
    assert_eq!(engineer.requests().len(), 6, "plan, implement x2, two test fixes, one review fix");
    let _ = sandbox.discard();
}

#[test]
fn a_resumed_build_counts_the_fix_rounds_it_already_made_and_leads_with_the_fix() {
    // Killed during review after: implement (wrong) → test red → `test:1` fix → test green.
    let run_id = RunId::new_random();
    let (repo, sandbox) = repo_with_worktree("build-resume", &run_id);
    std::fs::write(sandbox.root.join("value.txt"), line("2")).unwrap();
    let engineer = Scripted::queue(vec![]);
    let reviewer = Scripted::queue(vec![answer("No blocking issues.")]);
    let hooks = Arc::new(Recorder::default());
    for (key, text) in [
        ("plan", "Plan: set value.txt to 2."),
        ("implement", "Set value.txt to 3."),
        ("test:1", "The test wanted 2, not 3. value.txt now holds 2."),
    ] {
        hooks.stored.lock().unwrap().push((key.into(), StepResult { text: text.into(), ..Default::default() }));
    }
    let mut runner = AgentRunner::new(
        run_id,
        Roles::builtin(),
        resolver(vec![("engineer", engineer.clone()), ("reviewer", reviewer.clone())]),
        TestTools::new(),
        sandbox.clone(),
        Some(repo),
    );
    runner.hooks = hooks.clone();
    let cfg = PlaybookConfig { test_command: Some(value_is_two()), ..PlaybookConfig::default() };
    let d = build(&runner, "Make value.txt hold 2.", &cfg).unwrap();
    assert!(d.summary.starts_with("The test wanted 2"), "the fix leads, not the stale implement text: {}", d.summary);
    assert!(d.summary.contains("green after 1 fix round"), "{}", d.summary);
    assert!(prompt_of(&reviewer.requests()[0]).contains("green after 1 fix round"));
    assert_eq!(engineer.requests().len(), 0, "no completed step ran again");
    let _ = sandbox.discard();
}

// ---------------------------------------------------------------- single, shell

#[test]
fn a_role_prompt_dispatch_is_one_step_and_resumes_from_its_checkpoint() {
    let dir = temp_dir("single");
    let analyst = Scripted::queue(vec![answer(
        "The loss spike at step 4k is the LR warmup ending ([log](runs/a.log)).\n\nConfidence: medium",
    )]);
    let hooks = Arc::new(Recorder::default());
    let mut runner = AgentRunner::new(
        RunId::new_random(),
        Roles::builtin(),
        resolver(vec![("analyst", analyst.clone())]),
        TestTools::new(),
        scratch(&dir),
        None,
    );
    runner.hooks = hooks.clone();
    let d = run(None, "analyst", "Why did the loss spike?", &runner, &PlaybookConfig::default()).unwrap();
    assert_eq!(d.confidence, Confidence::Medium);
    assert_eq!(d.citations.len(), 1);
    assert!(d.summary.starts_with("The loss spike"));
    let sys: String = analyst.requests()[0].system.iter().map(|b| b.text.clone()).collect();
    assert!(sys.contains("You are the crew's analyst") && sys.contains("Your scratch directory"));

    // The same key again comes from the checkpoint, with no model call.
    hooks.stored.lock().unwrap().push(("main".into(), StepResult { text: "stored".into(), ..Default::default() }));
    let d = single(&runner, "analyst", "Why did the loss spike?").unwrap();
    assert_eq!(d.summary, "stored");
    assert_eq!(analyst.requests().len(), 1);
    hooks.stored.lock().unwrap().clear();
    assert!(matches!(single(&runner, "nobody", "x"), Err(CrewError::UnknownRole(_))));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_sandbox_shell_runs_in_the_sandbox_without_api_keys() {
    let dir = temp_dir("shell");
    let sb = scratch(&dir);
    std::env::set_var("OPENROUTER_API_KEY", "sk-or-test-secret");
    let cmd = if cfg!(windows) { "cd & echo key=%OPENROUTER_API_KEY%" } else { "pwd; echo key=$OPENROUTER_API_KEY" };
    let r = sandbox_shell(&sb, cmd, &CancelToken::new()).unwrap();
    assert!(r.ok && r.exit_code == Some(0), "{r:?}");
    assert!(!r.output.contains("sk-or-test-secret"), "{}", r.output);
    let leaf = dir.file_name().unwrap().to_str().unwrap();
    assert!(r.output.contains(leaf), "cwd is the sandbox: {}", r.output);
    let r = sandbox_shell(&sb, "exit 3", &CancelToken::new()).unwrap();
    assert_eq!((r.ok, r.exit_code), (false, Some(3)));
    let cancel = CancelToken::new();
    cancel.cancel();
    assert!(matches!(sandbox_shell(&sb, "exit 0", &cancel), Err(CrewError::Cancelled)));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- per-step sandbox

/// `write` into a fixed root, whatever the model's path.
struct Rooted {
    root: PathBuf,
}

impl ToolHost for Rooted {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![spec("write")]
    }

    fn call(&self, call: &ToolCallReq, _ctx: &ToolCtx) -> ToolOutcome {
        let path = self.root.join(call.args.get("path").and_then(Value::as_str).unwrap_or("out.txt"));
        match std::fs::write(path, "hi") {
            Ok(()) => ToolOutcome::ok("written", "1 file"),
            Err(e) => ToolOutcome::error(e.to_string()),
        }
    }

    fn consequence(&self, _call: &ToolCallReq) -> Consequence {
        Consequence::Free
    }
}

fn writer_step(key: &str, sandbox: Option<Sandbox>) -> StepSpec {
    StepSpec {
        key: key.into(),
        role: "engineer".into(),
        label: "implement".into(),
        prompt: "Write hello.txt".into(),
        tools: None,
        effort: None,
        max_output: None,
        sandbox,
    }
}

fn writing_engineer() -> Arc<Scripted> {
    Scripted::new(|req, _| if after_tool(req) { answer("done") } else { call("write", json!({"path": "hello.txt"})) })
}

#[test]
fn a_step_with_its_own_sandbox_runs_its_tools_and_prompt_there() {
    let (run_dir, step_dir) = (temp_dir("step-run"), temp_dir("step-own"));
    let engineer = writing_engineer();
    let mut runner = AgentRunner::new(
        RunId::new_random(),
        Roles::builtin(),
        resolver(vec![("engineer", engineer.clone())]),
        Arc::new(Rooted { root: run_dir.clone() }),
        scratch(&run_dir),
        None,
    );
    runner.tool_factory = Some(Arc::new(|sb: &Sandbox, _ws: Option<&Path>| {
        Arc::new(Rooted { root: sb.root.clone() }) as Arc<dyn ToolHost>
    }));

    // No sandbox on the step: the run's own.
    runner.step(writer_step("implement:t1", None)).unwrap();
    assert!(run_dir.join("hello.txt").exists() && !step_dir.join("hello.txt").exists());
    std::fs::remove_file(run_dir.join("hello.txt")).unwrap();

    // Its own: the tools are rooted there and the prompt names it.
    runner.step(writer_step("implement:t2", Some(scratch(&step_dir)))).unwrap();
    assert!(step_dir.join("hello.txt").exists() && !run_dir.join("hello.txt").exists());
    let sys: String = engineer.requests().last().unwrap().system.iter().map(|b| b.text.clone()).collect();
    assert!(sys.contains(&step_dir.display().to_string()), "{sys}");
    let _ = std::fs::remove_dir_all(&run_dir);
    let _ = std::fs::remove_dir_all(&step_dir);
}

#[test]
fn a_step_with_its_own_sandbox_and_no_tool_factory_fails_instead_of_writing_elsewhere() {
    let (run_dir, step_dir) = (temp_dir("nofactory-run"), temp_dir("nofactory-own"));
    let runner = AgentRunner::new(
        RunId::new_random(),
        Roles::builtin(),
        resolver(vec![("engineer", writing_engineer())]),
        Arc::new(Rooted { root: run_dir.clone() }),
        scratch(&run_dir),
        None,
    );
    let err = runner.step(writer_step("implement:t1", Some(scratch(&step_dir)))).unwrap_err();
    assert!(
        matches!(&err, CrewError::Step { step, message } if step == "implement:t1" && message.contains("tool factory")),
        "{err}"
    );
    assert!(!run_dir.join("hello.txt").exists() && !step_dir.join("hello.txt").exists());
    let _ = std::fs::remove_dir_all(&run_dir);
    let _ = std::fs::remove_dir_all(&step_dir);
}
