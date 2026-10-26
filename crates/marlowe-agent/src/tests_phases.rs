//! Phase events: every step before the first token is named, compaction reports its progress and
//! its failure, and the provider's first frame closes the wire phase.

use marlowe_proto::{PhaseKind, PhaseState};

use super::*;

/// Sleeps in recall; the provider sleeps before its first frame.
struct SlowMemory(Duration);

impl MemoryHook for SlowMemory {
    fn before_turn(&self, _session: &SessionId, _query: &str) -> Recollection {
        std::thread::sleep(self.0);
        Recollection::default()
    }
    fn after_turn(&self, _session: &SessionId, _user: &str, _assistant: &str) {}
}

struct SlowProvider {
    inner: Arc<ScriptedProvider>,
    before_first_frame: Duration,
}

impl Provider for SlowProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Ollama
    }
    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        self.inner.list_models()
    }
    fn stream(&self, req: &ChatRequest, cancel: &CancelToken, on: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        std::thread::sleep(self.before_first_frame);
        self.inner.stream(req, cancel, on)
    }
}

fn timed_run(a: &Agent, c: &mut Conversation, user: &str) -> (TurnOutcome, Vec<(Instant, AgentEvent)>) {
    let (_tx, rx) = crossbeam_channel::unbounded();
    let mut events = Vec::new();
    let outcome = a.run_turn(c, user, &CancelToken::new(), &rx, &mut |e| events.push((Instant::now(), e)));
    (outcome, events)
}

fn phase_marks(events: &[(Instant, AgentEvent)]) -> Vec<(Instant, PhaseKind, PhaseState, Option<u64>)> {
    events
        .iter()
        .filter_map(|(t, e)| match e {
            AgentEvent::Phase(p) => Some((*t, p.kind, p.state, p.ms)),
            _ => None,
        })
        .collect()
}

#[test]
fn every_step_before_the_first_token_is_a_named_phase_with_no_gap() {
    use PhaseKind::*;
    use PhaseState::*;
    let inner = ScriptedProvider::new(vec![answer("hello")]);
    let provider = Arc::new(SlowProvider { inner, before_first_frame: Duration::from_millis(40) });
    let mut a = Agent::new(provider, model(), TestTools::new(), SystemPrompt::with_core("core"));
    a.memory = Arc::new(SlowMemory(Duration::from_millis(30)));
    let mut c = convo();
    let started = Instant::now();
    let (outcome, events) = timed_run(&a, &mut c, "hi");
    assert_eq!(outcome.stop, TurnStop::Done);

    let marks = phase_marks(&events);
    let order: Vec<(PhaseKind, PhaseState)> = marks.iter().map(|m| (m.1, m.2)).collect();
    assert_eq!(
        order[..10],
        [(Prompt, Start), (Prompt, End), (Recall, Start), (Recall, End), (Prompt, Start), (Prompt, End), (Fit, Start), (Fit, End), (Wire, Start), (Wire, End)]
    );
    // Gaps: from the turn's start to the first Start, and between one phase's end and the next
    // one's start, stay inside a frame budget.
    let frame = Duration::from_millis(16);
    assert!(marks[0].0 - started < frame, "the first phase starts at once");
    for pair in marks[..10].windows(2) {
        let (prev, next) = (&pair[0], &pair[1]);
        if prev.2 == End && next.2 == Start {
            assert!(next.0 - prev.0 < frame, "unnamed gap between {:?} and {:?}: {:?}", prev.1, next.1, next.0 - prev.0);
        }
    }
    // The measured times are real: recall slept 30 ms, the wire waited 40 ms for a first frame.
    let ms = |kind, state| marks.iter().find(|m| m.1 == kind && m.2 == state).and_then(|m| m.3).unwrap();
    assert!((30..60).contains(&ms(Recall, End)), "recall {}", ms(Recall, End));
    assert!((40..80).contains(&ms(Wire, End)), "wire {}", ms(Wire, End));
    // The call is announced before it goes out, and the first token follows the wire phase.
    let at = |f: &dyn Fn(&AgentEvent) -> bool| events.iter().find(|(_, e)| f(e)).map(|(t, _)| *t).unwrap();
    let call_started = at(&|e| matches!(e, AgentEvent::CallStarted { step: 1, .. }));
    let wire_end = marks.iter().find(|m| m.1 == Wire && m.2 == End).unwrap().0;
    let first_token = at(&|e| matches!(e, AgentEvent::FirstToken { .. }));
    let first_delta = at(&|e| matches!(e, AgentEvent::TextDelta(_)));
    assert!(call_started <= wire_end && wire_end <= first_token && first_token <= first_delta);
    assert!(first_delta - first_token < frame, "the token reaches the sink at once");
    let estimate = events.iter().find_map(|(_, e)| match e {
        AgentEvent::CallStarted { est_prompt_tokens, .. } => Some(*est_prompt_tokens),
        _ => None,
    });
    assert!(estimate.unwrap() > 0);
}

fn crowded(first: Step) -> (Agent, Conversation) {
    let p = ScriptedProvider::new(vec![first, answer("fresh answer")]);
    let mut info = model();
    info.context_window = Some(2_000);
    info.max_output = Some(200);
    let a = Agent::new(p, info, TestTools::new(), SystemPrompt::with_core("core"));
    let mut c = convo();
    for i in 0..10 {
        c.push(Message::user(format!("question {i} {}", "q".repeat(600))));
        c.push(Message::assistant(format!("answer {i} {}", "a".repeat(600))));
    }
    (a, c)
}

fn compact_events(events: &[(Instant, AgentEvent)]) -> Vec<&marlowe_proto::PhaseEvent> {
    events
        .iter()
        .filter_map(|(_, e)| match e {
            AgentEvent::Phase(p) if p.kind == PhaseKind::Compact => Some(p),
            _ => None,
        })
        .collect()
}

#[test]
fn compaction_reports_start_progress_and_the_real_sizes() {
    let (a, mut c) = crowded(answer("SUMMARY OF EARLIER"));
    let (_, events) = timed_run(&a, &mut c, "now?");
    let compact = compact_events(&events);
    assert_eq!(compact.first().map(|p| p.state), Some(PhaseState::Start));
    let progress = compact.iter().find(|p| p.state == PhaseState::Progress).expect("progress while the summary streams");
    assert!(progress.est, "the cap is an upper bound, so the progress is an estimate");
    assert!(progress.done <= progress.total);
    let end = compact.last().unwrap();
    assert_eq!(end.state, PhaseState::End);
    assert_ne!(end.detail.as_deref(), Some("failed"));
    assert!(events.iter().any(|(_, e)| matches!(e, AgentEvent::Compacted { .. })));
    // The end carries the prompt's measured size before and after (the compacted one is smaller).
    let (before, after) = (end.done.unwrap(), end.total.unwrap());
    assert!(after < before, "{before} -> {after}");
}

#[test]
fn a_failed_summary_says_so_instead_of_vanishing() {
    let (a, mut c) = crowded(Step::Fail(ProviderError::server("summariser down")));
    let (outcome, events) = timed_run(&a, &mut c, "now?");
    assert_eq!(outcome.text, "fresh answer");
    let compact = compact_events(&events);
    let end = compact.last().expect("the compact phase ends");
    assert_eq!(end.state, PhaseState::End);
    assert_eq!(end.detail.as_deref(), Some("failed"));
    assert!(!events.iter().any(|(_, e)| matches!(e, AgentEvent::Compacted { .. })));
}

/// An engine that says when it sent the request and reports its own timings.
struct Engine {
    inner: Arc<ScriptedProvider>,
    usage: Usage,
}

impl Provider for Engine {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Ollama
    }
    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        self.inner.list_models()
    }
    fn stream(&self, req: &ChatRequest, cancel: &CancelToken, on: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        on(StreamEvent::Sent);
        std::thread::sleep(Duration::from_millis(20));
        let usage = self.usage;
        self.inner.stream(req, cancel, &mut |e| match e {
            // The script has no usage frame; the engine reports its own just before the stop.
            StreamEvent::Stop(s) => {
                on(StreamEvent::Usage(usage));
                on(StreamEvent::Stop(s));
            }
            other => on(other),
        })
    }
}

fn prefill(events: &[(Instant, AgentEvent)]) -> Vec<&marlowe_proto::PhaseEvent> {
    events
        .iter()
        .filter_map(|(_, e)| match e {
            AgentEvent::Phase(p) if p.kind == PhaseKind::Prefill => Some(p),
            _ => None,
        })
        .collect()
}

#[test]
fn the_send_starts_an_estimated_prefill_that_the_engines_counts_end() {
    let usage = Usage { input: 300, output: 5, prompt_eval: Some(300), prefill_ms: Some(150), load_ms: Some(0), decode_ms: Some(40), ..Default::default() };
    let inner = ScriptedProvider::new(vec![answer("hello")]);
    let mut a = Agent::new(Arc::new(Engine { inner, usage }), model(), TestTools::new(), SystemPrompt::with_core("core"));
    a.memory = Arc::new(NoMemory);
    let (_, events) = timed_run(&a, &mut convo(), "hi");
    let marks = phase_marks(&events);
    let pos = |k: PhaseKind, s: PhaseState, staged: bool| {
        events.iter().position(|(_, e)| matches!(e, AgentEvent::Phase(p) if p.kind == k && p.state == s && p.stage.is_some() == staged)).unwrap()
    };
    // Wire opens before the engine is asked, the read starts at the send, the wire ends at the
    // engine's first frame.
    assert!(pos(PhaseKind::Wire, PhaseState::Start, false) < pos(PhaseKind::Prefill, PhaseState::Start, false));
    assert!(pos(PhaseKind::Wire, PhaseState::End, true) < pos(PhaseKind::Prefill, PhaseState::Start, false), "preparation is over at the send");
    assert!(pos(PhaseKind::Prefill, PhaseState::Start, false) < pos(PhaseKind::Wire, PhaseState::End, false));
    let p = prefill(&events);
    assert_eq!(p.len(), 2);
    let est_prompt = events.iter().find_map(|(_, e)| if let AgentEvent::CallStarted { est_prompt_tokens, .. } = e { Some(*est_prompt_tokens) } else { None }).unwrap();
    assert!(p[0].est && p[0].total == Some(u64::from(est_prompt)), "the start carries our estimate: {:?}", p[0]);
    // The end is the engine's own count and time. Ollama's count is the whole prompt, cached part
    // included, so there is no split and no rate here: those come from its runner log or not at all.
    let end = p[1];
    assert_eq!((end.state, end.done, end.total, end.ms), (PhaseState::End, Some(300), Some(300), Some(150)));
    assert_eq!((end.cached, end.rate, end.est), (None, None, false));
    assert!(marks.iter().any(|m| m.1 == PhaseKind::Wire && m.2 == PhaseState::End && m.3.is_some()));
}

/// An engine that reports the read as it goes (llama.cpp) and then the split of the whole prompt.
struct Reading {
    inner: Arc<ScriptedProvider>,
}

impl Provider for Reading {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Ollama
    }
    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        self.inner.list_models()
    }
    fn stream(&self, req: &ChatRequest, cancel: &CancelToken, on: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        on(StreamEvent::Sent);
        on(StreamEvent::ReadProgress { total: 9_000, cached: 6_000, processed: 6_000, ms: 0 });
        on(StreamEvent::ReadProgress { total: 9_000, cached: 6_000, processed: 9_000, ms: 60 });
        let usage = Usage { input: 9_000, cached_input: 6_000, prefill_ms: Some(900), output: 5, ..Default::default() };
        self.inner.stream(req, cancel, &mut |e| match e {
            StreamEvent::Stop(s) => {
                on(StreamEvent::Usage(usage));
                on(StreamEvent::Stop(s));
            }
            other => on(other),
        })
    }
}

#[test]
fn an_engine_that_reports_the_read_gives_exact_progress_and_the_cached_split() {
    let mut a = Agent::new(Arc::new(Reading { inner: ScriptedProvider::new(vec![answer("hello")]) }), model(), TestTools::new(), SystemPrompt::with_core("core"));
    a.memory = Arc::new(NoMemory);
    let (_, events) = timed_run(&a, &mut convo(), "hi");
    let p = prefill(&events);
    assert_eq!(p.len(), 4, "start, two progress, end: {p:?}");
    assert!(p[0].est, "the send still starts with our estimate");
    for (seen, reached) in [(p[1], 6_000), (p[2], 9_000)] {
        assert_eq!((seen.state, seen.done, seen.total, seen.cached, seen.est), (PhaseState::Progress, Some(reached), Some(9_000), Some(6_000), false));
    }
    let end = p[3];
    assert_eq!((end.state, end.done, end.total, end.cached, end.ms, end.est), (PhaseState::End, Some(3_000), Some(9_000), Some(6_000), Some(900), false));
    // The server's clock gives the rate: 3,000 fresh tokens in 60 ms. Before it has a clock there is none.
    assert_eq!((p[1].rate, p[2].rate), (None, Some(50_000.0)));
}

#[test]
fn a_provider_that_cannot_say_when_it_sent_has_no_prefill_phase() {
    let a = Agent::new(ScriptedProvider::new(vec![answer("hello")]), model(), TestTools::new(), SystemPrompt::with_core("core"));
    let (_, events) = timed_run(&a, &mut convo(), "hi");
    assert!(prefill(&events).is_empty());
}

#[test]
fn a_call_that_fails_after_the_send_still_closes_the_prefill_phase() {
    let inner = ScriptedProvider::new(vec![Step::Fail(ProviderError::bad_request("engine fell over"))]);
    let a = Agent::new(Arc::new(Engine { inner, usage: Usage::default() }), model(), TestTools::new(), SystemPrompt::with_core("core"));
    let (outcome, events) = timed_run(&a, &mut convo(), "hi");
    assert_eq!(outcome.stop, TurnStop::Error);
    let p = prefill(&events);
    assert_eq!(p.iter().map(|p| p.state).collect::<Vec<_>>(), [PhaseState::Start, PhaseState::End]);
    assert_eq!(p[1].detail.as_deref(), Some("no report"));
}
