//! build_v2 end to end (design 3.6, parity A9): a fixture repository with its own build and test
//! scripts, the real supervisor, real git worktrees, a scripted provider playing every role.
//!
//! The one run covers the track's acceptance list: a 3-task DAG (two parallel, one after); hub
//! files frozen after the contract (an engineer's write to one is refused); a lockfile conflict
//! taken from one side and regenerated mechanically; a merged build that fails, two integrator
//! rounds, then the serial fallback; a pre-existing failing test that does not block; a flaky
//! test caught by the baseline's retry; a task stuck on the same failure twice, replanned by the
//! lead; an engineer's NEED_HUB_CHANGE made by the lead and kept; a security blocker found in
//! review and fixed; the diff waiting for merge approval.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use marlowe_crew::playbook::build_v2::LockRule;
use marlowe_crew::*;
use marlowe_model::{CancelToken, ChatRequest, ProviderError, StreamEvent};

const RUN_TESTS: &str = r#"import sys, os, glob, importlib.util
files = sys.argv[1:] or sorted(glob.glob("tests/test_*.py"))
sys.path.insert(0, os.path.join(os.getcwd(), "mods"))
failed = 0
for f in files:
    name = f.replace(os.sep, "/") + "::test"
    try:
        spec = importlib.util.spec_from_file_location("t_" + os.path.basename(f)[:-3], f)
        m = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(m)
        m.test()
        print("PASSED " + name)
    except Exception as e:
        failed += 1
        print("FAILED " + name + " - " + repr(e))
sys.exit(1 if failed else 0)
"#;

const BUILD: &str = r#"import os, sys, ast
err = 0
names = {}
for m in [l.strip() for l in open("registry.txt") if l.strip()]:
    p = os.path.join("mods", m + ".py")
    if not os.path.exists(p):
        print("error: registry lists " + m + " but " + p + " is missing")
        err = 1
        continue
    tree = ast.parse(open(p).read())
    for node in tree.body:
        if isinstance(node, ast.Assign) and any(getattr(t, "id", "") == "NAME" for t in node.targets):
            v = ast.literal_eval(node.value)
            if v in names:
                print("error: duplicate NAME " + repr(v) + " in " + names[v] + " and " + p)
                err = 1
            names[v] = p
sys.exit(err)
"#;

const LOCKGEN: &str = r#"lines = sorted(l.strip() for l in open("deps.txt") if l.strip())
open("deps.lock", "w").write("".join("locked " + l + "\n" for l in lines))
"#;

const FLAKY: &str = r#"import os
def test():
    p = ".flaky"
    n = int(open(p).read()) if os.path.exists(p) else 0
    open(p, "w").write(str(n + 1))
    assert n % 2 == 1, "flaky"
"#;

const PROFILE: &str = "[commands]\nbuild = \"python build.py\"\ntest = \"python run_tests.py\"\n\n[hubs]\nfiles = [\"registry.txt\", \"deps.txt\"]\n";

const PLAN: &str = r#"Here is the plan.
```json
{"tasks": [
  {"id": "t1", "goal": "alpha: value() returns 1", "files_owned": ["mods/alpha.py"], "depends_on": [], "accept_cmd": "python run_tests.py tests/test_alpha.py"},
  {"id": "t2", "goal": "beta: value() returns 2", "files_owned": ["mods/beta.py"], "depends_on": [], "accept_cmd": "python run_tests.py tests/test_beta.py"},
  {"id": "t3", "goal": "gamma: value() is alpha.value() + 2", "files_owned": ["mods/gamma.py"], "interfaces": "alpha.value()", "depends_on": ["t1"], "accept_cmd": "python run_tests.py tests/test_gamma.py"}
], "hubs": ["registry.txt", "deps.txt"]}
```"#;

const STUB: &str = "def value():\n    raise NotImplementedError\n";
const ALPHA_UNSAFE: &str = "NAME = \"shared\"\n\ndef value():\n    return 1\n\ndef run(text):\n    return eval(text)\n";
const ALPHA_SAFE: &str = "NAME = \"shared\"\n\ndef value():\n    return 1\n";

/// What the scripted roles saw, for the assertions.
#[derive(Default)]
struct Seen {
    frozen_refusal: bool,
    t2_took_hub: bool,
    t2_hub_undone: bool,
    t3_rounds: Vec<bool>,
    security_reviews: Vec<bool>,
}

fn script(seen: Arc<Mutex<Seen>>) -> impl Fn(&ChatRequest, usize, &CancelToken) -> Result<Vec<StreamEvent>, ProviderError> + Send + Sync {
    move |req, _, _| {
        let p = prompt_of(req);
        let done = after_tool(req);
        Ok(if p.starts_with("## Stage: plan") {
            answer(PLAN)
        } else if p.starts_with("## Stage: contract") {
            if done {
                answer("Contract ready: registry, deps, stubs and failing acceptance tests.")
            } else {
                calls(&[
                    write("registry.txt", "core\nalpha\nbeta\ngamma\n"),
                    write("deps.txt", "base\nleft\nright\n"),
                    write("mods/alpha.py", STUB),
                    write("mods/beta.py", STUB),
                    write("mods/gamma.py", STUB),
                    write("tests/test_alpha.py", "import alpha\ndef test():\n    assert alpha.value() == 1\n"),
                    write("tests/test_beta.py", "import beta\ndef test():\n    assert beta.value() == 2\n"),
                    write("tests/test_gamma.py", "import gamma\ndef test():\n    assert gamma.value() == 3\n"),
                ])
            }
        } else if p.starts_with("## Task t1") {
            if done {
                if tool_results(req).contains("frozen after the build's contract stage") {
                    seen.lock().unwrap().frozen_refusal = true;
                }
                answer("Implemented alpha.")
            } else {
                calls(&[write("registry.txt", "alpha\n"), write("mods/alpha.py", ALPHA_UNSAFE), write("deps.lock", "t1 lock\n")])
            }
        } else if p.starts_with("## Task t2") {
            if p.contains("You changed frozen hub files") {
                seen.lock().unwrap().t2_hub_undone = true;
            }
            if p.contains("The lead changed the hub files you asked for (deps.txt); they are in your worktree now") {
                // Round 2: the lead's change is in; nothing more to do.
                seen.lock().unwrap().t2_took_hub = true;
                answer("Implemented beta; it uses the extra dependency now.")
            } else if done {
                answer("Implemented beta.\nNEED_HUB_CHANGE: deps.txt: beta needs the extra dependency")
            } else {
                calls(&[write("mods/beta.py", "NAME = \"shared\"\n\ndef value():\n    return 2\n"), write("deps.lock", "t2 lock\n")])
            }
        } else if p.starts_with("## Task t3") {
            let revised = p.contains("The lead's revised instructions");
            if done {
                answer("Wrote gamma.")
            } else {
                seen.lock().unwrap().t3_rounds.push(revised);
                let body = if revised { "import alpha\n\ndef value():\n    return alpha.value() + 2\n" } else { "def value():\n    return 0\n" };
                calls(&[write("mods/gamma.py", body)])
            }
        } else if p.starts_with("## Hub change") {
            assert!(p.contains("- deps.txt: beta needs the extra dependency"), "{p}");
            if done {
                answer("Added extra to deps.txt.")
            } else {
                calls(&[write("deps.txt", "base\nleft\nright\nextra\n")])
            }
        } else if p.starts_with("## Replan task t3") {
            answer("gamma.value() must return alpha.value() + 2; import alpha from mods.")
        } else if p.starts_with("## Integrate") {
            answer("I could not reconcile the two modules without changing a task's intent.")
        } else if p.starts_with("## Serial re-implementation of task t2") {
            if done {
                answer("Re-implemented beta with its own NAME.")
            } else {
                calls(&[write("mods/beta.py", "NAME = \"beta\"\n\ndef value():\n    return 2\n")])
            }
        } else if p.starts_with("## Review lens: security") {
            let unsafe_code = p.contains("eval(text)");
            seen.lock().unwrap().security_reviews.push(unsafe_code);
            if unsafe_code {
                answer(
                    "```json\n[{\"severity\": \"blocker\", \"file\": \"mods/alpha.py\", \"line\": 7, \"claim\": \"run() passes its \
                     input to eval\", \"evidence\": \"eval(text) on caller input\", \"fix\": \"remove run()\"}]\n```",
                )
            } else {
                answer("[]")
            }
        } else if p.starts_with("## Review lens:") {
            answer("```json\n[]\n```")
        } else if p.starts_with("## Fix (round") {
            if done {
                answer("Removed run().")
            } else {
                assert!(p.contains("mods/alpha.py:7"), "the finding reaches the fix: {p}");
                calls(&[write("mods/alpha.py", ALPHA_SAFE)])
            }
        } else {
            panic!("unscripted step: {}", p.chars().take(200).collect::<String>())
        })
    }
}

#[test]
fn parity_a9_build_v2_runs_a_three_task_dag_end_to_end() {
    let _watchdog = watchdog("parity_a9_build_v2_runs_a_three_task_dag_end_to_end", 420);
    let home = home("b2");
    let base = temp_dir("b2-repo");
    let work = repo(
        &base,
        &[
            (".gitignore", ".flaky\n__pycache__/\n"),
            (".marlowe/project.toml", PROFILE),
            ("run_tests.py", RUN_TESTS),
            ("build.py", BUILD),
            ("lockgen.py", LOCKGEN),
            ("registry.txt", "core\n"),
            ("deps.txt", "base\n"),
            ("deps.lock", "locked base\n"),
            ("mods/core.py", "def value():\n    return 0\n"),
            ("tests/test_legacy.py", "def test():\n    assert False, \"a known bug\"\n"),
            ("tests/test_flaky.py", FLAKY),
        ],
    );
    let seen = Arc::new(Mutex::new(Seen::default()));
    let provider = Scripted::new(script(seen.clone()));
    let mut cfg = CrewConfig::default();
    cfg.build.lockfiles = vec![LockRule { file: "deps.lock".into(), regen: "python lockgen.py".into() }];
    let fx = start(&home, &provider, cfg, true);
    let id = fx.sup.dispatch(None, args("engineer", "Add the alpha, beta and gamma modules.", Some(Playbook::Build), Some(&work))).unwrap().run_id;

    // NeedsYou also shows while an approval (the plan's) is being answered: wait for the result.
    wait("the finished build", 240, || {
        let s = info(&fx.sup, &id);
        assert!(!(s.status.is_terminal() && s.status != CrewRunStatus::NeedsYou), "run ended {:?}, note {:?}", s.status, s.note);
        s.status == CrewRunStatus::NeedsYou && s.result.is_some()
    });
    let run = wait_status(&fx.sup, &id, CrewRunStatus::NeedsYou, 240);
    let result = run.result.clone().unwrap_or_else(|| panic!("a result; note {:?}", run.note));
    let summary = &result.summary;

    // The plan was put to the user, with its quote.
    let asked = fx.approvals.asked.lock().unwrap().clone();
    let plan_ask = asked.iter().find(|a| a.tool == "build_plan").expect("the plan asks for approval");
    assert!(plan_ask.summary.contains("3 tasks in 2 waves"), "{}", plan_ask.summary);
    assert!(plan_ask.detail.contains("Hub files (frozen after the contract stage): registry.txt, deps.txt, .marlowe/project.toml"), "{}", plan_ask.detail);

    // Baseline: the known bug fails twice, the flaky test once.
    let baseline: Value = serde_json::from_str(&std::fs::read_to_string(home.join("runs").join(id.as_str()).join("baseline.json")).unwrap()).unwrap();
    assert_eq!(baseline["failing"], serde_json::json!(["tests/test_legacy.py::test"]));
    assert_eq!(baseline["flaky"], serde_json::json!(["tests/test_flaky.py::test"]));

    let seen = seen.lock().unwrap();
    assert!(seen.frozen_refusal, "the engineer's write to a hub file was refused");
    assert!(seen.t2_took_hub, "the engineer was told the lead's hub change is in its worktree");
    assert!(!seen.t2_hub_undone, "the lead's hub change never reads as the engineer's own edit");
    assert_eq!(seen.t3_rounds, [false, false, true], "two identical failures, a replan, then the revised round");
    assert_eq!(seen.security_reviews, [true, false], "the blocker was found, fixed, and re-reviewed clean");

    let (_, steps) = fx.sup.transcript(&id).unwrap();
    let actions: Vec<String> = steps.iter().map(|s| s.action.clone()).collect();
    let has = |a: &str| actions.iter().any(|x| x.starts_with(a));
    for want in ["plan", "contract", "task t1 round 1", "task t2 round 1", "hub change for t2", "task t2 round 2", "integrate t2: build fix 1", "integrate t2: build fix 2", "serial t2", "replan task t3", "review: security", "fix 1"] {
        assert!(has(want), "missing step {want}: {actions:#?}");
    }
    assert!(!has("integrate t2: build fix 3"), "at most two integrator rounds");
    assert!(!has("task t2 round 3"), "the round after the hub change finishes the task");

    for want in ["3 of 3 tasks delivered", "| t2 | serial |", "Gate vs baseline: no new failures", "baseline: 1 failing, 1 flaky", "security 0 finding(s), 0 blocking", "deps.lock regenerated"] {
        assert!(summary.contains(want), "summary lacks {want}:\n{summary}");
    }
    assert_eq!(result.confidence, Confidence::High, "{summary}");
    assert!(run.note.as_deref().unwrap_or("").contains("merge"), "{:?}", run.note);

    // Approved: the build branch lands on the checkout, with every task and nothing frozen broken.
    let outcome = fx.sup.merge(&id).unwrap();
    assert!(matches!(outcome, MergeOutcome::FastForward { .. }), "{outcome:?}");
    let read = |f: &str| std::fs::read_to_string(work.join(f)).unwrap().replace("\r\n", "\n");
    assert_eq!(read("registry.txt"), "core\nalpha\nbeta\ngamma\n", "the contract's hub edit, untouched by the engineer");
    assert_eq!(read("deps.txt"), "base\nleft\nright\nextra\n", "the lead's hub change, kept");
    assert_eq!(read("deps.lock"), "locked base\nlocked extra\nlocked left\nlocked right\n", "regenerated, never merged by hand");
    assert_eq!(read("mods/alpha.py"), ALPHA_SAFE);
    assert!(read("mods/beta.py").contains("NAME = \"beta\""), "the serial re-implementation");
    assert!(read("mods/gamma.py").contains("alpha.value() + 2"));
    // Child worktrees and their merged branches are gone; the run's own branch went with the merge.
    let branches = git(&work, &["branch", "--list", "crew/*"]);
    assert!(branches.is_empty(), "{branches}");
    assert_eq!(std::fs::read_dir(home.join("worktrees")).map(|d| d.count()).unwrap_or(0), 0);
    drop(seen);
    finish(fx, &[&home, &base]);
}

use marlowe_crew::Confidence;
use serde_json::Value;
