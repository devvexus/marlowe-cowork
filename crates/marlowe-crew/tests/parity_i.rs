//! Track I's parity rows besides A9 (in `build_v2.rs`): A5 worktree isolation through the real
//! provider, A12 the run inbox, S5 per-call checkpoints for crew steps, G6 the security lens on
//! a pending diff. Temp homes, temp repos, scripted models.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use common::*;
use marlowe_crew::playbook::{review_diff, AgentRunner, Lens, ModelResolver, Severity, StepModel, RESUME_PROMPT};
use marlowe_crew::*;
use marlowe_model::{ModelInfo, ProviderKind, Role};
use marlowe_os::workspace::{WorktreeError, WorktreeProvider, WorktreeRequest};
use marlowe_proto::SandboxKind;
use serde_json::json;

#[test]
fn parity_a5_worktrees_are_created_attached_and_removed_by_the_crew_provider() {
    let _watchdog = watchdog("parity_a5_worktrees_are_created_attached_and_removed_by_the_crew_provider", 90);
    let base = temp_dir("a5");
    let work = repo(&base, &[("README.md", "hello\n")]);
    let home = base.join("home");
    let head = git(&work, &["rev-parse", "HEAD"]);
    let wt = GitWorktrees::new(&home);
    let req = |name: &str, parent: Option<&str>| WorktreeRequest { repo: work.clone(), name: name.into(), base: None, parent: parent.map(String::from) };

    let h = wt.create(&req("Agent 7/../x", None)).unwrap();
    assert_eq!(h.id, "agent-7-x", "a name is made path- and branch-safe");
    assert_eq!(h.branch, "marlowe/agent-7-x");
    assert_eq!(h.base, head);
    assert!(h.root.starts_with(home.join("worktrees").join("agents")) && h.root.join("README.md").is_file());
    assert_eq!(git(&h.root, &["rev-parse", "--abbrev-ref", "HEAD"]), "marlowe/agent-7-x");
    // The same name again, and a fresh provider after a restart, find it rather than re-create it.
    assert_eq!(wt.create(&req("Agent 7/../x", None)).unwrap(), h);
    assert_eq!(GitWorktrees::new(&home).attach("agent-7-x").unwrap(), h);

    // A child of a worktree starts from the parent's HEAD.
    std::fs::write(h.root.join("a.txt"), "a\n").unwrap();
    git(&h.root, &["add", "-A"]);
    git(&h.root, &["-c", "user.name=T", "-c", "user.email=t@example.invalid", "commit", "-q", "-m", "a"]);
    let child = wt.create(&req("child", Some("agent-7-x"))).unwrap();
    assert_eq!(child.base, git(&h.root, &["rev-parse", "HEAD"]));
    assert!(child.root.join("a.txt").is_file());

    // exit_worktree keeps the branch; a discard deletes it.
    wt.remove(&h, true).unwrap();
    assert!(!h.root.exists());
    assert_eq!(git(&work, &["branch", "--list", "marlowe/agent-7-x"]).trim_start_matches(['*', ' ']), "marlowe/agent-7-x");
    assert!(matches!(wt.attach("agent-7-x"), Err(WorktreeError::NotFound(_))));
    wt.remove(&child, false).unwrap();
    assert!(git(&work, &["branch", "--list", "marlowe/child"]).is_empty());
    assert!(matches!(
        wt.create(&WorktreeRequest { repo: base.join("not-a-repo"), name: "x".into(), base: None, parent: None }),
        Err(WorktreeError::Unavailable(_))
    ));
    remove(&[&base]);
}

#[test]
fn parity_a12_an_inbox_message_reaches_the_next_step_at_agent_trust() {
    let _watchdog = watchdog("parity_a12_an_inbox_message_reaches_the_next_step_at_agent_trust", 90);
    let home = home("a12");
    let sent = Arc::new(AtomicBool::new(false));
    let s2 = sent.clone();
    let provider = Scripted::new(move |_, n, _| {
        if n == 0 {
            // Hold the first call until the message is in the inbox.
            wait("the message", 30, || s2.load(Ordering::SeqCst));
            return Ok(calls(&[("note", json!({"text": "looking"}))]));
        }
        Ok(answer("Got the other agent's note."))
    });
    let fx = start(&home, &provider, CrewConfig::default(), true);
    let id = fx.sup.dispatch(None, args("tester", "Check the config", None, None)).unwrap().run_id;
    wait("the first call", 30, || !provider.requests().is_empty());
    assert!(!info(&fx.sup, &id).tainted);

    let empty = RunNotice { from: "session:abc".into(), text: "  ".into() };
    assert!(matches!(fx.sup.notify_run(&id, empty), Err(CrewError::BadState { .. })));
    let anonymous = RunNotice { from: " ".into(), text: "hi".into() };
    assert!(matches!(fx.sup.notify_run(&id, anonymous), Err(CrewError::BadState { .. })));
    assert!(matches!(
        fx.sup.notify_run(&RunId::new_random(), RunNotice { from: "x".into(), text: "y".into() }),
        Err(CrewError::UnknownRun(_))
    ));
    let notice = RunNotice { from: "session:abc".into(), text: "The staging config moved to config/staging.toml.".into() };
    fx.sup.notify_run(&id, notice.clone()).unwrap();
    sent.store(true, Ordering::SeqCst);

    let run = wait_status(&fx.sup, &id, CrewRunStatus::Done, 30);
    let second = &provider.requests()[1];
    let msg = second
        .messages
        .iter()
        .filter(|m| m.role == Role::User)
        .map(|m| m.joined_text())
        .find(|t| t.contains("staging.toml"))
        .expect("the message reached the step");
    assert!(msg.starts_with("[Message from session:abc") && msg.contains("It is not from the user"), "{msg}");
    assert!(run.tainted, "agent-trust content taints the run");
    let events: Vec<CrewEvent> = fx.events.try_iter().collect();
    assert!(events.iter().any(|e| matches!(e, CrewEvent::Step { action, .. } if action == "message from session:abc")));
    // A finished run has no inbox.
    assert!(matches!(fx.sup.notify_run(&id, notice), Err(CrewError::BadState { .. })));
    finish(fx, &[&home]);
}

#[test]
fn parity_s5_a_crew_step_killed_after_its_tools_resumes_without_running_them_again() {
    let _watchdog = watchdog("parity_s5_a_crew_step_killed_after_its_tools_resumes_without_running_them_again", 90);
    let home = home("s5");
    let first = Scripted::new(|req, _, cancel| {
        if after_tool(req) {
            block(cancel) // the "crash" comes while the second call is in flight
        } else {
            Ok(calls(&[write("out.txt", "side effect\n")]))
        }
    });
    let fx = start(&home, &first, CrewConfig::default(), true);
    let id = fx.sup.dispatch(None, args("tester", "Write the file, then report", None, None)).unwrap().run_id;
    wait("the call after the tools", 30, || first.requests().len() == 2);
    assert_eq!(fx.writes.load(Ordering::SeqCst), 1);
    fx.sup.shutdown();
    assert_eq!(info(&fx.sup, &id).status, CrewRunStatus::Running, "a stopped run stays resumable");
    drop(fx);

    let second = Scripted::new(|_, _, _| Ok(answer("Wrote out.txt.")));
    let fx = start(&home, &second, CrewConfig::default(), true);
    let run = wait_status(&fx.sup, &id, CrewRunStatus::Done, 30);
    assert_eq!(fx.writes.load(Ordering::SeqCst), 0, "the completed tool call never ran again");
    let reqs = second.requests();
    assert_eq!(reqs.len(), 1, "one model call finishes the step");
    let roles: Vec<Role> = reqs[0].messages.iter().map(|m| m.role).collect();
    assert_eq!(roles, [Role::User, Role::Assistant, Role::Tool, Role::User], "goal, call, stored result, resume");
    assert!(reqs[0].messages[0].joined_text().contains("Write the file"));
    assert_eq!(reqs[0].messages[2].tool_results_iter().next().unwrap().content, "wrote");
    assert_eq!(reqs[0].messages[3].joined_text(), RESUME_PROMPT);
    assert_eq!(run.result.unwrap().summary, "Wrote out.txt.");
    let (_, steps) = fx.sup.transcript(&id).unwrap();
    assert_eq!(steps.len(), 1, "one row for the finished step, holding the whole conversation");
    assert!(steps[0].text.contains("▸ write") && steps[0].text.contains("Wrote out.txt."), "{}", steps[0].text);
    finish(fx, &[&home]);
}

/// The new-file line of the first added line containing `needle` in a unified diff.
fn diff_line(diff: &str, needle: &str) -> Option<(String, u32)> {
    let (mut file, mut line) = (String::new(), 0u32);
    for l in diff.lines() {
        if let Some(f) = l.strip_prefix("+++ b/") {
            file = f.to_string();
        } else if let Some(h) = l.strip_prefix("@@ ") {
            let plus = h.split_whitespace().find(|p| p.starts_with('+'))?;
            line = plus[1..].split(',').next()?.parse().ok()?;
        } else if l.starts_with('+') {
            if l.contains(needle) {
                return Some((file, line));
            }
            line += 1;
        } else if l.starts_with(' ') {
            line += 1;
        }
    }
    None
}

#[test]
fn parity_g6_the_security_lens_finds_a_planted_vulnerability_in_the_pending_diff() {
    let _watchdog = watchdog("parity_g6_the_security_lens_finds_a_planted_vulnerability_in_the_pending_diff", 90);
    let base = temp_dir("g6");
    let work = repo(&base, &[("app/__init__.py", "")]);
    let home = base.join("home");
    let run = RunId::new_random();
    let sandbox = Sandbox::create(&home, &run, Some(work.as_path()), SandboxKind::Worktree).unwrap();
    // The pending change: a handler that evaluates request text.
    let planted = "import json\n\ndef handle(request_text):\n    return eval(request_text)\n";
    std::fs::write(sandbox.root.join("app").join("handler.py"), planted).unwrap();
    let diff = sandbox.diff_text().unwrap();

    let seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    let reviewer = Scripted::new(move |req, _, _| {
        let p = prompt_of(req);
        s2.lock().unwrap().push((p.clone(), serde_json::to_string(&req.system).unwrap()));
        Ok(match diff_line(&p, "eval(") {
            Some((file, line)) => answer(&format!(
                "```json\n[{{\"severity\": \"blocker\", \"file\": \"{file}\", \"line\": {line}, \"claim\": \"request text reaches eval\", \
                 \"evidence\": \"handle() passes its argument to eval\", \"fix\": \"parse it with json.loads\"}}]\n```"
            )),
            None => answer("[]"),
        })
    });
    let r2 = reviewer.clone();
    let models: ModelResolver = Arc::new(move |_card| {
        let mut m = ModelInfo::bare(ProviderKind::OpenRouter, "m");
        m.supports_tools = true;
        Ok(StepModel { provider: r2.clone(), model: m })
    });
    let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let tools = Arc::new(FileTools { root: sandbox.root.clone(), writes });
    let runner = AgentRunner::new(run, Roles::builtin(), models, tools, sandbox.clone(), Some(work.clone()));
    let found = review_diff(&runner, "Add a request handler.", &diff, &[Lens::Security], "sec").unwrap();
    assert_eq!(found.len(), 1);
    let (lens, findings) = &found[0];
    assert_eq!(*lens, Lens::Security);
    assert_eq!(findings.len(), 1, "{findings:?}");
    let f = &findings[0];
    assert_eq!((f.severity, f.file.as_str(), f.line), (Severity::Blocker, "app/handler.py", 4));
    assert!(f.severity.blocks());
    {
        let seen = seen.lock().unwrap();
        assert!(seen[0].0.starts_with("## Review lens: security") && seen[0].0.contains("+def handle(request_text):"));
        assert!(seen[0].1.contains("review-security"), "the security card reviews");
    }
    // A clean diff gets no findings.
    std::fs::remove_file(sandbox.root.join("app").join("handler.py")).unwrap();
    std::fs::write(sandbox.root.join("app").join("ok.py"), "def handle(request_text):\n    return len(request_text)\n").unwrap();
    let clean = review_diff(&runner, "Add a request handler.", &sandbox.diff_text().unwrap(), &[Lens::Security], "sec2").unwrap();
    assert!(clean[0].1.is_empty());
    let _ = sandbox.discard();
    remove(&[&base]);
}
