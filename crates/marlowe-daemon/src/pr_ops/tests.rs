use std::sync::Mutex;

use marlowe_agent::{Consequence, ToolOutcome};
use marlowe_model::ToolSpec;

use super::*;

/// A tool host that answers `gh` calls from fixed outcomes and records the arguments.
struct FakeGh {
    plan: ToolOutcome,
    done: ToolOutcome,
    calls: Mutex<Vec<Value>>,
}

impl FakeGh {
    fn new(plan: ToolOutcome, done: ToolOutcome) -> Self {
        Self { plan, done, calls: Mutex::new(Vec::new()) }
    }

    fn calls(&self) -> Vec<Value> {
        self.calls.lock().unwrap().clone()
    }
}

impl ToolHost for FakeGh {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    fn call(&self, call: &ToolCallReq, ctx: &ToolCtx) -> ToolOutcome {
        assert_eq!(call.name, "gh");
        assert!(ctx.workspace.is_some(), "the host works in the original checkout");
        self.calls.lock().unwrap().push(call.args.clone());
        if call.args.get("dry_run").and_then(Value::as_bool) == Some(true) {
            self.plan.clone()
        } else {
            self.done.clone()
        }
    }

    fn consequence(&self, _call: &ToolCallReq) -> Consequence {
        Consequence::Free
    }
}

const PLAN: &str = "Pull request to open (one approval covers the push and the PR):\n  remote: origin (https://github.com/o/r.git)\n  branch: crew/abcd1234 @ 1a2b3c4 (2 commits, 3 files, +10 −1)\n  commit: 1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d\n  into:   main (origin/main)\n  title:  Fix the parser\n  body:\n    Parser fix.\n";

fn info(status: &str, note: Option<&str>, worktree: bool) -> CrewRunInfo {
    let sandbox = if worktree {
        json!({"kind": "worktree", "root": "C:\\p\\wt", "origin": "C:\\work\\proj", "branch": "crew/abcd1234"})
    } else {
        json!({"kind": "scratch", "root": "C:\\p\\scratch"})
    };
    serde_json::from_value(json!({
        "id": "abcd1234-0000", "session": "s-1", "role": "engineer", "title": "Fix the parser", "goal": "fix it",
        "status": status, "sandbox": sandbox, "budget": {}, "spent": {}, "step": 3, "note": note,
        "created_ms": 1, "updated_ms": 2,
        "result": {"summary": "Fixed the parser.", "artifacts": [], "citations": []}
    }))
    .expect("run info")
}

fn pr_call_args(host: &FakeGh) -> Vec<Value> {
    host.calls()
}

#[test]
fn parity_g3_a_crew_pr_is_one_approval_then_the_push_and_the_pr() {
    let host = FakeGh::new(ToolOutcome::ok(PLAN, "plan"), ToolOutcome::ok("Opened pull request https://github.com/o/r/pull/9\npushed", "PR #9"));
    let asked = Mutex::new(Vec::new());
    let ask = |summary: String, detail: String| {
        asked.lock().unwrap().push((summary, detail));
        ApprovalDecision::Allow
    };
    let url = open_pr(&host, &SessionId("s-1".into()), &info("needs_you", None, true), None, None, &ask).unwrap();
    assert_eq!(url, "https://github.com/o/r/pull/9");
    let asked = asked.lock().unwrap();
    assert_eq!(asked.len(), 1, "exactly one approval");
    assert_eq!(asked[0].0, "open a pull request for run abcd1234: push crew/abcd1234 to origin (https://github.com/o/r.git) and propose it into main");
    assert!(asked[0].1.contains("title:  Fix the parser") && asked[0].1.contains("Parser fix."), "the modal carries the whole plan: {}", asked[0].1);
    let calls = pr_call_args(&host);
    assert_eq!(calls.len(), 2, "a dry run for the plan, then the real call");
    assert_eq!(calls[0]["dry_run"], true);
    assert!(calls[1].get("dry_run").is_none());
    assert!(calls[0].get("expect_sha").is_none());
    assert_eq!(calls[1]["expect_sha"], "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d", "the push is pinned to the commit the user was shown");
    for c in &calls {
        assert_eq!(c["op"], "pr_create");
        assert_eq!(c["branch"], "crew/abcd1234");
        assert_eq!(c["cwd"], "C:\\work\\proj");
        assert_eq!(c["title"], "Fix the parser");
        assert!(c["body"].as_str().unwrap().starts_with("Fixed the parser.") && c["body"].as_str().unwrap().contains("crew run abcd1234"), "{c}");
    }
}

#[test]
fn a_plan_that_does_not_name_its_commit_is_not_pushed() {
    let plan = PLAN.replace("  commit: 1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d\n", "");
    let host = FakeGh::new(ToolOutcome::ok(plan, "plan"), ToolOutcome::ok("Opened pull request https://github.com/o/r/pull/9", "PR #9"));
    let never = |_: String, _: String| -> ApprovalDecision { panic!("must not ask") };
    let e = open_pr(&host, &SessionId("s".into()), &info("done", None, true), None, None, &never).unwrap_err();
    assert!(e.contains("did not name the commit"), "{e}");
    assert_eq!(host.calls().len(), 1, "only the dry run happened");
}

#[test]
fn a_denied_pr_pushes_and_posts_nothing() {
    let host = FakeGh::new(ToolOutcome::ok(PLAN, "plan"), ToolOutcome::ok("Opened pull request https://github.com/o/r/pull/9", "PR #9"));
    let e = open_pr(&host, &SessionId("s".into()), &info("done", None, true), Some("Own title".into()), Some("Own body".into()), &|_, _| ApprovalDecision::Deny).unwrap_err();
    assert!(e.contains("was not approved; nothing was pushed"), "{e}");
    let calls = host.calls();
    assert_eq!(calls.len(), 1, "only the dry run happened");
    assert_eq!((calls[0]["title"].as_str(), calls[0]["body"].as_str()), (Some("Own title"), Some("Own body")), "the caller's words win");
}

#[test]
fn a_merged_run_proposes_the_commit_it_ended_at() {
    let host = FakeGh::new(ToolOutcome::ok(PLAN, "plan"), ToolOutcome::ok("https://github.com/o/r/pull/4", "PR #4"));
    open_pr(&host, &SessionId("s".into()), &info("done", Some("merged: fast-forward to 1a2b3c4"), true), None, None, &|_, _| ApprovalDecision::Allow).unwrap();
    assert_eq!(host.calls()[0]["sha"], "1a2b3c4");
    assert_eq!(merged_commit(Some("merged: merge commit deadbeef0")).as_deref(), Some("deadbeef0"));
    assert_eq!(merged_commit(Some("report saved to C:\\x")), None);
    assert_eq!(merged_commit(None), None);
}

#[test]
fn runs_that_cannot_have_a_pr_say_why_before_anything_is_asked() {
    let host = FakeGh::new(ToolOutcome::ok(PLAN, "plan"), ToolOutcome::ok("x", "x"));
    let never = |_: String, _: String| -> ApprovalDecision { panic!("must not ask") };
    for (i, want) in [
        (info("running", None, true), "is running; pause or cancel it first"),
        (info("done", None, false), "has no git worktree"),
    ] {
        let e = open_pr(&host, &SessionId("s".into()), &i, None, None, &never).unwrap_err();
        assert!(e.contains(want), "{e}");
    }
    // The tool's own refusal (nothing to propose, gh missing) comes back as it is.
    let host = FakeGh::new(ToolOutcome::error("`crew/abcd1234` has no commits beyond `origin/main`, so there is nothing to propose"), ToolOutcome::ok("x", "x"));
    let e = open_pr(&host, &SessionId("s".into()), &info("done", None, true), None, None, &never).unwrap_err();
    assert!(e.contains("nothing to propose"), "{e}");
    // A failure after the yes is reported, and a success without a URL is not called success.
    let host = FakeGh::new(ToolOutcome::ok(PLAN, "plan"), ToolOutcome::error("gh pr create failed (exit 4)"));
    assert!(open_pr(&host, &SessionId("s".into()), &info("done", None, true), None, None, &|_, _| ApprovalDecision::Allow).unwrap_err().contains("exit 4"));
    let host = FakeGh::new(ToolOutcome::ok(PLAN, "plan"), ToolOutcome::ok("done, no link", "x"));
    assert!(open_pr(&host, &SessionId("s".into()), &info("done", None, true), None, None, &|_, _| ApprovalDecision::Allow).unwrap_err().contains("gave no URL"));
}
