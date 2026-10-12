//! Track I's parity rows besides A9 (in `build_v2.rs`): A5 worktree isolation through the real
//! provider, A12 the run inbox, S5 per-call checkpoints for crew steps, G6 the security lens on
//! a pending diff. Temp homes, temp repos, scripted models.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use common::*;
use marlowe_agent::commands::CommandsMiddleware;
use marlowe_agent::git_prompts::SECURITY_CHECKLIST;
use marlowe_agent::instructions::Sources;
use marlowe_agent::{Agent, AutoApprove, Consequence, Conversation, SystemPrompt, ToolCallReq, ToolCtx, ToolHost, ToolOutcome};
use marlowe_crew::playbook::{review_diff, AgentRunner, Lens, ModelResolver, Severity, StepModel, RESUME_PROMPT};
use marlowe_crew::*;
use marlowe_model::{CancelToken, ModelInfo, ProviderKind, Role, ToolSpec};
use marlowe_os::workspace::{WorktreeError, WorktreeProvider, WorktreeRequest};
use marlowe_proto::{SandboxKind, SessionId};
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
    let (key, name) = h.id.split_once('-').unwrap();
    assert_eq!(name, "agent-7-x", "a name is made path- and branch-safe");
    assert!(key.len() == 8 && key.chars().all(|c| c.is_ascii_hexdigit()), "the id leads with its repository's key: {}", h.id);
    assert_eq!(h.branch, "marlowe/agent-7-x");
    assert_eq!(h.base, head);
    assert!(h.root.starts_with(home.join("worktrees").join("agents")) && h.root.join("README.md").is_file());
    assert_eq!(git(&h.root, &["rev-parse", "--abbrev-ref", "HEAD"]), "marlowe/agent-7-x");
    // The same name again, and a fresh provider after a restart, find it rather than re-create it.
    assert_eq!(wt.create(&req("Agent 7/../x", None)).unwrap(), h);
    assert_eq!(GitWorktrees::new(&home).attach(&h.id).unwrap(), h);
    assert!(matches!(wt.attach("../agent-7-x"), Err(WorktreeError::NotFound(_))), "an id is checked, never joined as a path");

    // The same name in another repository is another worktree, of that repository.
    let other_base = temp_dir("a5-other");
    let other = repo(&other_base, &[("OTHER.md", "other\n")]);
    let o = wt.create(&WorktreeRequest { repo: other.clone(), name: "Agent 7/../x".into(), base: None, parent: None }).unwrap();
    assert_ne!(o.id, h.id);
    assert_ne!(o.root, h.root);
    assert!(o.root.join("OTHER.md").is_file() && !o.root.join("README.md").exists(), "not the first repository's worktree");
    assert_eq!(git(&o.root, &["rev-parse", "--path-format=absolute", "--git-common-dir"]), git(&other, &["rev-parse", "--path-format=absolute", "--git-common-dir"]));
    wt.remove(&o, false).unwrap();
    // Two long names with a common start stay apart.
    let long = "a-very-long-worktree-name-that-goes-on-and-on-";
    let l1 = wt.create(&req(&format!("{long}one"), None)).unwrap();
    let l2 = wt.create(&req(&format!("{long}two"), None)).unwrap();
    assert_ne!(l1.id, l2.id);
    assert_ne!(l1.branch, l2.branch);
    wt.remove(&l1, false).unwrap();
    wt.remove(&l2, false).unwrap();

    // A child of a worktree starts from the parent's HEAD.
    std::fs::write(h.root.join("a.txt"), "a\n").unwrap();
    git(&h.root, &["add", "-A"]);
    git(&h.root, &["-c", "user.name=T", "-c", "user.email=t@example.invalid", "commit", "-q", "-m", "a"]);
    let child = wt.create(&req("child", Some(&h.id))).unwrap();
    assert_eq!(child.base, git(&h.root, &["rev-parse", "HEAD"]));
    assert!(child.root.join("a.txt").is_file());

    // exit_worktree keeps the branch; a discard deletes it.
    wt.remove(&h, true).unwrap();
    assert!(!h.root.exists());
    assert_eq!(git(&work, &["branch", "--list", "marlowe/agent-7-x"]).trim_start_matches(['*', ' ']), "marlowe/agent-7-x");
    assert!(matches!(wt.attach(&h.id), Err(WorktreeError::NotFound(_))));
    wt.remove(&child, false).unwrap();
    assert!(git(&work, &["branch", "--list", "marlowe/child"]).is_empty());
    assert!(matches!(
        wt.create(&WorktreeRequest { repo: base.join("not-a-repo"), name: "x".into(), base: None, parent: None }),
        Err(WorktreeError::Unavailable(_))
    ));
    remove(&[&base, &other_base]);
}

/// A build's child worktrees each build into their own target dir, and a discard of a run that
/// stopped mid-wave (so `deliver` never cleaned up) takes every child worktree, its build output
/// and its `crew/<run>-<task>` branch with it.
#[test]
fn a_discarded_build_takes_its_child_worktrees_and_branches() {
    let _watchdog = watchdog("a_discarded_build_takes_its_child_worktrees_and_branches", 90);
    let base = temp_dir("kids");
    let work = repo(&base, &[("README.md", "hello\n")]);
    let home = base.join("home");
    let run = RunId::new_random();
    let sb = Sandbox::create(&home, &run, Some(work.as_path()), SandboxKind::Worktree).unwrap();
    let a = sb.create_child(&home, &run, "t1", &[]).unwrap();
    let b = sb.create_child(&home, &run, "t2", &[]).unwrap();
    let dirs: Vec<_> = [&sb, &a, &b].iter().map(|s| s.target_dir.clone().unwrap()).collect();
    assert!(dirs[0] != dirs[1] && dirs[1] != dirs[2] && dirs[0] != dirs[2], "one target dir per worktree: {dirs:?}");
    assert_eq!(dirs[1].parent(), dirs[0].parent(), "under the repository's");
    for d in &dirs {
        std::fs::create_dir_all(d.join("debug")).unwrap();
    }
    assert_eq!(git(&work, &["branch", "--list", "crew/*"]).lines().count(), 3);

    sb.discard().unwrap();
    assert!(git(&work, &["branch", "--list", "crew/*"]).is_empty(), "{}", git(&work, &["branch", "--list"]));
    assert_eq!(git(&work, &["worktree", "list", "--porcelain"]).matches("worktree ").count(), 1, "only the checkout is left");
    assert!(!a.root.exists() && !b.root.exists() && !sb.root.exists());
    for d in &dirs {
        assert!(!d.exists(), "{} was left", d.display());
    }
    remove(&[&base]);
}

#[test]
fn parity_a12_an_inbox_message_reaches_the_next_step_at_agent_trust() {
    let _watchdog = watchdog("parity_a12_an_inbox_message_reaches_the_next_step_at_agent_trust", 90);
    let home = home("a12");
    let sent = Arc::new(AtomicBool::new(false));
    let s2 = sent.clone();
    let provider = Scripted::new(move |_, n, _| {
        match n {
            0 => {
                // Hold the first call until the message is in the inbox.
                wait("the message", 30, || s2.load(Ordering::SeqCst));
                Ok(calls(&[("note", json!({"text": "looking"}))]))
            }
            // In the same step, after reading the message: a shell that runs free in a clean run.
            1 => Ok(calls(&[("bash", json!({"command": "echo checked"}))])),
            _ => Ok(answer("Got the other agent's note.")),
        }
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
    // ...and the step that read it at once: its next shell asks, as after any untrusted read.
    let asked = fx.approvals.asked.lock().unwrap().clone();
    assert!(asked.iter().any(|a| a.summary.contains("after untrusted content")), "{asked:#?}");
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

#[test]
fn parity_s5_a_resumed_step_keeps_the_taint_of_what_it_read_before_the_stop() {
    let _watchdog = watchdog("parity_s5_a_resumed_step_keeps_the_taint_of_what_it_read_before_the_stop", 90);
    let home = home("s5t");
    let first = Scripted::new(|req, _, cancel| {
        if after_tool(req) {
            block(cancel)
        } else {
            Ok(calls(&[("fetch", json!({"url": "https://example.invalid/page"}))]))
        }
    });
    let fx = start(&home, &first, CrewConfig::default(), true);
    let id = fx.sup.dispatch(None, args("tester", "Read the page, then check it", None, None)).unwrap().run_id;
    wait("the call after the read", 30, || first.requests().len() == 2);
    fx.sup.shutdown();
    assert!(!info(&fx.sup, &id).tainted, "the step had not finished, so the run flag is not set yet");
    drop(fx);

    // Resumed: the untrusted page is in the history it continues from, so a shell asks.
    let second = Scripted::new(|req, _, _| {
        Ok(if after_tool(req) { answer("Checked.") } else { calls(&[("bash", json!({"command": "echo checked"}))]) })
    });
    let fx = start(&home, &second, CrewConfig::default(), true);
    let run = wait_status(&fx.sup, &id, CrewRunStatus::Done, 30);
    let asked = fx.approvals.asked.lock().unwrap().clone();
    assert!(asked.iter().any(|a| a.summary.contains("after untrusted content")), "{asked:#?}");
    assert!(run.tainted, "the resumed step reports what it read");
    finish(fx, &[&home]);
}

/// A `git` tool whose `diff` op returns a fixed diff.
struct GitDiff(String);

impl ToolHost for GitDiff {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec { name: "git".into(), description: "git".into(), input_schema: json!({"type": "object"}) }]
    }

    fn call(&self, call: &ToolCallReq, _ctx: &ToolCtx) -> ToolOutcome {
        match call.args["op"].as_str() {
            Some("diff") => ToolOutcome::ok(self.0.clone(), "diff"),
            _ => ToolOutcome::ok("", "nothing"),
        }
    }

    fn consequence(&self, _call: &ToolCallReq) -> Consequence {
        Consequence::Free
    }
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
        assert!(seen[0].0.contains(SECURITY_CHECKLIST), "the crew's lens is the one list");
    }

    // The user's `/security-review`: the conductor's command table expands it to the same lens,
    // the model reads the pending diff through git and names the planted eval.
    let first_user = Arc::new(Mutex::new(String::new()));
    let fu = first_user.clone();
    let conductor = Scripted::new(move |req, _, _| {
        if !after_tool(req) {
            *fu.lock().unwrap() = req.messages.iter().rev().find(|m| m.role == Role::User).map(|m| m.joined_text()).unwrap_or_default();
            return Ok(calls(&[("git", json!({"op": "diff"}))]));
        }
        Ok(match diff_line(&tool_results(req), "eval(") {
            Some((file, line)) => answer(&format!("blocker {file}:{line}: request text reaches eval")),
            None => answer("No findings."),
        })
    });
    let mut model = ModelInfo::bare(ProviderKind::OpenRouter, "m");
    model.supports_tools = true;
    let mut agent = Agent::new(conductor.clone(), model, Arc::new(GitDiff(diff.clone())), SystemPrompt::with_core("You are Marlowe."));
    agent.approvals = Arc::new(AutoApprove);
    let no_shell = Arc::new(|_: &str, _: Option<&std::path::Path>| -> Result<String, String> { Err("no shell here".into()) });
    let sources = Sources { roots: vec![work.clone()], cwd: Some(work.clone()), stop_at: Some(base.clone()), ..Default::default() };
    agent.middleware.push(CommandsMiddleware::new(sources, no_shell));
    let mut convo = Conversation::new(SessionId("g6".into()), Some(work.clone()));
    let (_tx, rx) = crossbeam_channel::unbounded();
    let _ = agent.run_turn(&mut convo, "/security-review", &CancelToken::new(), &rx, &mut |_| {});
    let expanded = first_user.lock().unwrap().clone();
    assert!(expanded.starts_with("<command-name>/security-review</command-name>"), "{expanded}");
    assert!(expanded.contains(SECURITY_CHECKLIST) && expanded.contains("git{op:\"diff\"}"), "{expanded}");
    let reply = convo.messages.last().map(|m| m.joined_text()).unwrap_or_default();
    assert_eq!(reply, "blocker app/handler.py:4: request text reaches eval");
    // A clean diff gets no findings.
    std::fs::remove_file(sandbox.root.join("app").join("handler.py")).unwrap();
    std::fs::write(sandbox.root.join("app").join("ok.py"), "def handle(request_text):\n    return len(request_text)\n").unwrap();
    let clean = review_diff(&runner, "Add a request handler.", &sandbox.diff_text().unwrap(), &[Lens::Security], "sec2").unwrap();
    assert!(clean[0].1.is_empty());
    let _ = sandbox.discard();
    remove(&[&base]);
}
