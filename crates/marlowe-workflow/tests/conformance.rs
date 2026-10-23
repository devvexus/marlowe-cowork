//! Gate A17: the workflow semantics conformance suite on the scripted host. One fixture script per
//! behaviour in `tests/conformance/` (WF1-WF5, WF7, WF8); every one must pass.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{unbounded, Receiver};
use marlowe_workflow::journal::{AgentRecord, Resume};
use marlowe_workflow::{
    check, run, AgentCall, AgentResult, AgentStatus, Budget, Control, RunConfig, RunOutcome, ScriptResolver, Scripted, ScriptedHost,
    WfEvent, WorkflowError,
};
use serde_json::{json, Value};

fn fixture(name: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/conformance").join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

struct Saved;

impl ScriptResolver for Saved {
    fn resolve(&self, name: &str) -> Result<(String, String), String> {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/conformance/saved").join(format!("{name}.star"));
        std::fs::read_to_string(&p).map(|s| (format!("{name}.star"), s)).map_err(|_| format!("no saved workflow named {name}"))
    }
}

fn cfg(name: &str, args: Value) -> RunConfig {
    let mut c = RunConfig::new(name, args);
    c.max_concurrency = 8;
    c.resolver = Some(Arc::new(Saved));
    c
}

fn no_control() -> Receiver<Control> {
    unbounded().1
}

/// Runs a fixture; returns the outcome and every event.
fn go(name: &str, cfg: RunConfig, host: Arc<ScriptedHost>) -> (Result<RunOutcome, WorkflowError>, Vec<WfEvent>) {
    let mut events = Vec::new();
    let ctl = no_control();
    let r = run(&fixture(name), cfg, host, &mut |e: &WfEvent| events.push(e.clone()), &ctl);
    (r, events)
}

fn logs(events: &[WfEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            WfEvent::Log(l) => Some(l.clone()),
            _ => None,
        })
        .collect()
}

fn invalid(name: &str) -> String {
    match check(name, &fixture(name)) {
        Ok(_) => panic!("{name} validated"),
        Err(d) => marlowe_workflow::validate::render(&d),
    }
}

#[test]
fn c01_two_agents_in_sequence() {
    let host = ScriptedHost::echo();
    let (r, _) = go("01-two-agents.star", cfg("01", json!({})), host.clone());
    let out = r.unwrap();
    assert_eq!(out.value, json!(["done: first", "done: second after done: first"]));
    assert_eq!(host.prompts(), ["first", "second after done: first"]);
    assert_eq!(out.agents, 2);
}

#[test]
fn c02_parallel_runs_together() {
    let host = ScriptedHost::new(|c| Scripted::after(150, AgentResult::text(c.prompt.clone(), 1)));
    let t = std::time::Instant::now();
    let (r, events) = go("02-parallel.star", cfg("02", json!({})), host.clone());
    assert_eq!(r.unwrap().value, json!(["p0", "p1", "p2"]));
    assert!(t.elapsed() < Duration::from_millis(400), "three 150 ms agents overlapped: {:?}", t.elapsed());
    let calls = host.calls();
    let last_start = calls.iter().map(|c| c.started).max().unwrap();
    let first_end = calls.iter().filter_map(|c| c.finished).min().unwrap();
    assert!(last_start < first_end, "all three started before any finished");
    assert!(events.iter().any(|e| matches!(e, WfEvent::Phase(p) if p == "fan-out")));
}

#[test]
fn c03_a_failing_thunk_is_none() {
    let (r, events) = go("03-parallel-throw.star", cfg("03", json!({})), ScriptedHost::echo());
    assert_eq!(r.unwrap().value, json!([null, "done: ok", null]));
    let l = logs(&events);
    assert!(l.iter().any(|x| x.contains("parallel item 0 failed") && x.contains("this thunk fails")), "{l:?}");
}

#[test]
fn c04_pipeline_has_no_barrier() {
    // B's first stage is slow; A runs all three stages meanwhile.
    let host = ScriptedHost::new(|c| {
        let ms = if c.prompt == "s1 B" { 400 } else { 20 };
        Scripted::after(ms, AgentResult::text(c.prompt.clone(), 1))
    });
    let (r, _) = go("04-pipeline-no-barrier.star", cfg("04", json!({})), host.clone());
    assert_eq!(r.unwrap().value, json!(["s3 A", "s3 B"]));
    let calls = host.calls();
    let get = |p: &str| calls.iter().find(|c| c.call.prompt == p).unwrap().clone();
    let a3 = get("s3 A");
    let b1 = get("s1 B");
    assert!(a3.started < b1.finished.unwrap(), "item A reached stage 3 while B was still in stage 1");
}

#[test]
fn c05_pipeline_stage_arity_and_failure() {
    let host = ScriptedHost::echo();
    let (r, events) = go("05-pipeline-arity.star", cfg("05", json!({})), host.clone());
    assert_eq!(r.unwrap().value, json!(["done: 0:GOOD", null]));
    assert_eq!(host.prompts(), ["0:GOOD"], "the failed item's later stages never ran");
    assert!(logs(&events).iter().any(|l| l.contains("pipeline item 1 stage 1 failed")));
}

#[test]
fn c06_phases_and_log_lines_once_in_order() {
    let (r, events) = go("06-phase-log.star", cfg("06", json!({})), ScriptedHost::echo());
    assert_eq!(r.unwrap().value, json!("done: a"));
    let marks: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            WfEvent::Phase(p) => Some(format!("phase {p}")),
            WfEvent::Log(l) => Some(l.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(marks, ["phase one", "start 1", "after a", "phase two", "done 2"]);
    // Agents carry the phase they were issued in.
    let phases: Vec<Option<String>> = events
        .iter()
        .filter_map(|e| match e {
            WfEvent::Queued { call } => Some(call.opts.phase.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(phases, [Some("one".into()), Some("two".into()), Some("two".into())]);
}

#[test]
fn c07_structured_output_is_a_dict() {
    let host = ScriptedHost::new(|c: &AgentCall| {
        assert!(c.opts.schema.is_some());
        Scripted::now(AgentResult::object(json!({"file": "src/a.rs", "line": 12, "severity": "high"}), 5))
    });
    let (r, _) = go("07-schema.star", cfg("07", json!({})), host);
    assert_eq!(r.unwrap().value, json!({"where": "src/a.rs:12", "severity": "high"}));
}

#[test]
fn c08_an_answer_that_breaks_the_schema_is_none() {
    let host = ScriptedHost::new(|_| Scripted::now(AgentResult::object(json!({"n": "seven"}), 5)));
    let (r, events) = go("08-schema-bad-answer.star", cfg("08", json!({})), host);
    assert_eq!(r.unwrap().value, json!(true));
    assert!(logs(&events).iter().any(|l| l.contains("did not match the schema") && l.contains("/n: expected integer")));
}

#[test]
fn c09_unsatisfiable_schema_throws_with_no_call() {
    let host = ScriptedHost::echo();
    let (r, _) = go("09-schema-unsatisfiable.star", cfg("09", json!({})), host.clone());
    match r {
        Err(WorkflowError::Script(d)) => assert!(d.message.contains("`missing` is required but not in `properties`"), "{d}"),
        other => panic!("{other:?}"),
    }
    assert!(host.calls().is_empty());
}

#[test]
fn c10_budget_loop_stops_on_remaining() {
    let mut c = cfg("10", json!({}));
    c.budget = Some(Budget::new(Some(25)));
    let (r, _) = go("10-budget-loop.star", c, ScriptedHost::echo());
    assert_eq!(r.unwrap().value, json!({"steps": 2, "spent": 20, "total": 25}));
}

#[test]
fn c11_agent_throws_at_the_ceiling() {
    let mut c = cfg("11", json!({}));
    let pool = Budget::new(Some(30));
    c.budget = Some(pool.clone());
    let host = ScriptedHost::echo();
    let (r, _) = go("11-budget-ceiling.star", c, host.clone());
    match r {
        Err(WorkflowError::Script(d)) => assert!(d.message.contains("token budget is spent"), "{d}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(host.calls().len(), 3);
    assert_eq!(pool.spent(), 30);
}

#[test]
fn c12_items_cap() {
    let (r, _) = go("12-too-many-items.star", cfg("12", json!({})), ScriptedHost::echo());
    match r {
        Err(WorkflowError::Script(d)) => assert!(d.message.contains("at most 4096 items"), "{d}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn c13_nested_workflow_one_level() {
    let host = ScriptedHost::echo();
    let (r, _) = go("13-nested.star", cfg("13", json!({"topic": "rust"})), host.clone());
    assert_eq!(r.unwrap().value, json!({"child": "done: research rust"}));
    // `team()` is the same call under its new name; `workflow()` above is the old one.
    let (r, _) = go("13b-nested-team.star", cfg("13b", json!({"topic": "rust"})), host.clone());
    assert_eq!(r.unwrap().value, json!({"child": "done: research rust"}));
    let (r, _) = go("27-child-cannot-nest.star", cfg("27", json!({})), ScriptedHost::echo());
    match r {
        Err(WorkflowError::Script(d)) => assert!(d.message.contains("one level deep"), "{d}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn c14_no_clock_randomness_or_files() {
    let e = invalid("14-forbidden.star");
    assert!(e.contains("time"), "{e}");
    // Each forbidden built-in on its own, so a regression in any one of them shows.
    for (call, name) in [
        ("time.now()", "time"),
        ("random()", "random"),
        ("open(\"notes.txt\")", "open"),
        ("sleep(1)", "sleep"),
        ("now()", "now"),
        ("read_file(\"notes.txt\")", "read_file"),
        ("os.getenv(\"HOME\")", "os"),
        ("load(\"x\")", "load"),
    ] {
        let src = format!("META = {{\"description\": \"d\"}}\ndef main(args):\n    return {call}\n");
        let e = match check("forbidden.star", &src) {
            Ok(_) => panic!("{call} validated"),
            Err(d) => marlowe_workflow::validate::render(&d),
        };
        assert!(e.contains(name), "{call}: {e}");
    }
}

#[test]
fn c15_while_is_a_parse_error() {
    let e = invalid("15-while.star");
    assert!(e.contains("line 4"), "{e}");
}

#[test]
fn c16_recursion_is_refused() {
    let e = invalid("16-recursion.star");
    assert!(e.contains("recursion is not allowed") && e.contains("ping -> pong -> ping"), "{e}");
}

#[test]
fn c17_a_runaway_pass_stops() {
    let mut c = cfg("17", json!({}));
    c.max_statements = 10_000;
    let (r, _) = go("17-runaway-loop.star", c, ScriptedHost::echo());
    match r {
        Err(WorkflowError::Script(d)) => assert!(d.message.contains("statements"), "{d}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn c18_javascript_is_a_parse_error_with_its_line() {
    let e = invalid("18-javascript.star");
    assert!(e.contains("line 1") && e.contains("Starlark"), "{e}");
}

#[test]
fn c19_meta_must_be_a_literal() {
    let host = ScriptedHost::echo();
    let e = invalid("19-meta-not-literal.star");
    assert!(e.contains("pure literal") && e.contains("line 2"), "{e}");
    let (r, _) = go("19-meta-not-literal.star", cfg("19", json!({})), host.clone());
    assert!(matches!(r, Err(WorkflowError::Invalid(_))));
    assert!(host.calls().is_empty(), "no model call");
}

#[test]
fn c20_late_bound_lambdas_are_refused_and_bound_ones_run() {
    let e = invalid("20-late-binding.star");
    assert!(e.contains("loop variable `f`") && e.contains("lambda f=f"), "{e}");
    let (r, _) = go("21-bound-lambdas.star", cfg("21", json!({"files": ["a", "b"]})), ScriptedHost::echo());
    assert_eq!(r.unwrap().value, json!(["done: file a", "done: file b"]));
}

fn records(events: &[WfEvent]) -> Vec<AgentRecord> {
    events
        .iter()
        .filter_map(|e| match e {
            WfEvent::Finished { call, result, cached, completion } => Some(AgentRecord {
                seq: call.index,
                id: call.id.clone(),
                key: call.key.clone(),
                completion: *completion,
                label: call.display_label(),
                phase: call.opts.phase.clone(),
                cached: *cached,
                result: result.clone(),
            }),
            _ => None,
        })
        .collect()
}

#[test]
fn c21_resume_same_script_makes_no_calls() {
    let args = json!({"prompts": ["a", "b", "c", "d"]});
    let (r, events) = go("22-resume.star", cfg("22", args.clone()), ScriptedHost::echo());
    let first = r.unwrap().value;
    let mut c = cfg("22", args);
    c.resume = Some(Resume::from_records(&records(&events), true));
    let host = ScriptedHost::echo();
    let (r, _) = go("22-resume.star", c, host.clone());
    let out = r.unwrap();
    assert_eq!(out.value, first);
    assert_eq!(out.cached, 4);
    assert!(host.calls().is_empty(), "0 model calls");
}

#[test]
fn c22_resume_after_an_edit_keeps_the_unchanged_prefix() {
    let (r, events) = go("22-resume.star", cfg("22", json!({"prompts": ["a", "b", "c", "d"]})), ScriptedHost::echo());
    r.unwrap();
    // The third prompt changed: calls 1-2 cached, 3 and every later call live (even the unchanged 4th).
    let mut c = cfg("22", json!({"prompts": ["a", "b", "C", "d"]}));
    c.resume = Some(Resume::from_records(&records(&events), true));
    let host = ScriptedHost::echo();
    let (r, _) = go("22-resume.star", c, host.clone());
    let out = r.unwrap();
    assert_eq!(out.value, json!(["done: a", "done: b", "done: C", "done: d"]));
    assert_eq!(host.prompts(), ["C", "d"]);
    assert_eq!(out.cached, 2);
}

#[test]
fn c23_resume_in_place_runs_only_the_rest() {
    // A daemon killed after 2 of 4 agents: the journal has two; only the other two run.
    let (r, events) = go("22-resume.star", cfg("22", json!({"prompts": ["a", "b", "c", "d"]})), ScriptedHost::echo());
    r.unwrap();
    let recs: Vec<AgentRecord> = records(&events).into_iter().take(2).collect();
    let mut c = cfg("22", json!({"prompts": ["a", "b", "c", "d"]}));
    c.resume = Some(Resume::from_records(&recs, false));
    let host = ScriptedHost::echo();
    let (r, _) = go("22-resume.star", c, host.clone());
    assert_eq!(r.unwrap().cached, 2);
    assert_eq!(host.prompts(), ["c", "d"]);
}

#[test]
fn c24_agents_run_from_main_only() {
    let e = invalid("23-top-level-agent.star");
    assert!(e.contains("from inside `def main(args):`"), "{e}");
}

#[test]
fn c25_args_verbatim() {
    let args = json!({"n": 1, "list": [1, 2.5, "x", null, true], "nested": {"k": "v"}});
    let (r, _) = go("24-args.star", cfg("24", args.clone()), ScriptedHost::echo());
    assert_eq!(r.unwrap().value, json!({"same": true, "args": args}));
}

#[test]
fn c25b_no_args_is_an_empty_object_so_get_with_a_default_works() {
    let (r, _) = go("24-args.star", cfg("24", serde_json::Value::Null), ScriptedHost::echo());
    assert_eq!(r.unwrap().value, json!({"same": true, "args": {}}));
}

#[test]
fn c26_a_misspelt_name_is_found_before_any_agent() {
    let host = ScriptedHost::echo();
    let e = invalid("25-undefined-name.star");
    assert!(e.contains("agnet"), "{e}");
    let (r, _) = go("25-undefined-name.star", cfg("25", json!({})), host.clone());
    assert!(matches!(r, Err(WorkflowError::Invalid(_))), "{r:?}");
    assert!(host.calls().is_empty());
}

#[test]
fn c27_skip_and_stop() {
    let host = ScriptedHost::new(|c| Scripted::after(if c.prompt == "skip me" { 5_000 } else { 50 }, AgentResult::text(c.prompt.clone(), 1)));
    let (tx, rx) = unbounded();
    let src = fixture("26-skip.star");
    let h = host.clone();
    let worker = std::thread::spawn(move || {
        let mut events = Vec::new();
        let r = run(&src, cfg("26", json!({})), h, &mut |e: &WfEvent| events.push(e.clone()), &rx);
        (r, events)
    });
    // Skip the slow one once it has started.
    let t = std::time::Instant::now();
    while !host.prompts().iter().any(|p| p == "skip me") && t.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(5));
    }
    let index = host.calls().iter().find(|c| c.call.prompt == "skip me").unwrap().call.index;
    tx.send(Control::Skip(index)).unwrap();
    let (r, events) = worker.join().unwrap();
    assert_eq!(r.unwrap().value, json!(["keep", null, "keep too"]));
    assert!(events.iter().any(|e| matches!(e, WfEvent::Finished { result, .. } if result.status == AgentStatus::Skipped)));
    assert!(t.elapsed() < Duration::from_secs(4), "the skipped agent was not waited for");

    // Stop: no new agent starts, the run ends Stopped.
    let host = ScriptedHost::new(|c| Scripted::after(300, AgentResult::text(c.prompt.clone(), 1)));
    let (tx, rx) = unbounded();
    let src = fixture("22-resume.star");
    let h = host.clone();
    let worker = std::thread::spawn(move || {
        run(&src, cfg("22", json!({"prompts": ["a", "b", "c"]})), h, &mut marlowe_workflow::NoObserver, &rx)
    });
    std::thread::sleep(Duration::from_millis(100));
    tx.send(Control::Stop).unwrap();
    assert_eq!(worker.join().unwrap(), Err(WorkflowError::Stopped));
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(host.prompts(), ["a"], "nothing started after the stop");
    assert!(host.calls()[0].cancelled, "the in-flight agent was cancelled");
}

#[test]
fn c28_agent_cap() {
    let mut c = cfg("22", json!({"prompts": ["a", "b", "c", "d"]}));
    c.max_agents = 3;
    let (r, _) = go("22-resume.star", c, ScriptedHost::echo());
    match r {
        Err(WorkflowError::Script(d)) => assert!(d.message.contains("at most 3 agents"), "{d}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn c29_every_fixture_has_a_test() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/conformance");
    let n = std::fs::read_dir(dir).unwrap().filter(|e| e.as_ref().unwrap().path().extension().is_some_and(|x| x == "star")).count();
    assert!(n >= 20, "gate A17 needs at least 20 fixture scripts, found {n}");
}

#[test]
fn c31_a_thousand_agents() {
    let src = "META = {\"description\": \"fan out\"}\ndef main(args):\n    r = pipeline(range(1000), lambda prev, item, i: agent(\"n\" + str(item)))\n    return len([x for x in r if x])\n";
    let mut c = cfg("thousand", json!({}));
    c.max_concurrency = 16;
    let host = ScriptedHost::new(|c| Scripted::now(AgentResult::text(c.prompt.clone(), 1)));
    let t = std::time::Instant::now();
    let out = run(src, c, host.clone(), &mut marlowe_workflow::NoObserver, &no_control()).unwrap();
    assert_eq!(out.value, json!(1000));
    assert_eq!(host.calls().len(), 1000);
    assert!(t.elapsed() < Duration::from_secs(60), "{:?}", t.elapsed());
    eprintln!("1000 agents: {:?}", t.elapsed());
}

#[test]
fn c30_failed_agents_are_none_and_logged() {
    let host = ScriptedHost::new(|c| {
        if c.prompt == "first" {
            Scripted::now(AgentResult::failed("the model call failed", 3))
        } else {
            Scripted::now(AgentResult::text("x", 1))
        }
    });
    let src = "META = {\"description\": \"d\"}\ndef main(args):\n    return [agent(\"first\"), agent(\"second\")]\n";
    let mut events = Vec::new();
    let r = run(src, cfg("inline", json!({})), host, &mut |e: &WfEvent| events.push(e.clone()), &no_control()).unwrap();
    assert_eq!(r.value, json!([null, "x"]));
    assert!(logs(&events).iter().any(|l| l.contains("failed: the model call failed")));
    assert_eq!(r.tokens, 4);
}

/// The fatal message a fixture run ends with.
fn script_error(r: Result<RunOutcome, WorkflowError>) -> String {
    match r {
        Err(WorkflowError::Script(d)) => d.message,
        other => panic!("{other:?}"),
    }
}

#[test]
fn c32_comprehension_iterations_count_like_statements() {
    // Comprehensions run no statement; their iterations are counted all the same.
    for kind in ["flat", "filtered", "nested", "dict"] {
        let mut c = cfg("28", json!({kind: true}));
        c.max_statements = 20_000;
        let t = std::time::Instant::now();
        let (r, _) = go("28-runaway-comprehension.star", c, ScriptedHost::echo());
        let m = script_error(r);
        assert!(m.contains("statements"), "{kind}: {m}");
        assert!(t.elapsed() < Duration::from_secs(20), "{kind}: {:?}", t.elapsed());
    }
    // An ordinary comprehension is untouched.
    let (r, _) = go("28-runaway-comprehension.star", cfg("28", json!({})), ScriptedHost::echo());
    assert_eq!(r.unwrap().value, json!(5));
}

#[test]
fn c33_huge_values_end_with_an_error_before_they_are_built() {
    let cases = [
        ("list", "MB"),
        ("string", "MB"),
        ("bigint-string", "MB"),
        ("shift", "4096 bits"),
        ("range", "range() gives at most"),
        ("list-range", "range() gives at most"),
        ("parallel", "range() gives at most"),
        ("doubling", "MB of values"),
        ("lambda-doubling", "`+` would build"),
        ("list-concat", "`+` would build"),
    ];
    for (kind, says) in cases {
        let mut c = cfg("29", json!({"kind": kind}));
        // A small heap ceiling keeps the test itself small (the `+` cases stop on their own).
        if !matches!(kind, "lambda-doubling" | "list-concat") {
            c.max_heap_bytes = 8 << 20;
        }
        let (r, _) = go("29-huge-values.star", c, ScriptedHost::echo());
        let m = script_error(r);
        assert!(m.contains(says), "{kind}: {m}");
    }
    // Small repeats and shifts work as in Starlark.
    let (r, _) = go("29-huge-values.star", cfg("29", json!({"kind": "small"})), ScriptedHost::echo());
    assert_eq!(r.unwrap().value, json!(6 + 4 + 12));
}

#[test]
fn c34_a_variable_the_loop_body_sets_is_late_bound_too() {
    let e = invalid("30-late-binding-body.star");
    assert!(e.contains("`p`") && e.contains("lambda p=p"), "{e}");
    // Bound as a default, the same loop runs one agent per file.
    let fixed = fixture("30-late-binding-body.star").replace("lambda: agent(p)", "lambda p=p: agent(p)");
    let mut events = Vec::new();
    let out = run(&fixed, cfg("30", json!({"files": ["a", "b"]})), ScriptedHost::echo(), &mut |e: &WfEvent| events.push(e.clone()), &no_control()).unwrap();
    assert_eq!(out.value, json!(["done: Review a", "done: Review b"]));
}

#[test]
fn c35_the_check_runs_nothing_and_the_run_is_bounded() {
    // The check (before approval, on the conductor's thread) resolves names and runs nothing, so
    // a runaway top level costs it nothing.
    let t = std::time::Instant::now();
    check("31-top-level-runaway.star", &fixture("31-top-level-runaway.star")).unwrap();
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    // Run, the top level is bounded like everything else.
    let mut c = cfg("31", json!({}));
    c.max_statements = 20_000;
    let (r, _) = go("31-top-level-runaway.star", c, ScriptedHost::echo());
    assert!(script_error(r).contains("statements"));
}

#[test]
fn c37_nothing_in_a_script_runs_before_approval() {
    // A top level that would fail, or build something huge, still validates: it never ran.
    let src = "META = {\"description\": \"d\"}
BIG = \"x\" * 1000000000
STOP = fail(\"the top level ran\")
def main(args):
    return 1
";
    check("inline.star", src).unwrap();
    // A misspelt name anywhere is still found, and so is a direct agent call at the top level.
    let typo = "META = {\"description\": \"d\"}\nX = 1\ndef main(args):\n    return agnet(\"x\")\n";
    assert!(matches!(check("inline.star", typo), Err(d) if d[0].message.contains("agnet")));
    let early = "META = {\"description\": \"d\"}\nX = [parallel([])]\ndef main(args):\n    return X\n";
    assert!(matches!(check("inline.star", early), Err(d) if d[0].message.contains("call parallel() from inside") && d[0].line == Some(2)));
    // A lambda or a function at the top level may call agent: it runs later, from main.
    let later = "META = {\"description\": \"d\"}\nASK = lambda q: agent(q)\ndef main(args):\n    return ASK(\"x\")\n";
    check("inline.star", later).unwrap();
    // The top level reads `args` like main does.
    let topic = "META = {\"description\": \"d\"}\nTOPIC = args.get(\"topic\", \"none\")\ndef main(args):\n    return TOPIC\n";
    check("inline.star", topic).unwrap();
    let out = run(topic, cfg("inline", json!({"topic": "rust"})), ScriptedHost::echo(), &mut |_: &WfEvent| {}, &no_control()).unwrap();
    assert_eq!(out.value, json!("rust"));
}

#[test]
fn c38_lambda_calls_count_like_statements() {
    // `map`, `filter` and `sorted(key=...)` loop inside the interpreter; the lambda is what ticks.
    for (kind, src) in [
        ("map", "len(map(lambda i: len(map(lambda j: 0, range(100000))), range(100000)))"),
        ("filter", "len(filter(lambda i: len(filter(lambda j: False, range(100000))) > 0, range(100000)))"),
        ("sorted", "len(sorted(range(100000), key = lambda i: len(sorted(range(100000), key = lambda j: -j))))"),
    ] {
        let source = format!("META = {{\"description\": \"d\"}}\ndef main(args):\n    return {src}\n");
        let mut c = cfg("inline", json!({}));
        c.max_statements = 20_000;
        let t = std::time::Instant::now();
        let r = run(&source, c, ScriptedHost::echo(), &mut |_: &WfEvent| {}, &no_control());
        assert!(script_error(r).contains("statements"), "{kind}");
        assert!(t.elapsed() < Duration::from_secs(20), "{kind}: {:?}", t.elapsed());
    }
    // An ordinary lambda is untouched.
    let ok = "META = {\"description\": \"d\"}\ndef main(args):\n    return sorted(map(lambda x: x * 2, [3, 1, 2]), key = lambda y: -y)\n";
    let out = run(ok, cfg("inline", json!({})), ScriptedHost::echo(), &mut |_: &WfEvent| {}, &no_control()).unwrap();
    assert_eq!(out.value, json!([6, 4, 2]));
}

#[test]
fn c39_a_pass_has_a_time_limit() {
    let src = "META = {\"description\": \"d\"}\ndef main(args):\n    n = 0\n    for i in range(100000):\n        n = n + 1\n    return n\n";
    let mut c = cfg("inline", json!({}));
    c.max_pass_time = Duration::ZERO;
    let r = run(src, c, ScriptedHost::echo(), &mut |_: &WfEvent| {}, &no_control());
    assert!(script_error(r).contains("ran longer than"));
}

#[test]
fn c40_prompts_and_log_lines_have_a_size_limit() {
    let src = "META = {\"description\": \"d\"}
def main(args):
    log(\"y\" * 10000)
    big = \"z\" * 2000000
    return [agent(\"short\"), parallel([lambda: agent(big)])]
";
    let host = ScriptedHost::echo();
    let mut events = Vec::new();
    let out = run(src, cfg("inline", json!({})), host.clone(), &mut |e: &WfEvent| events.push(e.clone()), &no_control()).unwrap();
    assert_eq!(out.value, json!(["done: short", [null]]));
    assert_eq!(host.calls().len(), 1, "the oversized prompt never reached a model");
    let l = logs(&events);
    assert!(l.iter().any(|x| x.starts_with("yyy") && x.ends_with("[...]") && x.len() < 4100), "{:?}", l.iter().map(|x| x.len()).collect::<Vec<_>>());
    assert!(l.iter().any(|x| x.contains("parallel item 0 failed") && x.contains("keep it under 1024 KB")), "{l:?}");
}

#[test]
fn c41_each_failing_item_is_logged_once() {
    // Item 0 fails on the second pass (after its agent finished), item 1 on every pass: each
    // failure line appears exactly once, whatever order they fail in.
    let src = "META = {\"description\": \"d\"}
def main(args):
    a = parallel([lambda: int(agent(\"a\")), lambda: int(\"b\"), lambda: agent(\"c\")])
    b = pipeline([\"p\", \"q\"], lambda prev, item: agent(item), lambda prev: int(prev) if prev == \"done: p\" else int(\"x\" + prev))
    return [a, b]
";
    let host = ScriptedHost::new(|c| Scripted::after(if c.prompt == "a" { 50 } else { 10 }, AgentResult::text(format!("done: {}", c.prompt), 1)));
    let mut events = Vec::new();
    let out = run(src, cfg("inline", json!({})), host, &mut |e: &WfEvent| events.push(e.clone()), &no_control()).unwrap();
    assert_eq!(out.value, json!([[null, null, "done: c"], [null, null]]));
    let l = logs(&events);
    for item in ["parallel item 0 failed", "parallel item 1 failed", "pipeline item 0 stage 1 failed", "pipeline item 1 stage 1 failed"] {
        assert_eq!(l.iter().filter(|x| x.contains(item)).count(), 1, "{item}: {l:#?}");
    }
}

#[test]
fn c36_guard_names_and_augmented_repeats_are_refused() {
    let src = "META = {\"description\": \"d\"}
_wf_tick = lambda x: x
def main(args):
    s = \"x\"
    s *= 3
    n = 1
    n <<= 2
    return s
";
    let e = match check("guards.star", src) {
        Ok(_) => panic!("validated"),
        Err(d) => marlowe_workflow::validate::render(&d),
    };
    assert!(e.contains("line 2") && e.contains("reserved"), "{e}");
    assert!(e.contains("line 5") && e.contains("x = x * n"), "{e}");
    assert!(e.contains("line 7") && e.contains("x = x << n"), "{e}");
}

#[test]
fn c32_parallel_takes_prompt_strings_and_runs_them_together() {
    let host = ScriptedHost::new(|c| Scripted::after(150, AgentResult::text(c.prompt.clone(), 1)));
    let t = std::time::Instant::now();
    let (r, events) = go("32-parallel-prompts.star", cfg("32", json!({})), host.clone());
    assert_eq!(r.unwrap().value, json!(["p0", "p1", "p2"]));
    assert!(t.elapsed() < Duration::from_millis(400), "overlapped: {:?}", t.elapsed());
    let calls = host.calls();
    assert!(calls.iter().map(|c| c.started).max().unwrap() < calls.iter().filter_map(|c| c.finished).min().unwrap());
    assert!(events.iter().any(|e| matches!(e, WfEvent::Phase(p) if p == "fan-out")));
    // A value that is neither a prompt nor a function is still refused, naming both forms.
    let e = match check("x.star", "META = {\"description\": \"d\"}\ndef main(args):\n    return parallel([1])\n") {
        Ok(_) => {
            let (r, _) = go_source("META = {\"description\": \"d\"}\ndef main(args):\n    return parallel([1])\n", ScriptedHost::echo());
            format!("{:?}", r.err())
        }
        Err(d) => marlowe_workflow::validate::render(&d),
    };
    assert!(e.contains("prompt strings or functions"), "{e}");
}

fn go_source(src: &str, host: Arc<ScriptedHost>) -> (Result<RunOutcome, WorkflowError>, Vec<WfEvent>) {
    let mut events = Vec::new();
    let ctl = no_control();
    let r = run(src, cfg("src", json!({})), host, &mut |e: &WfEvent| events.push(e.clone()), &ctl);
    (r, events)
}
