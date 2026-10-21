use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use marlowe_agent::{CallCtx, CallFlow, Conversation, FinalFlow, SessionScope, StepInfo, ToolCtx, ToolMiddleware, TurnFlow};
use marlowe_model::{CancelToken, Message, Part, Role, ToolCall};
use marlowe_proto::SessionId;
use serde_json::{json, Value};

use super::*;

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("marlowe-intent-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

const SAID: &str = "Read-only, spawn a workflow to summarize the repo workspace.";

#[test]
fn detection_table() {
    let yes = [
        (SAID, Over::From("folders")),
        ("Use a workflow: find every TODO in the code and group them by folder.", Over::From("folders")),
        ("Run this as a workflow: compare how the services handle errors.", Over::Topic),
        ("Kick off a workflow comparing min-p, top-k and nucleus sampling.", Over::Topic),
        ("Fan out over each crate and report what it does.", Over::From("packages")),
        ("One agent per folder, tell me what each is for.", Over::From("folders")),
        ("Spawn three agents in parallel to review the last commits.", Over::From("commits:5")),
        ("Can you run a workflow over the packages?", Over::From("packages")),
        ("Please start a workflow to review each commit.", Over::From("commits:5")),
    ];
    for (text, over) in yes {
        assert_eq!(detect(text), Some(over), "{text}");
    }
    let no = [
        "what is a workflow?",
        "How do workflows differ from dispatch",
        "cancel the workflow",
        "stop that agent",
        "why did the workflow fail?",
        "Write a workflow script that reviews each crate.",
        "Save this as a .star workflow file.",
        "The workflow finished, thanks.",
        "Do we have a workflow for releases?",
        "Dispatch three agents in parallel.",
        "Call the `workflow` tool with items_from folders.",
        "/code-review the last commit",
        "Explain fan-out to me.",
        "Here is the log:\n```\nstart workflow run x\nspawn workflow a\n```\nWhat went wrong?",
        "An example only: ```run a workflow``` and nothing else.",
        "<system-reminder>The user asked for a workflow.</system-reminder> thanks, that was fine",
    ];
    for text in no {
        assert_eq!(detect(text), None, "{text}");
    }
}

fn convo(ws: &Path) -> Conversation {
    let mut c = Conversation::new(SessionId("s".into()), Some(ws.to_path_buf()));
    c.push(Message::user("earlier question"));
    c.push(Message::assistant("earlier answer"));
    c
}

#[test]
fn the_reminder_rides_only_in_this_turns_user_message() {
    let ws = temp("cache");
    for d in ["ml", "notebooks", "services"] {
        std::fs::create_dir_all(ws.join(d)).unwrap();
    }
    let mut with = convo(&ws);
    let mut without = convo(&ws);
    let before = with.messages.clone();
    let (mut a, mut b) = (SAID.to_string(), "Summarize the repo workspace.".to_string());
    assert_eq!(WorkflowIntent.before_turn(&mut with, &mut a), TurnFlow::Continue);
    assert_eq!(WorkflowIntent.before_turn(&mut without, &mut b), TurnFlow::Continue);
    assert_eq!(a, SAID, "the user's text is not rewritten");
    assert!(without.pending_context.is_empty());
    assert_eq!(with.pending_context.len(), 1);
    assert_eq!(serde_json::to_string(&with.messages).unwrap(), serde_json::to_string(&before).unwrap());
    with.push(Message::user(&a));
    assert_eq!(serde_json::to_string(&with.messages[..before.len()]).unwrap(), serde_json::to_string(&before).unwrap());
    let last = with.messages.last().unwrap();
    assert_eq!(last.role, Role::User);
    assert_eq!(last.content.len(), 2);
    assert!(matches!(&last.content[1], Part::Text { text } if text.contains("Call the `workflow` tool now")));
    assert!(with.messages[..before.len()].iter().all(|m| m.content.iter().all(|p| !matches!(p, Part::Text { text } if text.contains("system-reminder")))));
    assert!(with.pending_context.is_empty());
}

#[test]
fn the_folder_line_is_the_resolvers() {
    let ws = temp("folders");
    for d in ["ml", "notebooks", "services", "target", ".git"] {
        std::fs::create_dir_all(ws.join(d)).unwrap();
    }
    let note = reminder(&Over::From("folders"), Some(&ws));
    let found = items::resolve("folders", &ws).unwrap().items;
    assert_eq!(found, ["ml", "notebooks", "services"]);
    assert!(note.contains(&format!("Top-level folders here: {}.", found.join(", "))), "{note}");
    assert!(note.contains(r#""items_from":"folders""#) && note.contains("not `agent`, not `dispatch`"), "{note}");
    assert!(reminder(&Over::From("commits:5"), Some(&ws)).contains(r#""items_from":"commits:5""#));
    let topic = reminder(&Over::Topic, Some(&ws));
    assert!(topic.contains(r#""items":["<angle 1>""#) && !topic.contains("Top-level folders"), "{topic}");
}

fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall { id: id.into(), name: name.into(), args }
}

/// Runs `before_approval` for each call the way the loop does: the conversation's extensions are
/// lent to the batch, then taken back, then the step ends.
fn batch(convo: &mut Conversation, ws: &Path, calls: Vec<ToolCall>) -> Vec<CallFlow> {
    let scope = SessionScope { ext: Arc::new(Mutex::new(std::mem::take(&mut convo.ext))), ..SessionScope::detached() };
    let cx = CallCtx::new(ToolCtx::new(SessionId("s".into()), 1, Some(ws.to_path_buf()), CancelToken::new(), false), scope.clone());
    let out = calls.into_iter().map(|mut c| WorkflowIntent.before_approval(&mut c, &cx)).collect();
    convo.ext = std::mem::take(&mut *scope.ext.lock().unwrap());
    WorkflowIntent.after_step(StepInfo { turn: 1, step: 1 }, convo);
    out
}

fn reminded(ws: &Path) -> Conversation {
    let mut c = convo(ws);
    WorkflowIntent.before_turn(&mut c, &mut SAID.to_string());
    c
}

fn text(f: &CallFlow) -> Option<String> {
    match f {
        CallFlow::Done(o) => Some(o.content.clone()),
        CallFlow::Continue => None,
    }
}

#[test]
fn the_first_batch_of_dispatches_is_answered_once_then_a_repeat_runs() {
    let ws = temp("redirect");
    let mut c = reminded(&ws);
    let first = batch(
        &mut c,
        &ws,
        vec![
            call("1", "dispatch", json!({"role": "researcher", "prompt": "summarize marlowe-loop"})),
            call("2", "dispatch", json!({"role": "researcher", "prompt": "summarize marlowe-memory"})),
            call("3", "agent", json!({"prompt": "look at the daemon"})),
        ],
    );
    for f in &first {
        let t = text(f).expect("answered");
        assert!(t.starts_with("Not run: the user asked for a workflow") && t.contains("workflow{"), "{t}");
    }
    assert!(text(&first[0]).unwrap().contains("summarize marlowe-loop"));
    assert!(text(&first[1]).unwrap().contains("summarize marlowe-memory"), "each carries its own prompt");
    let again = batch(&mut c, &ws, vec![call("4", "dispatch", json!({"role": "researcher", "prompt": "summarize marlowe-loop"}))]);
    assert_eq!(again[0], CallFlow::Continue, "the repeat runs");
}

#[test]
fn no_redirect_after_a_workflow_call_for_foreground_agents_or_without_the_trigger() {
    let ws = temp("noredirect");
    let mut c = reminded(&ws);
    let out = batch(
        &mut c,
        &ws,
        vec![call("1", "workflow", json!({"prompt": "x {item}", "items_from": "folders"})), call("2", "dispatch", json!({"role": "researcher", "prompt": "p"}))],
    );
    assert_eq!(out, [CallFlow::Continue, CallFlow::Continue]);

    let mut c = reminded(&ws);
    let out = batch(&mut c, &ws, vec![call("1", "agent", json!({"prompt": "p", "background": false}))]);
    assert_eq!(out[0], CallFlow::Continue);

    let mut plain = convo(&ws);
    WorkflowIntent.before_turn(&mut plain, &mut "Summarize the repo workspace.".to_string());
    let out = batch(&mut plain, &ws, vec![call("1", "dispatch", json!({"role": "researcher", "prompt": "p"}))]);
    assert_eq!(out[0], CallFlow::Continue);
    assert!(plain.pending_context.is_empty());
}

fn step() -> StepInfo {
    StepInfo { turn: 1, step: 1 }
}

#[test]
fn the_nudge_needs_no_call_and_no_question_and_comes_once() {
    let ws = temp("nudge");
    let mut c = reminded(&ws);
    let FinalFlow::Continue(note) = WorkflowIntent.before_final(step(), &mut c, "I will look at the workspace and summarize it.") else {
        panic!("nudged")
    };
    assert!(note.contains("workflow{") && note.contains("items_from") && note.contains("Read-only, spawn a workflow"), "{note}");
    assert_eq!(WorkflowIntent.before_final(step(), &mut c, "Still prose."), FinalFlow::Accept, "once");

    let mut c = reminded(&ws);
    assert_eq!(WorkflowIntent.before_final(step(), &mut c, "Which folder do you mean?"), FinalFlow::Accept);

    let mut c = reminded(&ws);
    batch(&mut c, &ws, vec![call("1", "dispatch", json!({"role": "researcher", "prompt": "p"}))]);
    assert_eq!(WorkflowIntent.before_final(step(), &mut c, "Started."), FinalFlow::Accept, "a call ran");

    let mut c = convo(&ws);
    WorkflowIntent.before_turn(&mut c, &mut "Summarize the repo workspace.".to_string());
    assert_eq!(WorkflowIntent.before_final(step(), &mut c, "Here it is."), FinalFlow::Accept, "never reminded");
}

#[test]
fn only_the_conductor_host_installs_it() {
    let (home, ws) = (temp("home"), temp("ws"));
    let setup = crate::specs::HostSetup { home: &home, workspace: Some(&ws) };
    let names = |t: &crate::BuiltinTools| -> Vec<String> { crate::specs::workflow::middleware(t).iter().map(|m| m.name().to_string()).collect() };
    let conductor = crate::specs::workflow::configure(crate::BuiltinTools::new(Some(ws.clone())), &setup);
    assert!(names(&conductor).contains(&"workflow-intent".to_string()));
    let crew = crate::specs::workflow::crew_configure(crate::BuiltinTools::new(Some(ws.clone())), &setup);
    assert!(!names(&crew).contains(&"workflow-intent".to_string()), "{:?}", names(&crew));
}
