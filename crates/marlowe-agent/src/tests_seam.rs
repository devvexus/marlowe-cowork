//! The seam's extension points: middleware around turns, calls and steps, observers,
//! `Conversation::ext` lent to calls, daemon services, host-owned middleware.

use std::path::PathBuf;

use marlowe_model::ToolCall;

use super::*;

/// TestTools, plus a mutation report for `echo` (as if it wrote `echo.txt`).
struct Mutating(Arc<TestTools>);

impl ToolHost for Mutating {
    fn specs(&self) -> Vec<ToolSpec> {
        self.0.specs()
    }
    fn call(&self, call: &ToolCallReq, ctx: &ToolCtx) -> ToolOutcome {
        self.0.call(call, ctx)
    }
    fn consequence(&self, call: &ToolCallReq) -> Consequence {
        self.0.consequence(call)
    }
    fn mutation(&self, call: &ToolCallReq, _ctx: &ToolCtx) -> Mutation {
        if call.name == "echo" {
            Mutation::Paths(vec![PathBuf::from("echo.txt")])
        } else {
            Mutation::None
        }
    }
}

#[derive(Default)]
struct Recorder {
    log: Mutex<Vec<String>>,
    /// `before_final` asks for one more step this many times.
    continue_finals: AtomicUsize,
}

impl Recorder {
    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
    fn push(&self, s: String) {
        self.log.lock().unwrap().push(s);
    }
}

impl ToolMiddleware for Recorder {
    fn name(&self) -> &str {
        "recorder"
    }
    fn before_approval(&self, call: &mut ToolCall, _cx: &CallCtx) -> CallFlow {
        self.push(format!("approve {}", call.name));
        if call.name == "echo" {
            call.args = json!({"text": "rewritten"});
            // Ignored: only the arguments may change.
            call.name = "fail".into();
        }
        CallFlow::Continue
    }
    fn before_call(&self, call: &ToolCall, _cx: &CallCtx) -> CallFlow {
        self.push(format!("before {}", call.name));
        match call.name.as_str() {
            "fail" => CallFlow::Done(ToolOutcome::error("blocked by middleware")),
            _ => CallFlow::Continue,
        }
    }
    fn pre_mutation(&self, call: &ToolCall, _cx: &CallCtx, mutation: &Mutation) -> Result<(), ToolOutcome> {
        self.push(format!("pre_mutation {} {mutation:?}", call.name));
        Ok(())
    }
    fn after_call(&self, call: &ToolCall, _cx: &CallCtx, outcome: &mut ToolOutcome) {
        self.push(format!("after {}", call.name));
        outcome.content.push_str(" +diag");
    }
    fn after_step(&self, info: StepInfo, convo: &mut Conversation) -> Vec<Message> {
        self.push(format!("after_step {}", info.step));
        *convo.ext.get_or_default::<u32>() += 1;
        vec![Message::user("<reminder>job 1 exited</reminder>")]
    }
    fn before_final(&self, _info: StepInfo, _convo: &mut Conversation, text: &str) -> FinalFlow {
        self.push(format!("final {text}"));
        let left = self.continue_finals.load(Ordering::SeqCst);
        if left > 0 {
            self.continue_finals.store(left - 1, Ordering::SeqCst);
            FinalFlow::Continue("You have open todos.".into())
        } else {
            FinalFlow::Accept
        }
    }
}

#[derive(Default)]
struct Watcher {
    calls: Mutex<Vec<(u32, usize)>>,
    batches: Mutex<Vec<(u32, usize)>>,
}

impl TurnObserver for Watcher {
    fn on_model_call(&self, _convo: &Conversation, call: &ModelCall<'_>) {
        self.calls.lock().unwrap().push((call.info.step, call.completion.tool_calls.len()));
    }
    fn on_tools_done(&self, _convo: &Conversation, info: StepInfo, calls: &[ToolCall], results: &[marlowe_model::ToolResult]) {
        assert_eq!(calls.len(), results.len());
        self.batches.lock().unwrap().push((info.step, results.len()));
    }
}

#[test]
fn middleware_wraps_calls_and_steps_and_observers_see_everything() {
    let mut first = call(0, "a", "echo", json!({"text": "hi"}));
    first.extend(call(1, "b", "fail", json!({})));
    first.push(StreamEvent::Stop(StopReason::ToolUse));
    let p = ScriptedProvider::new(vec![Step::Events(first), answer("done"), answer("really done")]);
    let tools = TestTools::new();
    let mut a = Agent::new(p.clone(), model(), Arc::new(Mutating(tools.clone())), SystemPrompt::with_core("core"));
    let rec = Arc::new(Recorder { continue_finals: AtomicUsize::new(1), ..Recorder::default() });
    let watch = Arc::new(Watcher::default());
    a.middleware.push(rec.clone());
    a.observers.push(watch.clone());
    let mut c = convo();
    let r = run(&a, &mut c, "go");

    assert_eq!(r.outcome.stop, TurnStop::Done);
    assert_eq!(r.outcome.steps, 3, "before_final kept the turn going for one more step");
    // `fail` never reached the host; `echo` ran with rewritten arguments under its own name.
    assert_eq!(tools.ran("fail"), 0);
    assert_eq!(tools.ran("echo"), 1);
    let results = tool_results(&c.messages[2]);
    assert_eq!(results[0].content, "rewritten +diag");
    assert_eq!(results[1].content, "blocked by middleware");
    assert!(results[1].is_error);
    let log = rec.log();
    assert_eq!(&log[..2], ["approve echo", "approve fail"], "before_approval runs first, in call order: {log:?}");
    assert!(log.contains(&"pre_mutation echo Paths([\"echo.txt\"])".to_string()), "{log:?}");
    assert!(!log.iter().any(|l| l == "after fail"), "a short-circuited call gets no after_call: {log:?}");
    assert!(log.contains(&"after_step 1".to_string()));
    assert_eq!(log.iter().filter(|l| l.starts_with("final")).count(), 2);
    // The after_step reminder and the before_final reminder both reached the next request.
    let reqs = p.requests();
    let second: String = reqs[1].messages.iter().map(|m| m.joined_text()).collect();
    assert!(second.contains("job 1 exited"), "{second}");
    let third: String = reqs[2].messages.iter().map(|m| m.joined_text()).collect();
    assert!(third.contains("You have open todos."), "{third}");
    assert_eq!(*c.ext.get::<u32>().unwrap(), 1);

    assert_eq!(*watch.calls.lock().unwrap(), vec![(1, 2), (2, 0), (3, 0)]);
    assert_eq!(*watch.batches.lock().unwrap(), vec![(1, 2)]);
}

#[test]
fn no_middleware_means_the_call_is_untouched() {
    let mut first = call(0, "a", "echo", json!({"text": "hi"}));
    first.push(StreamEvent::Stop(StopReason::ToolUse));
    let p = ScriptedProvider::new(vec![Step::Events(first), answer("ok")]);
    let a = Agent::new(p, model(), Arc::new(Mutating(TestTools::new())), SystemPrompt::with_core("core"));
    let mut c = convo();
    run(&a, &mut c, "go");
    assert_eq!(tool_results(&c.messages[2])[0].content, "hi");
}

/// `read` is free and clean inside `/ws`; outside it asks and taints.
struct PathTools {
    ran: Mutex<Vec<String>>,
}

impl PathTools {
    fn outside(call: &ToolCallReq) -> bool {
        !call.args["path"].as_str().unwrap_or("").starts_with("/ws/")
    }
}

impl ToolHost for PathTools {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec { name: "read".into(), description: "read".into(), input_schema: json!({"type": "object"}) }]
    }
    fn call(&self, call: &ToolCallReq, _ctx: &ToolCtx) -> ToolOutcome {
        let path = call.args["path"].as_str().unwrap_or("").to_string();
        self.ran.lock().unwrap().push(path.clone());
        ToolOutcome::ok(format!("contents of {path}"), "1 line")
    }
    fn consequence(&self, call: &ToolCallReq) -> Consequence {
        if Self::outside(call) {
            Consequence::Ask { summary: "read outside the workspace".into() }
        } else {
            Consequence::Free
        }
    }
    fn taints(&self, call: &ToolCallReq) -> bool {
        Self::outside(call)
    }
}

/// Rewrites every `read` to `path`.
struct Redirect(&'static str);

impl ToolMiddleware for Redirect {
    fn name(&self) -> &str {
        "redirect"
    }
    fn before_approval(&self, call: &mut ToolCall, _cx: &CallCtx) -> CallFlow {
        call.args = json!({"path": self.0});
        CallFlow::Continue
    }
}

fn read_turn(gate: Arc<RecordingGate>, redirect: &'static str) -> (Arc<PathTools>, Conversation) {
    let mut first = call(0, "a", "read", json!({"path": "/ws/a.txt"}));
    first.push(StreamEvent::Stop(StopReason::ToolUse));
    let p = ScriptedProvider::new(vec![Step::Events(first), answer("ok")]);
    let tools = Arc::new(PathTools { ran: Mutex::new(Vec::new()) });
    let mut a = Agent::new(p, model(), tools.clone(), SystemPrompt::with_core("core"));
    a.approvals = gate;
    a.middleware.push(Arc::new(Redirect(redirect)));
    let mut c = convo();
    run(&a, &mut c, "go");
    (tools, c)
}

#[test]
fn a_rewrite_is_judged_by_the_gate_and_taints_as_the_call_that_runs() {
    // A free in-workspace read rewritten to an outside path: the gate is asked about the rewritten
    // call, and once allowed it runs as rewritten and taints the conversation.
    let gate = RecordingGate::new(ApprovalDecision::Allow);
    let (tools, c) = read_turn(gate.clone(), "/etc/secrets");
    {
        let asked = gate.asked.lock().unwrap();
        assert_eq!(asked.len(), 1, "the rewritten call needs approval");
        assert!(asked[0].detail.contains("/etc/secrets"), "{}", asked[0].detail);
    }
    assert_eq!(*tools.ran.lock().unwrap(), vec!["/etc/secrets".to_string()]);
    assert!(c.exposure.tainted, "the outside read that ran taints, though the model asked for a clean one");

    // Denied: nothing runs, nothing taints.
    let gate = RecordingGate::new(ApprovalDecision::Deny);
    let (tools, c) = read_turn(gate, "/etc/secrets");
    assert!(tools.ran.lock().unwrap().is_empty());
    assert!(!c.exposure.tainted);

    // Rewritten to another clean path: free, not asked.
    let gate = RecordingGate::new(ApprovalDecision::Deny);
    let (tools, c) = read_turn(gate.clone(), "/ws/b.txt");
    assert!(gate.asked.lock().unwrap().is_empty());
    assert_eq!(*tools.ran.lock().unwrap(), vec!["/ws/b.txt".to_string()]);
    assert!(!c.exposure.tainted);
}

/// Blocks prompts containing `secret`, and tags the rest.
struct PromptGuard;

impl ToolMiddleware for PromptGuard {
    fn name(&self) -> &str {
        "prompt-guard"
    }
    fn before_turn(&self, _convo: &mut Conversation, text: &mut String) -> TurnFlow {
        if text.contains("secret") {
            return TurnFlow::Block("prompts may not mention secrets".into());
        }
        text.push_str("\n[context: branch main]");
        TurnFlow::Continue
    }
}

#[test]
fn before_turn_rewrites_the_prompt_or_blocks_the_turn() {
    let p = ScriptedProvider::new(vec![answer("ok")]);
    let mut a = Agent::new(p.clone(), model(), TestTools::new(), SystemPrompt::with_core("core"));
    a.middleware.push(Arc::new(PromptGuard));
    let mut c = convo();

    let blocked = run(&a, &mut c, "print the secret");
    assert!(blocked.outcome.blocked);
    assert_eq!(blocked.outcome.steps, 0);
    assert_eq!(blocked.outcome.error.as_deref(), Some("prompts may not mention secrets"));
    assert!(c.messages.is_empty(), "a blocked prompt is not kept");
    assert_eq!(c.next_turn, 0);
    assert!(p.requests().is_empty(), "no model call");

    let ok = run(&a, &mut c, "hello");
    assert!(!ok.outcome.blocked);
    assert_eq!(c.messages[0].joined_text(), "hello\n[context: branch main]");
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Todos(Vec<String>);

/// A daemon service, as F's `ask_user` channel would be.
struct Greeting(&'static str);

/// Answers through `call_scoped`: reads a service, appends to the session's todos.
struct Scoped;

impl ToolHost for Scoped {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec { name: "todo".into(), description: "todo".into(), input_schema: json!({"type": "object"}) }]
    }
    fn call(&self, _call: &ToolCallReq, _ctx: &ToolCtx) -> ToolOutcome {
        ToolOutcome::error("needs a session")
    }
    fn call_scoped(&self, call: &ToolCallReq, cx: &CallCtx) -> ToolOutcome {
        let greeting = cx.scope.services.get::<Greeting>().map_or("none", |g| g.0);
        let item = call.args["item"].as_str().unwrap_or("").to_string();
        let n = cx.scope.with_ext(|e| {
            let todos = e.get_or_default::<Todos>();
            todos.0.push(item);
            todos.0.len()
        });
        ToolOutcome::ok(format!("{greeting}: {n} todos in {}", cx.session), "ok")
    }
    fn consequence(&self, _call: &ToolCallReq) -> Consequence {
        Consequence::Free
    }
    fn middleware(&self) -> Vec<Arc<dyn ToolMiddleware>> {
        vec![Arc::new(PromptGuard)]
    }
}

#[test]
fn calls_reach_the_session_state_and_services_and_hosts_bring_their_middleware() {
    let mut first = call(0, "a", "todo", json!({"item": "write tests"}));
    first.extend(call(1, "b", "todo", json!({"item": "run them"})));
    first.push(StreamEvent::Stop(StopReason::ToolUse));
    let p = ScriptedProvider::new(vec![Step::Events(first), answer("ok")]);
    let mut a = Agent::new(p, model(), Arc::new(Scoped), SystemPrompt::with_core("core"));
    assert_eq!(a.middleware.len(), 1, "Agent::new seeds the host's own middleware");
    a.services = Services::new().with(Greeting("hello"));
    let mut c = convo();
    c.ext.insert(Todos(vec!["read the spec".into()]));
    run(&a, &mut c, "plan");
    let results = tool_results(&c.messages[2]);
    let mut counts: Vec<String> = results.iter().map(|r| r.content.clone()).collect();
    counts.sort();
    assert_eq!(counts, vec!["hello: 2 todos in s1", "hello: 3 todos in s1"], "parallel calls share the lent state");
    assert_eq!(c.ext.get::<Todos>().map(|t| t.0.len()), Some(3), "the extensions come back after the batch");
    assert!(c.messages[0].joined_text().ends_with("[context: branch main]"), "the host's middleware ran");

    // A plain `call` has no session; `call_scoped` with a detached scope has an empty one.
    let ctx = ToolCtx::new(SessionId::from("s"), 0, None, CancelToken::new(), false);
    let todo = ToolCall { id: "x".into(), name: "todo".into(), args: json!({}) };
    assert!(Scoped.call(&todo, &ctx).is_error);
    assert_eq!(Scoped.call_scoped(&todo, &CallCtx::detached(ctx)).content, "none: 1 todos in s");
}

#[test]
fn extensions_hold_one_value_per_type_and_clone_with_the_conversation() {
    let mut c = convo();
    assert!(c.ext.get::<Todos>().is_none());
    c.ext.get_or_default::<Todos>().0.push("write tests".into());
    assert_eq!(c.ext.insert(Todos(vec!["a".into()])), Some(Todos(vec!["write tests".into()])));
    c.ext.insert(7u64);
    let copy = c.clone();
    c.ext.get_mut::<Todos>().unwrap().0.push("b".into());
    assert_eq!(copy.ext.get::<Todos>(), Some(&Todos(vec!["a".into()])), "a clone is independent");
    assert_eq!(c.ext.get::<u64>(), Some(&7));
    assert_eq!(c.ext.len(), 2);
    assert!(c.ext.remove::<u64>());
    assert!(format!("{:?}", c.ext).contains("Todos"));
}

#[test]
fn tool_persona_blocks_append_once_and_render_their_text() {
    let b = PersonaBlock::Tool { group: ToolGroup::Git, text: "Use the git tool for history." };
    let t = SystemPrompt::with_core("core").blocks_from([b, b]).render_text();
    assert_eq!(t.matches("Use the git tool").count(), 1, "{t}");
    assert_eq!(SystemPrompt::with_core("core").blocks().len(), 0);
}

/// A file host for the integration fixes: `edit` replaces `old` with `new` in one shared text
/// (it fails when `old` is absent, so order matters), `write` stores `path` and asks every time
/// for a secret-shaped or off-machine path (`AskEvery`) and ordinarily otherwise.
#[derive(Default)]
struct Files {
    text: Mutex<String>,
    written: Mutex<Vec<String>>,
}

impl ToolHost for Files {
    fn specs(&self) -> Vec<ToolSpec> {
        ["edit", "write", "echo"]
            .iter()
            .map(|n| ToolSpec { name: n.to_string(), description: format!("{n} tool"), input_schema: json!({"type": "object"}) })
            .collect()
    }
    fn call(&self, call: &ToolCallReq, _ctx: &ToolCtx) -> ToolOutcome {
        match call.name.as_str() {
            "edit" => {
                let (old, new) = (call.args["old"].as_str().unwrap_or(""), call.args["new"].as_str().unwrap_or(""));
                // Widen the window for a race: a parallel second edit would run inside it.
                std::thread::sleep(Duration::from_millis(1));
                let mut t = self.text.lock().unwrap();
                if !t.contains(old) {
                    return ToolOutcome::error(format!("{old} not found"));
                }
                *t = t.replacen(old, new, 1);
                ToolOutcome::ok("edited", "edited")
            }
            "write" => {
                self.written.lock().unwrap().push(call.args["path"].as_str().unwrap_or("").to_string());
                ToolOutcome::ok("written", "written")
            }
            _ => ToolOutcome::ok("echo", "echo"),
        }
    }
    fn consequence(&self, call: &ToolCallReq) -> Consequence {
        let path = call.args["path"].as_str().unwrap_or("");
        match call.name.as_str() {
            "write" if path == ".env" || path.starts_with(r"\\") => Consequence::AskEvery { summary: format!("write {path}") },
            "write" => Consequence::Ask { summary: format!("write {path}") },
            _ => Consequence::Free,
        }
    }
    fn manifest(&self, name: &str) -> Option<ToolManifest> {
        let effects = match name {
            "edit" | "write" => Effects::WRITE,
            _ => Effects::READ,
        };
        Some(ToolManifest { group: ToolGroup::Files, profiles: Profiles::ALL, effects, path_args: &["path"] })
    }
}

/// Track A's review finding 4: two edits of one file in one message apply in call order (the
/// second edit's `old` exists only after the first), every time.
#[test]
fn free_writes_in_one_message_run_in_call_order() {
    for round in 0..50 {
        let mut first = call(0, "a", "edit", json!({"old": "one", "new": "two"}));
        first.extend(call(1, "b", "edit", json!({"old": "two", "new": "three"})));
        first.extend(call(2, "c", "echo", json!({})));
        first.push(StreamEvent::Stop(StopReason::ToolUse));
        let p = ScriptedProvider::new(vec![Step::Events(first), answer("ok")]);
        let files = Arc::new(Files { text: Mutex::new("one".into()), ..Files::default() });
        let a = Agent::new(p, model(), files.clone(), SystemPrompt::with_core("core"));
        let mut c = convo();
        run(&a, &mut c, "edit");
        let results = tool_results(&c.messages[2]);
        assert!(results.iter().all(|r| !r.is_error), "round {round}: {results:?}");
        assert_eq!(*files.text.lock().unwrap(), "three", "round {round}");
    }
}

/// Track A's review finding 1: a session allow for `write` frees later ordinary writes, but a
/// secret-shaped or off-machine write (`AskEvery`) still reaches the gate, in the same batch and
/// in later ones.
#[test]
fn a_session_allow_never_frees_a_fixed_ask() {
    let batch = |id: &str| {
        let mut ev = call(0, &format!("{id}0"), "write", json!({"path": "src/a.rs"}));
        ev.extend(call(1, &format!("{id}1"), "write", json!({"path": ".env"})));
        ev.extend(call(2, &format!("{id}2"), "write", json!({"path": r"\\host\share\x"})));
        ev.extend(call(3, &format!("{id}3"), "write", json!({"path": "src/b.rs"})));
        ev.push(StreamEvent::Stop(StopReason::ToolUse));
        Step::Events(ev)
    };
    let p = ScriptedProvider::new(vec![batch("a"), batch("b"), answer("ok")]);
    let files = Arc::new(Files::default());
    let gate = RecordingGate::new(ApprovalDecision::AllowForSession);
    let mut a = Agent::new(p, model(), files.clone(), SystemPrompt::with_core("core"));
    a.approvals = gate.clone();
    let mut c = convo();
    run(&a, &mut c, "write");
    let asked: Vec<String> = gate.asked.lock().unwrap().iter().map(|r| r.summary.clone()).collect();
    // Batch one: the first ordinary write asks (and is allowed for the session), src/b.rs is then
    // freed, the two fixed asks ask. Batch two: only the fixed asks ask.
    assert_eq!(asked, ["write src/a.rs", "write .env", r"write \\host\share\x", "write .env", r"write \\host\share\x"]);
    assert_eq!(files.written.lock().unwrap().len(), 8, "every allowed write ran");
}
