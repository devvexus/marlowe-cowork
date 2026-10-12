//! The built-in workflows (G11 code-review, G12 simplify) on the scripted host, and saved-workflow
//! resolution (WF6).

use std::path::PathBuf;
use std::sync::Arc;

use crossbeam_channel::unbounded;
use marlowe_workflow::saved::{self, Roots};
use marlowe_workflow::{run, AgentCall, AgentResult, RunConfig, Scripted, ScriptedHost, WfEvent};
use serde_json::{json, Value};

fn builtin(name: &str) -> String {
    saved::builtin(name).unwrap().to_string()
}

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("marlowe-wf-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A reviewer model: each finder reports one finding for its lens (the correctness finder the
/// planted bug); verifiers call the planted bug real and everything else not.
fn reviewer(c: &AgentCall) -> Scripted {
    let label = c.opts.label.clone().unwrap_or_default();
    if let Some(lens) = label.strip_prefix("find: ") {
        let f = if lens == "correctness" {
            json!({"lens": "correctness", "severity": "high", "file": "src/sum.rs", "line": 4, "claim": "the loop skips the last element (i < n - 1)", "evidence": "for i in 0..n - 1", "fix": "iterate 0..n"})
        } else {
            json!({"lens": lens, "severity": "low", "file": "src/sum.rs", "line": 9, "claim": format!("{lens} nit"), "evidence": "-", "fix": "-"})
        };
        return Scripted::now(AgentResult::object(json!({"findings": [f]}), 20));
    }
    if label.starts_with("verify") {
        let real = c.prompt.contains("skips the last element");
        return Scripted::now(AgentResult::object(json!({"real": real, "why": "checked"}), 5));
    }
    Scripted::now(AgentResult::text(format!("done: {label}"), 5))
}

fn review(args: Value) -> (Value, Arc<ScriptedHost>, Vec<WfEvent>) {
    let host = ScriptedHost::new(reviewer);
    let mut events = Vec::new();
    let ctl = unbounded().1;
    let out = run(&builtin("code-review"), RunConfig::new("code-review.star", args), host.clone(), &mut |e: &WfEvent| events.push(e.clone()), &ctl)
        .unwrap();
    (out.value, host, events)
}

#[test]
fn parity_g11_code_review_finds_the_planted_bug_at_medium() {
    let (v, host, _) = review(json!({"level": "medium"}));
    let findings = v["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1, "{v}");
    assert_eq!(findings[0]["file"], "src/sum.rs");
    assert!(findings[0]["claim"].as_str().unwrap().contains("skips the last element"));
    assert_eq!(v["refuted"], 1, "the security nit was refuted");
    assert_eq!(v["fix"], Value::Null, "no fix without --fix");
    // Every finder read the pending diff.
    assert!(host.prompts().iter().filter(|p| p.starts_with("Review the pending changes")).count() == 2);
}

#[test]
fn parity_g11_level_sets_the_agent_count() {
    // finders = lenses; verifiers = findings x votes (each finder reports one finding).
    for (level, agents) in [("low", 1 + 1), ("medium", 2 + 2), ("high", 3 + 3 * 3), ("xhigh", 4 + 4 * 3), ("max", 5 + 5 * 5)] {
        let (_, host, _) = review(json!({"level": level}));
        assert_eq!(host.calls().len(), agents, "level {level}");
    }
}

#[test]
fn parity_g11_fix_and_max_findings() {
    let (v, host, events) = review(json!({"level": "low", "fix": true, "max_findings": 1, "comment": true}));
    assert_eq!(v["findings"].as_array().unwrap().len(), 1);
    assert_eq!(v["comment"], true);
    let fix = host.calls().into_iter().find(|c| c.call.opts.label.as_deref() == Some("fix")).expect("a fix agent");
    assert!(fix.call.prompt.contains("src/sum.rs:4") && fix.call.prompt.contains("run the project's tests"));
    assert!(events.iter().any(|e| matches!(e, WfEvent::Phase(p) if p == "fix")));
    let (v, _, _) = review(json!({"level": "max", "max_findings": "all"}));
    assert_eq!(v["findings"].as_array().unwrap().len(), 1, "only the planted bug survives verification");
}

#[test]
fn parity_g11_bad_level_fails_cleanly() {
    let host = ScriptedHost::echo();
    let r = run(&builtin("code-review"), RunConfig::new("code-review.star", json!({"level": "extreme"})), host.clone(), &mut marlowe_workflow::NoObserver, &unbounded().1);
    assert!(format!("{r:?}").contains("level must be one of"), "{r:?}");
    assert!(host.calls().is_empty());
}

#[test]
fn parity_g12_simplify_applies_the_reuse_finding() {
    let host = ScriptedHost::new(|c| {
        let label = c.opts.label.clone().unwrap_or_default();
        if label == "find: reuse" {
            return Scripted::now(AgentResult::object(
                json!({"findings": [{"lens": "reuse", "severity": "low", "file": "src/util.rs", "line": 12, "claim": "clamp_pct duplicates util::clamp", "evidence": "same body", "fix": "call util::clamp"}]}),
                10,
            ));
        }
        if label.starts_with("find: ") {
            return Scripted::now(AgentResult::object(json!({"findings": []}), 10));
        }
        Scripted::now(AgentResult::text("applied 1; tests pass", 10))
    });
    let out = run(&builtin("simplify"), RunConfig::new("simplify.star", json!({})), host.clone(), &mut marlowe_workflow::NoObserver, &unbounded().1).unwrap();
    assert_eq!(out.value["findings"][0]["claim"], "clamp_pct duplicates util::clamp");
    assert_eq!(out.value["applied"], "applied 1; tests pass");
    let apply = host.calls().into_iter().find(|c| c.call.opts.label.as_deref() == Some("apply")).unwrap();
    assert!(apply.call.prompt.contains("call util::clamp") && apply.call.prompt.contains("Run the project's tests"));
    // Quality only: no lens asks for bugs.
    assert!(host.prompts().iter().all(|p| !p.starts_with("Review")));
}

#[test]
fn parity_wf6_project_shadows_user_and_builtins() {
    let ws = temp("ws");
    let home = temp("home");
    std::fs::create_dir_all(ws.join(".marlowe/workflows")).unwrap();
    std::fs::create_dir_all(home.join("workflows")).unwrap();
    let script = |d: &str| format!("META = {{\"description\": \"{d}\"}}\ndef main(args):\n    return \"{d}\"\n");
    std::fs::write(home.join("workflows/triage.star"), script("user triage")).unwrap();
    std::fs::write(home.join("workflows/only-user.star"), script("user only")).unwrap();
    std::fs::write(ws.join(".marlowe/workflows/triage.star"), script("project triage")).unwrap();
    std::fs::write(ws.join(".marlowe/workflows/code-review.star"), script("project review")).unwrap();
    let roots = Roots { workspaces: vec![ws.clone()], home: Some(home.clone()), plugins: vec![] };
    assert!(roots.resolve("triage").unwrap().source.contains("project triage"));
    assert!(roots.resolve("only-user").unwrap().source.contains("user only"));
    assert!(roots.resolve("code-review").unwrap().source.contains("project review"), "a project file shadows the built-in");
    assert!(roots.resolve("simplify").unwrap().origin == saved::Origin::Builtin);
    let list = roots.list();
    let names: Vec<&str> = list.iter().map(|(s, _)| s.name.as_str()).collect();
    assert_eq!(names, ["code-review", "triage", "only-user", "simplify"]);
    assert_eq!(list[1].0.description, "project triage");
    // The index for the prompt stays within its budget.
    let idx = saved::index(&list.iter().map(|(s, _)| s.clone()).collect::<Vec<_>>(), 120);
    assert!(idx.len() <= 160 && idx.contains("more: /workflows"), "{idx}");
}

/// `workflow(name)` inside a running workflow: the parent's approval never showed the child, so
/// only a built-in or a digest approved on its own runs; a relative path is in the workspace.
#[test]
fn a_child_workflow_runs_only_when_built_in_or_approved() {
    use marlowe_workflow::service;
    use marlowe_workflow::ScriptResolver;
    let (ws, home) = (temp("child-ws"), temp("child-home"));
    std::fs::create_dir_all(ws.join(".marlowe/workflows")).unwrap();
    let src = "META = {\"description\": \"child\"}\ndef main(args):\n    return 1\n";
    std::fs::write(ws.join(".marlowe/workflows/child.star"), src).unwrap();
    let roots = service::roots(&home, Some(&ws), vec![]);
    let child = |r: &Roots, n: &str| ScriptResolver::resolve(r, n);

    assert!(child(&roots, "child").unwrap_err().contains("has not been approved"));
    assert!(child(&roots, ".marlowe/workflows/child.star").unwrap_err().contains("has not been approved"));
    assert!(child(&roots, "code-review").is_ok(), "built-ins always run");

    service::trust(&home, &service::prepare(&roots, None, None, Some("child")).unwrap()).unwrap();
    assert_eq!(child(&roots, "child").unwrap().1, src);
    assert_eq!(child(&roots, ".marlowe/workflows/child.star").unwrap().1, src, "relative to the workspace");

    // Edited after its approval: the new version is not approved.
    std::fs::write(ws.join(".marlowe/workflows/child.star"), format!("{src}# changed\n")).unwrap();
    assert!(child(&roots, "child").unwrap_err().contains("has not been approved"));
    // A relative path with no workspace is not read from the daemon's directory.
    let bare = service::roots(&home, None, vec![]);
    assert!(child(&bare, ".marlowe/workflows/child.star").unwrap_err().contains("no workspace"));
}

#[test]
fn parity_wf6_cc_javascript_is_listed_and_named_for_conversion() {
    let ws = temp("cc");
    std::fs::create_dir_all(ws.join(".claude/workflows")).unwrap();
    std::fs::write(
        ws.join(".claude/workflows/portfolio-assess.js"),
        "export const meta = { name: 'portfolio-assess', description: 'Assess each system', phases: ['collect'] };\nconst r = await agent('x');\nreturn r;\n",
    )
    .unwrap();
    let roots = Roots { workspaces: vec![ws], home: None, plugins: vec![] };
    let list = roots.list();
    let js = list.iter().find(|(s, _)| s.name == "portfolio-assess").unwrap();
    assert!(js.0.description.contains("Assess each system") && js.0.description.contains("/workflow convert portfolio-assess"));
    let e = roots.resolve("portfolio-assess").unwrap_err();
    assert!(e.contains("JavaScript") && e.contains("/workflow convert portfolio-assess"), "{e}");
    let prompt = marlowe_workflow::convert::conversion_prompt("portfolio-assess", "a.js", ".marlowe/workflows/portfolio-assess.star");
    assert!(prompt.contains("validate_only:true") && prompt.contains("lambda f=f"));
}

#[test]
fn parity_wf9_the_authoring_skill_and_persona_examples_are_valid_scripts() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    for file in ["workflows/workflow-authoring.md", "persona/blocks/workflow.md"] {
        let text = std::fs::read_to_string(root.join(file)).unwrap();
        let blocks: Vec<&str> = text.split("```python").skip(1).map(|b| b.split("```").next().unwrap()).filter(|b| b.contains("def main")).collect();
        assert!(!blocks.is_empty(), "{file} has an example");
        for b in blocks {
            marlowe_workflow::check(file, b).unwrap_or_else(|d| panic!("{file}: {}", marlowe_workflow::validate::render(&d)));
        }
    }
}

#[test]
fn builtins_validate() {
    for (name, src) in saved::BUILTINS {
        marlowe_workflow::check(&format!("{name}.star"), src).unwrap_or_else(|d| panic!("{name}: {}", marlowe_workflow::validate::render(&d)));
    }
}
