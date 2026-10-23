//! The prompt cache never breaks: what the model produced goes back to it exactly, and nothing
//! it has read is rewritten except by a compaction. Each test is one cause the live audit found.

use marlowe_model::ToolCall;

use super::*;

fn thought(t: &str) -> StreamEvent {
    StreamEvent::ReasoningDelta { text: t.into(), tokens: 3 }
}

fn assistant_calls(m: &Message) -> Vec<ToolCall> {
    m.tool_calls().cloned().collect()
}

#[test]
fn tool_call_arguments_go_back_in_the_order_the_model_wrote_them() {
    let args = json!({"text": "hi", "zebra": {"y": 1, "x": [true, 2.50, "q\"uote\nline"]}, "alpha": null});
    let mut first = vec![thought("plan")];
    first.extend(call(0, "a", "echo", args.clone()));
    first.push(StreamEvent::Stop(StopReason::ToolUse));
    let p = ScriptedProvider::new(vec![Step::Events(first), answer("done")]);
    let a = agent(p.clone(), TestTools::new());
    let mut c = convo();
    run(&a, &mut c, "go");
    let kept = assistant_calls(&c.messages[1]);
    assert_eq!(serde_json::to_string(&kept[0].args).unwrap(), args.to_string(), "keys, nested keys and values as written");
    // The request after it carries the same bytes.
    let sent = p.requests();
    let again = assistant_calls(&sent[1].messages[1]);
    assert_eq!(serde_json::to_string(&again[0].args).unwrap(), args.to_string());
}

#[test]
fn the_history_keeps_the_call_as_written_while_the_loop_runs_the_canonical_one() {
    let mut first = call(0, "a", "Echo", json!({"text": "hi"}));
    first.push(StreamEvent::Stop(StopReason::ToolUse));
    let p = ScriptedProvider::new(vec![Step::Events(first), answer("done")]);
    let tools = TestTools::new();
    let a = agent(p, tools.clone());
    let mut c = convo();
    run(&a, &mut c, "go");
    assert_eq!(tools.ran("echo"), 1, "the canonical name ran");
    assert_eq!(assistant_calls(&c.messages[1])[0].name, "Echo", "the model's own spelling stays in the history");
}

#[test]
fn an_interrupt_keeps_reasoning_that_had_no_answer_yet() {
    let p = ScriptedProvider::new(vec![Step::UntilCancel(vec![thought("a long line of thought")])]);
    let a = agent(p, TestTools::new());
    let mut c = convo();
    let cancel = CancelToken::new();
    let canceller = cancel.clone();
    let (_tx, rx) = crossbeam_channel::unbounded();
    let out = a.run_turn(&mut c, "write a lot", &cancel, &rx, &mut |e| {
        if matches!(e, AgentEvent::ReasoningDelta { .. }) {
            canceller.cancel();
        }
    });
    assert_eq!(out.stop, TurnStop::Interrupted);
    let kept = c.messages.last().unwrap();
    assert_eq!(kept.role, Role::Assistant, "the thought is in the history, not dropped");
    assert_eq!(kept.thinking().unwrap().text, "a long line of thought");
    assert!(kept.joined_text().is_empty());
}

#[test]
fn an_interrupt_keeps_reasoning_and_the_half_answer_together() {
    let p = ScriptedProvider::new(vec![Step::UntilCancel(vec![thought("thinking, "), thought("still"), text("half an ans")])]);
    let a = agent(p.clone(), TestTools::new());
    let mut c = convo();
    let cancel = CancelToken::new();
    let canceller = cancel.clone();
    let (_tx, rx) = crossbeam_channel::unbounded();
    a.run_turn(&mut c, "write a lot", &cancel, &rx, &mut |e| {
        if matches!(e, AgentEvent::TextDelta(_)) {
            canceller.cancel();
        }
    });
    let kept = c.messages.last().unwrap().clone();
    assert_eq!(kept.thinking().unwrap().text, "thinking, still");
    assert_eq!(kept.joined_text(), "half an ans");
    // The next turn's request starts with exactly that.
    let p2 = ScriptedProvider::new(vec![answer("ok")]);
    let a2 = agent(p2.clone(), TestTools::new());
    run(&a2, &mut c, "go on");
    let sent = p2.requests();
    assert_eq!(sent[0].messages[1], kept);
    assert_eq!(sent[0].messages[2], Message::user("go on"));
}

#[test]
fn a_call_the_model_finished_writing_before_the_interrupt_stays_answered_as_not_run() {
    let mut evs = vec![thought("plan")];
    evs.extend(call(0, "a", "echo", json!({"text": "hi"})));
    evs.push(StreamEvent::Stop(StopReason::Cancelled));
    let p = ScriptedProvider::new(vec![Step::Events(evs)]);
    let tools = TestTools::new();
    let a = agent(p, tools.clone());
    let mut c = convo();
    let r = run(&a, &mut c, "go");
    assert_eq!(r.outcome.stop, TurnStop::Interrupted);
    assert_eq!(tools.ran("echo"), 0, "nothing ran");
    assert_eq!(assistant_calls(&c.messages[1])[0].name, "echo", "the call is in the history");
    let results = tool_results(&c.messages[2]);
    assert_eq!(results.len(), 1);
    assert!(results[0].is_error && results[0].content.starts_with("Not run"), "{}", results[0].content);
}

#[test]
fn a_reply_cut_for_repeating_keeps_everything_the_model_generated() {
    let said = "I will check the file now.\n".repeat(6);
    let p = ScriptedProvider::new(vec![Step::UntilCancel(vec![text(&said)]), answer("done")]);
    let a = agent(p.clone(), TestTools::new());
    let mut c = convo();
    let r = run(&a, &mut c, "go");
    assert_eq!(r.outcome.stop, TurnStop::Done);
    let kept = c.messages[1].joined_text();
    assert_eq!(kept, said, "the cut copy is reported, but the history holds what was generated");
    assert!(r.outcome.text.len() < said.len() + "done".len(), "the turn reports one copy");
}

#[test]
fn a_reply_of_reasoning_alone_is_kept() {
    let p = ScriptedProvider::new(vec![Step::Events(vec![thought("only thinking"), StreamEvent::Stop(StopReason::EndTurn)])]);
    let a = agent(p, TestTools::new());
    let mut c = convo();
    run(&a, &mut c, "go");
    assert_eq!(c.messages[1].thinking().unwrap().text, "only thinking");
}

/// A host whose tool list grows once `late` is set.
struct Growing {
    inner: Arc<TestTools>,
    late: AtomicUsize,
}

impl ToolHost for Growing {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.inner.specs();
        if self.late.load(Ordering::SeqCst) > 0 {
            specs.insert(1, ToolSpec { name: "mcp__late__ping".into(), description: "ping".into(), input_schema: json!({"type": "object"}) });
        }
        specs
    }
    fn call(&self, call: &ToolCallReq, ctx: &ToolCtx) -> ToolOutcome {
        self.inner.call(call, ctx)
    }
    fn consequence(&self, call: &ToolCallReq) -> Consequence {
        self.inner.consequence(call)
    }
}

#[test]
fn a_tool_that_appears_late_joins_the_list_at_the_next_compaction_and_is_announced_on_the_newest_message() {
    let host = Arc::new(Growing { inner: TestTools::new(), late: AtomicUsize::new(0) });
    let p = ScriptedProvider::new(vec![answer("one"), answer("two"), answer("three")]);
    let a = Agent::new(p.clone(), model(), host.clone(), SystemPrompt::with_core("core"));
    let mut c = convo();
    run(&a, &mut c, "first");
    host.late.store(1, Ordering::SeqCst);
    run(&a, &mut c, "second");
    run(&a, &mut c, "third");
    let sent = p.requests();
    let names = |r: &ChatRequest| r.tools.iter().map(|t| t.name.clone()).collect::<Vec<_>>();
    assert_eq!(names(&sent[1]), names(&sent[0]), "the list did not change");
    assert_eq!(names(&sent[2]), names(&sent[0]));
    let second_user = sent[1].messages.last().unwrap().joined_text();
    assert!(second_user.contains("mcp__late__ping") && second_user.ends_with("second"), "{second_user}");
    assert!(!sent[2].messages.last().unwrap().joined_text().contains("mcp__late__ping"), "told once");
    // The earlier messages are untouched by the announcement.
    assert_eq!(sent[2].messages[0], sent[1].messages[0]);
}

#[test]
fn a_conversation_well_inside_its_window_is_never_rewritten() {
    // About 82% of the window by the estimate: the old trigger (70%) elided old tool results
    // here; nothing may change until the window is nearly full.
    let p = ScriptedProvider::new(vec![answer("fine")]);
    let a = agent(p.clone(), TestTools::new());
    let mut c = convo();
    for i in 0..35 {
        let mut m = call(0, &format!("c{i}"), "echo", json!({"text": "x"}));
        m.push(StreamEvent::Stop(StopReason::ToolUse));
        c.push(Message::user(format!("go {i}")));
        c.push(fold_events(&m).to_message());
        c.push(Message::tool_results([marlowe_model::ToolResult::text(format!("c{i}"), "r".repeat(2_000), false)]));
    }
    let before = c.messages.clone();
    let r = run(&a, &mut c, "now");
    assert!(!r.events.iter().any(|e| matches!(e, AgentEvent::Compacted { .. })), "no compaction");
    assert_eq!(&c.messages[..before.len()], &before[..], "no tool result was masked");
    let sent = p.requests();
    assert!(!sent[0].messages.iter().any(|m| m.tool_results_iter().any(|r| r.content == context::MASKED_RESULT)));
}

#[test]
fn the_estimate_is_corrected_by_the_last_real_prompt_size() {
    let mut c = convo();
    assert_eq!(context::scaled_tokens(1_000, &c), 1_000);
    c.ext.insert(context::TokenScale::learned(600, 1_000).unwrap());
    assert_eq!(context::scaled_tokens(1_000, &c), 600);
    assert!(context::TokenScale::learned(0, 1_000).is_none());
}

#[test]
fn the_summary_call_shrinks_its_transcript_until_it_fits_and_is_sized_by_the_real_token_rate() {
    let overflow = ProviderError::bad_request("context_overflow: the prompt does not fit");
    let p = ScriptedProvider::new(vec![Step::Fail(overflow), answer("SUMMARY"), answer("fresh")]);
    let mut info = model();
    info.context_window = Some(2_000);
    info.max_output = Some(200);
    let a = Agent::new(p.clone(), info, TestTools::new(), SystemPrompt::with_core("core"));
    let mut c = convo();
    for i in 0..10 {
        c.push(Message::user(format!("question {i} {}", "q".repeat(600))));
        c.push(Message::assistant(format!("answer {i} {}", "a".repeat(600))));
    }
    // Dense text: twice as many tokens as estimated, so the transcript is cut to half.
    c.ext.insert(context::TokenScale(2.0));
    let r = run(&a, &mut c, "now?");
    assert_eq!(c.summary.as_deref(), Some("SUMMARY"), "{:?}", r.outcome);
    let reqs = p.requests();
    assert_eq!(reqs.len(), 3, "overflow, retry, then the turn");
    let (first, second) = (reqs[0].messages[0].joined_text(), reqs[1].messages[0].joined_text());
    assert!(second.len() < first.len(), "the retry sends less: {} then {}", first.len(), second.len());
}
