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
