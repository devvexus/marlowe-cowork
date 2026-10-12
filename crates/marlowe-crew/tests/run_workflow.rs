//! `Run.workflow` (seam2): a workflow run records its script, a plain run is stored exactly as it
//! was before, and a run stored before workflows existed loads with no workflow.

use std::path::PathBuf;

use marlowe_crew::{Roles, Run, RunStore};
use marlowe_proto::{CrewBudget, Playbook, SessionId, WorkflowRef};
use serde_json::json;

fn run() -> Run {
    let roles = Roles::builtin();
    Run::new(
        Some(SessionId("s1".into())),
        roles.get("engineer").unwrap(),
        "review the diff",
        Some(Playbook::Build),
        Some(PathBuf::from("C:/w")),
        CrewBudget::default(),
    )
}

fn reference() -> WorkflowRef {
    WorkflowRef {
        name: Some("code-review".into()),
        script_path: "C:/h/workflows/runs/r-1/script.star".into(),
        digest: "ab".repeat(32),
        args: json!({"level": "medium"}),
        foreground: false,
        resumed_from: None,
    }
}

#[test]
fn a_workflow_run_keeps_its_script_through_the_store_and_the_wire_view() {
    let store = RunStore::open_in_memory().unwrap();
    let mut r = run();
    assert_eq!(r.workflow, None, "a new run is not a workflow run");
    r.workflow = Some(reference());
    store.save(&r).unwrap();
    let back = store.get(&r.id).unwrap().expect("stored");
    assert_eq!(back, r);
    assert_eq!(back.workflow.as_ref().unwrap().args, json!({"level": "medium"}), "args are kept verbatim");
    assert_eq!(r.info().workflow, r.workflow, "clients see it");
}

#[test]
fn a_plain_run_is_stored_without_the_field_and_an_old_record_loads() {
    let r = run();
    let text = serde_json::to_string(&r).unwrap();
    assert!(!text.contains("workflow"), "a run that is not a workflow is written exactly as before: {text}");
    // A record from before wave 2 is the same JSON; it loads and is not a workflow.
    let old: Run = serde_json::from_str(&text).unwrap();
    assert_eq!(old.workflow, None);
    assert_eq!(old, r);
    // Through the store too: the record column is the run's JSON.
    let store = RunStore::open_in_memory().unwrap();
    store.save(&r).unwrap();
    assert_eq!(store.get(&r.id).unwrap().unwrap().workflow, None);
    assert_eq!(store.list(10).unwrap().len(), 1);
}
