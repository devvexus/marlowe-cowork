//! build_v2's pure parts: plan validation and waves, findings, hub requests, and the gate's
//! baseline and retry logic over a scripted shell. The playbook end to end is in
//! `tests/build_v2.rs`.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use marlowe_proto::SandboxKind;
use marlowe_verify::profile::Kind;

use super::gate::{self, GateSpec, GateStep};
use super::parse::{hub_requests, json_value, parse_findings, parse_plan};
use super::types::{Baseline, Lens, Severity};
use crate::playbook::{ShellResult, StepResult, StepRunner, StepSpec};
use crate::sandbox::Sandbox;
use crate::CrewError;

#[test]
fn a_plan_parses_validates_and_groups_into_waves() {
    let text = "Here is the plan.\n```json\n{\"tasks\": [\
        {\"id\": \"t1\", \"goal\": \"alpha\", \"files_owned\": [\"mods/alpha.py\"], \"depends_on\": [], \"accept_cmd\": \"python t.py a\"},\
        {\"id\": \"t2\", \"goal\": \"beta\", \"files_owned\": [\"mods/beta.py\", \"registry.txt\"]},\
        {\"id\": \"t3\", \"goal\": \"gamma\", \"files_owned\": [\"mods/gamma.py\"], \"depends_on\": [\"t1\"],},\
    ], \"hubs\": [\"registry.txt\"]}\n```";
    let plan = parse_plan(text, &["Cargo.toml".into()]).unwrap();
    assert_eq!(plan.tasks.len(), 3);
    assert_eq!(plan.hubs, ["registry.txt", "Cargo.toml"]);
    assert_eq!(plan.tasks[0].accept_cmd.as_deref(), Some("python t.py a"));
    assert_eq!(plan.waves(), vec![vec!["t1".to_string(), "t2".into()], vec!["t3".into()]]);
    // A shared hub is not an overlap.
    assert!(plan.notes.is_empty(), "{:?}", plan.notes);
}

#[test]
fn overlapping_owned_files_serialise_and_bad_plans_say_every_problem() {
    let text = r#"{"tasks": [
        {"id": "a", "goal": "x", "files_owned": ["src/lib.rs", "src/a.rs"]},
        {"id": "b", "goal": "y", "files_owned": ["src/a.rs"]}
    ]}"#;
    let plan = parse_plan(text, &[]).unwrap();
    assert_eq!(plan.tasks[1].depends_on, ["a"]);
    assert_eq!(plan.waves().len(), 2);
    assert!(plan.notes[0].contains("src/a.rs"), "{:?}", plan.notes);

    let bad = r#"{"tasks": [
        {"id": "a", "goal": "x", "depends_on": ["b"]},
        {"id": "b", "goal": "y", "depends_on": ["a"]},
        {"id": "c", "goal": "", "depends_on": ["zz"], "files_owned": ["../etc/x"]},
        {"id": "c", "goal": "z"},
        {"id": "Bad Id", "goal": "w"}
    ]}"#;
    let errors = parse_plan(bad, &[]).unwrap_err();
    let all = errors.join("\n");
    for want in ["`c` has no `goal`", "depends on `zz`", "relative to the repository root", "used twice", "Bad Id"] {
        assert!(all.contains(want), "missing {want}: {all}");
    }
    let cyc = parse_plan(r#"[{"id": "a", "goal": "x", "depends_on": ["b"]}, {"id": "b", "goal": "y", "depends_on": ["a"]}]"#, &[]).unwrap_err();
    assert!(cyc[0].contains("cycle"), "{cyc:?}");
    assert!(parse_plan("no json at all", &[]).unwrap_err()[0].contains("no JSON plan"));
}

#[test]
fn findings_parse_leniently_and_an_unreadable_reply_is_an_error() {
    let text = "Found one.\n```json\n[{\"severity\": \"blocker\", \"file\": \"mods\\\\alpha.py\", \"line\": \"3\", \"claim\": \"eval of input\", \"evidence\": \"line 3\", \"fix\": \"parse it\"},]\n```";
    let f = parse_findings(text, Lens::Security).unwrap();
    assert_eq!(f.len(), 1);
    assert_eq!((f[0].severity, f[0].file.as_str(), f[0].line), (Severity::Blocker, "mods/alpha.py", 3));
    assert!(f[0].severity.blocks() && !Severity::Minor.blocks());
    assert!(parse_findings("[]", Lens::Correctness).unwrap().is_empty());
    assert!(parse_findings("No findings through this lens.", Lens::Performance).unwrap().is_empty());
    assert_eq!(parse_findings(r#"{"findings": [{"severity": "nit", "file": "a", "claim": "c"}]}"#, Lens::Correctness).unwrap().len(), 1);
    assert!(parse_findings(r#"[{"severity": "awful", "claim": "c"}]"#, Lens::Correctness).unwrap_err().contains("severity"));
    assert!(parse_findings("I looked and it seems fine overall", Lens::Correctness).is_err());
    // An array of objects is the array, not its first object.
    assert!(json_value("x [ {\"a\": 1}, {\"a\": 2} ] y").unwrap().is_array());
}

#[test]
fn hub_requests_are_read_from_the_reply() {
    let r = hub_requests("Done.\nNEED_HUB_CHANGE: registry.txt: add gamma\n- NEED_HUB_CHANGE: `deps\\x.txt`: bump\nok");
    assert_eq!(r, vec![("registry.txt".to_string(), "add gamma".to_string()), ("deps/x.txt".into(), "bump".into())]);
    assert!(hub_requests("nothing needed").is_empty());
}

/// Shell results from per-command queues; the last result of a queue repeats.
struct ShellScript {
    sandbox: Sandbox,
    queues: Mutex<HashMap<String, VecDeque<ShellResult>>>,
    runs: Mutex<Vec<String>>,
}

impl ShellScript {
    fn new(script: &[(&str, &[(bool, &str)])]) -> ShellScript {
        let root = std::env::temp_dir();
        let sandbox = Sandbox {
            kind: SandboxKind::Scratch,
            root,
            origin: None,
            branch: None,
            base: None,
            repo_digest: None,
            frozen: Vec::new(),
            target_dir: None,
        };
        let queues = script
            .iter()
            .map(|(cmd, rs)| {
                let q = rs.iter().map(|(ok, out)| ShellResult { ok: *ok, exit_code: Some(if *ok { 0 } else { 1 }), output: out.to_string() }).collect();
                (cmd.to_string(), q)
            })
            .collect();
        ShellScript { sandbox, queues: Mutex::new(queues), runs: Mutex::new(Vec::new()) }
    }
}

impl StepRunner for ShellScript {
    fn step(&self, spec: StepSpec) -> Result<StepResult, CrewError> {
        Err(CrewError::Step { step: spec.key, message: "no agents here".into() })
    }

    fn parallel(&self, specs: Vec<StepSpec>) -> Vec<Result<StepResult, CrewError>> {
        specs.into_iter().map(|s| self.step(s)).collect()
    }

    fn shell(&self, command: &str) -> Result<ShellResult, CrewError> {
        self.runs.lock().unwrap().push(command.to_string());
        let mut q = self.queues.lock().unwrap();
        let queue = q.get_mut(command).expect("a scripted command");
        Ok(if queue.len() > 1 { queue.pop_front().unwrap() } else { queue.front().cloned().unwrap() })
    }

    fn sandbox(&self) -> &Sandbox {
        &self.sandbox
    }

    fn progress(&self, _action: &str) {}
}

fn spec() -> GateSpec {
    GateSpec {
        steps: vec![
            GateStep { kind: Kind::Build, command: "build".into() },
            GateStep { kind: Kind::Test, command: "test".into() },
        ],
    }
}

#[test]
fn the_baseline_retries_failures_once_and_flaky_ones_are_marked() {
    let s = ShellScript::new(&[
        ("build", &[(true, "ok")]),
        (
            "test",
            &[
                (false, "FAILED tests/test_legacy.py::test\nFAILED tests/test_flaky.py::test\n"),
                (false, "FAILED tests/test_legacy.py::test\nPASSED tests/test_flaky.py::test\n"),
            ],
        ),
    ]);
    let b = gate::baseline(&s, &s.sandbox, &spec(), "abc").unwrap();
    assert_eq!(b.failing, ["tests/test_legacy.py::test"]);
    assert_eq!(b.flaky, ["tests/test_flaky.py::test"]);
    assert_eq!(b.commands, ["build: build", "test: test"]);
    assert_eq!(s.runs.lock().unwrap().len(), 4, "each failure retried once: the gate ran twice");
}

#[test]
fn the_check_excuses_the_baseline_and_retries_new_failures() {
    let base = Baseline { failing: vec!["tests/old.py::test".into()], flaky: vec!["tests/flaky.py::test".into()], ..Default::default() };
    // Old and flaky failures do not block; nothing is retried.
    let s = ShellScript::new(&[("build", &[(true, "")]), ("test", &[(false, "FAILED tests/old.py::test\nFAILED tests/flaky.py::test")])]);
    let c = gate::check(&s, &s.sandbox, &spec(), &base).unwrap();
    assert!(c.green() && c.flaky.is_empty());
    assert_eq!(s.runs.lock().unwrap().len(), 2);
    // A new failure that passes on the retry is flaky; one that fails again is new.
    let s = ShellScript::new(&[
        ("build", &[(true, "")]),
        ("test", &[(false, "FAILED tests/new.py::test\nFAILED tests/blip.py::test"), (false, "FAILED tests/new.py::test")]),
    ]);
    let c = gate::check(&s, &s.sandbox, &spec(), &base).unwrap();
    assert_eq!(c.new_failures, ["tests/new.py::test"]);
    assert_eq!(c.flaky, ["tests/blip.py::test"]);
    assert_eq!(c.fixed, ["tests/old.py::test"]);
    assert!(c.evidence.contains("tests/new.py"));
    // A failing build stops the gate and fails without naming a test.
    let s = ShellScript::new(&[("build", &[(false, "error: nope")]), ("test", &[(true, "")])]);
    let c = gate::check(&s, &s.sandbox, &spec(), &Baseline::default()).unwrap();
    assert_eq!(c.new_failures, ["<build>"]);
    assert!(!s.runs.lock().unwrap().iter().any(|r| r == "test"));
}
