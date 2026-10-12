//! `Request::OpenPr` through the daemon over the real wire: a crew run in a real git worktree, a
//! scripted model for the crew step, and a recording `gh` tool host. The user is asked exactly
//! once, with the remote, branch and title; nothing is pushed or posted until they say yes. No
//! network, no GPU.

use std::io::BufReader;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use marlowe_agent::{Consequence, ToolCallReq, ToolCtx, ToolHost, ToolOutcome, UserInfo};
use marlowe_brains::{Library, MemoryStore, ProviderFactory};
use marlowe_crew::{CrewConfig, CrewSetup, RunId};
use marlowe_daemon::{Daemon, DaemonConfig, DaemonDeps};
use marlowe_model::{
    CancelToken, ChatRequest, Completion, CompletionBuilder, ModelInfo, Provider, ProviderError, ProviderKind, StopReason,
    StreamEvent, ToolSpec,
};
use marlowe_proto::{paths, write_line, ApprovalDecision, Envelope, Event, LineReader, Request};
use serde_json::{json, Value};

const WAIT: Duration = Duration::from_secs(60);

struct Quick;

impl Provider for Quick {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Ollama
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![model()])
    }

    fn stream(&self, _req: &ChatRequest, _cancel: &CancelToken, on: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        let mut b = CompletionBuilder::new();
        for e in [StreamEvent::TextDelta("Changed the parser.\nConfidence: high".into()), StreamEvent::Stop(StopReason::EndTurn)] {
            b.push(&e);
            on(e);
        }
        Ok(b.finish())
    }
}

fn model() -> ModelInfo {
    let mut m = ModelInfo::bare(ProviderKind::Ollama, "scripted");
    m.supports_tools = true;
    m.context_window = Some(32_768);
    m
}

/// Answers `gh pr_create`: a plan for the dry run, a URL for the real call; records both.
#[derive(Default)]
struct RecordingGh {
    calls: Mutex<Vec<(Value, Option<PathBuf>)>>,
}

const PLAN: &str = "Pull request to open (one approval covers the push and the PR):\n  remote: origin (https://github.com/o/r.git)\n  branch: BRANCH @ SHORT (1 commits, 1 files, +1 −0)\n  commit: SHA\n  into:   main (origin/main)\n  title:  Fix the parser\n  body:\n    Changed the parser.\n";

impl ToolHost for RecordingGh {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    fn call(&self, call: &ToolCallReq, ctx: &ToolCtx) -> ToolOutcome {
        assert_eq!(call.name, "gh");
        self.calls.lock().unwrap().push((call.args.clone(), ctx.workspace.clone()));
        // Like the real tool: the plan names the branch's commit, and a call pinned to another
        // commit (the branch moved after the yes) is refused before anything is pushed.
        let branch = call.args["head"].as_str().unwrap_or("?");
        let sha = git(ctx.workspace.as_deref().unwrap(), &["rev-parse", &format!("refs/heads/{branch}")]);
        if call.args["dry_run"] == json!(true) {
            ToolOutcome::ok(PLAN.replace("BRANCH", branch).replace("SHORT", &sha[..7]).replace("SHA", &sha), "plan")
        } else if call.args.get("expect_sha").and_then(Value::as_str).is_some_and(|e| e != sha) {
            ToolOutcome::error("the branch moved after the plan was shown; nothing was pushed")
        } else {
            ToolOutcome::ok("Opened pull request https://github.com/o/r/pull/21\npushed", "PR #21")
        }
    }

    fn consequence(&self, _call: &ToolCallReq) -> Consequence {
        Consequence::Free
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").args(args).current_dir(dir).output().expect("git runs");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn temp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("marlowe-openpr-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const CODER: &str = "---
name: coder
description: Changes code in a worktree for the daemon tests.
effort: off
tools: []
budget: {tokens: 100000, steps: 20, usd: 1.00}
sandbox: worktree
---
You are the crew's coder. Do what the goal says, then report. End with `Confidence: high`.
";

struct Client {
    w: TcpStream,
    r: LineReader<BufReader<TcpStream>>,
}

impl Client {
    fn connect(info: &marlowe_proto::DaemonInfo) -> Client {
        let w = TcpStream::connect(info.addr()).unwrap();
        w.set_read_timeout(Some(WAIT)).unwrap();
        let mut c = Client { r: LineReader::new(BufReader::new(w.try_clone().unwrap())), w };
        c.send(&Request::Hello { token: info.token.clone(), client: "test".into(), since_seq: None });
        assert!(matches!(c.next().event, Event::Welcome { .. }));
        c
    }

    fn send(&mut self, r: &Request) {
        write_line(&mut self.w, r).unwrap();
    }

    fn next(&mut self) -> Envelope {
        self.r.next::<Envelope>().expect("read an envelope").expect("stream open")
    }

    fn until_event(&mut self, pred: impl Fn(&Event) -> bool) -> Vec<Envelope> {
        let deadline = Instant::now() + WAIT;
        let mut out = Vec::new();
        while Instant::now() < deadline {
            let env = self.next();
            let hit = pred(&env.event);
            out.push(env);
            if hit {
                return out;
            }
        }
        panic!("timed out; saw {out:#?}");
    }
}

#[test]
fn parity_g3_open_pr_asks_once_then_pushes_and_opens() {
    let home = temp("home");
    let repo = temp("repo");
    git(&repo, &["init", "-q", "-b", "main"]);
    for (k, v) in [("user.name", "T"), ("user.email", "t@example.com"), ("commit.gpgsign", "false")] {
        git(&repo, &["config", k, v]);
    }
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    git(&repo, &["add", "a.txt"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    std::fs::create_dir_all(home.join("roles")).unwrap();
    std::fs::write(home.join("roles").join("coder.md"), CODER).unwrap();
    Library::open(&paths::cowork_db(&home)).unwrap().add(&model()).unwrap();

    let gh = Arc::new(RecordingGh::default());
    let mut providers = ProviderFactory::new(Arc::new(MemoryStore::new()));
    providers.register(ProviderKind::Ollama, |_| Ok(Arc::new(Quick) as Arc<dyn Provider>));
    let gh_for_factory = gh.clone();
    let deps = DaemonDeps {
        providers,
        tools: Arc::new(move |_| gh_for_factory.clone() as Arc<dyn ToolHost>),
        user: UserInfo { name: Some("Tester".into()), address: None },
        mind: None,
        sentinel: None,
        voice: None,
        crew: Some(CrewSetup { tools: Arc::new(|_, _| Arc::new(RecordingGh::default()) as Arc<dyn ToolHost>), config: CrewConfig::default(), research: None }),
    };
    let d = Daemon::start(deps, &home, DaemonConfig::default()).expect("daemon starts");
    let info = d.info();
    let join = std::thread::spawn(move || d.wait());
    let mut c = Client::connect(&info);

    // A run in a worktree of `repo`; the "agent" commits there.
    c.send(&Request::Dispatch {
        session: None,
        role: "coder".into(),
        goal: "Fix the parser".into(),
        playbook: None,
        workspace: Some(repo.to_string_lossy().into_owned()),
        budget: None,
    });
    let got = c.until_event(|e| matches!(e, Event::RunUpdated(_)));
    let run: RunId = got.iter().find_map(|e| if let Event::RunUpdated(i) = &e.event { Some(i.id.clone()) } else { None }).unwrap();
    c.until_event(|e| matches!(e, Event::RunFinished { run: r, .. } if *r == run));
    let branch = format!("crew/{}", run.as_str());
    assert!(git(&repo, &["branch", "--list", &branch]).contains(&branch), "the run has its worktree branch");
    c.send(&Request::ListRuns { limit: 10 });
    let got = c.until_event(|e| matches!(e, Event::Runs { .. }));
    let Event::Runs { list } = &got.last().unwrap().event else { unreachable!() };
    let sandbox = list.iter().find(|r| r.id == run).and_then(|r| r.sandbox.clone()).expect("the run has a sandbox");
    assert_eq!(sandbox.branch.as_deref(), Some(branch.as_str()));
    let worktree = PathBuf::from(&sandbox.root);
    std::fs::write(worktree.join("parser.rs"), "fn parse() {}\n").unwrap();
    git(&worktree, &["add", "parser.rs"]);
    git(&worktree, &["commit", "-q", "-m", "Fix the parser"]);

    // Yes: exactly one approval, then the real call.
    c.send(&Request::OpenPr { run: RunId(run.short().to_string()), title: None, body: None });
    let got = c.until_event(|e| matches!(e, Event::ApprovalRequested { .. }));
    let Event::ApprovalRequested { approval, run: asked_run, tool, summary, detail, .. } = got.last().unwrap().event.clone() else { unreachable!() };
    assert_eq!(asked_run.as_ref(), Some(&run));
    assert_eq!(tool, "gh pr_create");
    assert!(summary.contains(&format!("push {branch} to origin")) && summary.contains("into main"), "{summary}");
    assert!(detail.contains("title:  Fix the parser") && detail.contains("remote: origin"), "{detail}");
    assert_eq!(gh.calls.lock().unwrap().len(), 1, "only the dry run so far: nothing is pushed before the yes");
    c.send(&Request::Approve { approval, decision: ApprovalDecision::Allow, reason: None });
    let got = c.until_event(|e| matches!(e, Event::PrOpened { .. } | Event::Error { .. }));
    match &got.last().unwrap().event {
        Event::PrOpened { run: r, url } => {
            assert_eq!(r, &run);
            assert_eq!(url, "https://github.com/o/r/pull/21");
        }
        other => panic!("{other:?}"),
    }
    {
        let calls = gh.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0["dry_run"], json!(true));
        assert_eq!(calls[1].0.get("dry_run"), None);
        assert_eq!(calls[1].0["head"], json!(branch));
        assert_eq!(calls[1].0["expect_sha"], json!(git(&worktree, &["rev-parse", "HEAD"])), "pinned to the commit shown");
        assert_eq!(calls[1].0["op"], "pr_create");
        let norm = |p: &Path| p.to_string_lossy().replace('\\', "/").to_lowercase();
        assert_eq!(calls[1].1.as_deref().map(norm), Some(norm(&repo)), "run in the original checkout");
    }

    // No: nothing more happens.
    c.send(&Request::OpenPr { run: run.clone(), title: Some("Other title".into()), body: None });
    let got = c.until_event(|e| matches!(e, Event::ApprovalRequested { .. }));
    let Event::ApprovalRequested { approval, .. } = got.last().unwrap().event.clone() else { unreachable!() };
    c.send(&Request::Approve { approval, decision: ApprovalDecision::Deny, reason: None });
    let got = c.until_event(|e| matches!(e, Event::PrOpened { .. } | Event::Error { .. }));
    match &got.last().unwrap().event {
        Event::Error { message, .. } => assert!(message.contains("was not approved; nothing was pushed"), "{message}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(gh.calls.lock().unwrap().len(), 3, "the second request only made a dry run");

    // The branch moves between the ask and the answer: the yes was for another commit.
    c.send(&Request::OpenPr { run: run.clone(), title: None, body: None });
    let got = c.until_event(|e| matches!(e, Event::ApprovalRequested { .. }));
    let Event::ApprovalRequested { approval, .. } = got.last().unwrap().event.clone() else { unreachable!() };
    std::fs::write(worktree.join("late.rs"), "fn late() {}\n").unwrap();
    git(&worktree, &["add", "late.rs"]);
    git(&worktree, &["commit", "-q", "-m", "late"]);
    c.send(&Request::Approve { approval, decision: ApprovalDecision::Allow, reason: None });
    let got = c.until_event(|e| matches!(e, Event::PrOpened { .. } | Event::Error { .. }));
    match &got.last().unwrap().event {
        Event::Error { message, .. } => assert!(message.contains("moved") && message.contains("nothing was pushed"), "{message}"),
        other => panic!("an unapproved commit must not be proposed: {other:?}"),
    }

    // A run that does not exist.
    c.send(&Request::OpenPr { run: RunId("nope".into()), title: None, body: None });
    let got = c.until_event(|e| matches!(e, Event::PrOpened { .. } | Event::Error { .. }));
    assert!(matches!(&got.last().unwrap().event, Event::Error { message, .. } if message.contains("no run nope")));

    c.send(&Request::Shutdown);
    c.until_event(|e| matches!(e, Event::ShuttingDown));
    join.join().unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&repo);
}
