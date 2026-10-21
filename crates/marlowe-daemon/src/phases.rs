//! A turn's phase clock: every named step between Enter and the first token, as events for the
//! client and as measured spans for the turn's summary and the database.
//!
//! The clock's zero is Enter: the client's own stamp when the `Ask` carried one, else the daemon
//! receiving it, else the runner starting the turn. `Start` and `End` publish in order (replayed);
//! `Progress` is ephemeral. Spans are only what was measured, never an estimate.

use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;

use marlowe_agent::AgentEvent;
use marlowe_brains::LoadCost;
use marlowe_mind::{LiveEvent, LiveKind, MindEvent, MindSink};
use marlowe_proto::{
    Event, PhaseEvent, PhaseKind, PhaseSpan, PhaseState, ReadStat, SessionId, TurnNumbers,
};

use crate::daemon::Core;
use crate::mind::RecallSink;
use crate::lock;

#[derive(Default)]
struct Inner {
    spans: Vec<PhaseSpan>,
    /// Offset of the first request going on the wire, and the loop's estimate of its prompt.
    wire_ms: Option<u64>,
    est_prompt_tokens: Option<u32>,
    /// When the provider's first token reached the loop.
    token_at: Option<Instant>,
    ttft_ms: Option<u64>,
    token_to_bus_ms: Option<u64>,
    read: Option<ReadStat>,
    /// The planner's cost for a cold load admitted for this turn.
    load: Option<LoadCost>,
}

pub(crate) struct TurnClock {
    core: Arc<Core>,
    session: SessionId,
    turn: u32,
    origin: Instant,
    inner: Mutex<Inner>,
}

impl TurnClock {
    /// Starts the clock for `turn`. `received` is when the daemon took the ask, `sending_ms` how
    /// long the ask spent between Enter and then (the client's stamp against the daemon's).
    pub fn start(core: &Arc<Core>, session: &SessionId, turn: u32, received: Option<Instant>, sending_ms: Option<u64>) -> Arc<Self> {
        let now = Instant::now();
        let received = received.unwrap_or(now);
        let sending = sending_ms.unwrap_or(0);
        let origin = received.checked_sub(std::time::Duration::from_millis(sending)).unwrap_or(received);
        let clock = Arc::new(Self { core: core.clone(), session: session.clone(), turn, origin, inner: Mutex::new(Inner::default()) });
        // What happened before the runner had the turn, told now that someone is listening.
        let mut at = 0;
        if sending > 0 {
            clock.retro(PhaseKind::Sending, 0, sending);
            at = sending;
        }
        let setup = clock.elapsed_ms().saturating_sub(at);
        clock.retro(PhaseKind::Setup, at, setup);
        clock
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }

    /// A step that already happened: both its events go out now.
    fn retro(&self, kind: PhaseKind, start_ms: u64, ms: u64) {
        self.emit(PhaseEvent::start(kind));
        self.emit_at(PhaseEvent::end(kind, ms), Some(start_ms));
    }

    pub fn emit(&self, ev: PhaseEvent) {
        self.emit_at(ev, None);
    }

    fn emit_at(&self, mut ev: PhaseEvent, start_ms: Option<u64>) {
        ev.session = Some(self.session.clone());
        ev.turn = self.turn;
        match ev.state {
            PhaseState::Progress => self.core.bus.publish_ephemeral(Event::Phase(ev)),
            PhaseState::Start => {
                self.core.bus.publish(Event::Phase(ev));
            }
            PhaseState::End => {
                let ms = ev.ms.unwrap_or(0);
                let start_ms = start_ms.unwrap_or_else(|| self.elapsed_ms().saturating_sub(ms));
                let nested = ev.kind == PhaseKind::Recall && ev.stage.is_some();
                let span = PhaseSpan {
                    kind: ev.kind,
                    stage: ev.stage.clone(),
                    start_ms,
                    ms,
                    n: ev.total.or(ev.done),
                    detail: ev.detail.clone(),
                    parent: nested.then_some(PhaseKind::Recall),
                    est: false,
                };
                lock(&self.inner).spans.push(span);
                self.core.bus.publish(Event::Phase(ev));
            }
        }
    }

    /// The loop's events: phases go out, the wire and first-token marks are kept.
    pub fn on_agent(&self, ev: &AgentEvent) {
        match ev {
            AgentEvent::Phase(p) => self.emit(p.clone()),
            AgentEvent::CallStarted { est_prompt_tokens, .. } => {
                let mut g = lock(&self.inner);
                g.wire_ms.get_or_insert_with(|| self.elapsed_ms());
                g.est_prompt_tokens.get_or_insert(*est_prompt_tokens);
            }
            AgentEvent::FirstToken { at } => {
                lock(&self.inner).token_at.get_or_insert(*at);
            }
            _ => {}
        }
    }

    /// The first text or reasoning of the turn is on the bus now.
    pub fn token_published(&self) {
        let mut g = lock(&self.inner);
        if g.ttft_ms.is_some() {
            return;
        }
        let Some(at) = g.token_at else { return };
        g.ttft_ms = Some(self.elapsed_ms());
        g.token_to_bus_ms = Some(at.elapsed().as_millis() as u64);
    }

    /// The cold load this turn admitted, if it did.
    pub fn set_load(&self, load: Option<LoadCost>) {
        lock(&self.inner).load = load;
    }

    /// The cold load this turn admitted, with the planner's predicted bytes.
    #[allow(dead_code)]
    pub fn load(&self) -> Option<LoadCost> {
        lock(&self.inner).load.clone()
    }

    /// What the first model call read (set by the provider's usage report).
    #[allow(dead_code)]
    pub fn set_read(&self, read: ReadStat) {
        lock(&self.inner).read.get_or_insert(read);
    }

    /// The loop's estimate of the first prompt, for the prefill estimate.
    #[allow(dead_code)]
    pub fn est_prompt_tokens(&self) -> Option<u32> {
        lock(&self.inner).est_prompt_tokens
    }

    /// The spans and totals so far.
    pub fn finish(&self) -> (Vec<PhaseSpan>, TurnNumbers) {
        let g = lock(&self.inner);
        let mut spans = g.spans.clone();
        spans.sort_by_key(|s| (s.start_ms, s.parent.is_some()));
        // Enter to the wire, less what was the model's or the user's to spend: recall, waiting
        // for admission, compaction.
        let enter_to_wire_ms = g.wire_ms.map(|wire| {
            let theirs: u64 = spans
                .iter()
                .filter(|s| s.parent.is_none() && matches!(s.kind, PhaseKind::Recall | PhaseKind::Admit | PhaseKind::Compact))
                .filter(|s| s.start_ms + s.ms <= wire + 1)
                .map(|s| s.ms)
                .sum();
            wire.saturating_sub(theirs)
        });
        let numbers = TurnNumbers { ttft_ms: g.ttft_ms, enter_to_wire_ms, token_to_bus_ms: g.token_to_bus_ms, read: g.read };
        (spans, numbers)
    }

    /// A sink for code that publishes plain events (the memory hook): phases go through the
    /// clock, everything else straight to the bus.
    pub fn sink(self: &Arc<Self>) -> RecallSink {
        let clock = self.clone();
        Arc::new(move |e: Event| match e {
            Event::Phase(p) => clock.emit(p),
            other => {
                clock.core.bus.publish(other);
            }
        })
    }
}

/// `Recall` sub-stage events for one recall stage the Mind reported.
pub(crate) fn recall_phase(ev: LiveEvent) -> PhaseEvent {
    let unit = match ev.stage {
        "rerank" => "pairs",
        _ => "items",
    };
    let mut p = match ev.kind {
        LiveKind::Start => PhaseEvent::start(PhaseKind::Recall),
        LiveKind::Progress { done, total, rate } => {
            let mut p = PhaseEvent::progress(PhaseKind::Recall, done, total).unit(unit);
            p.rate = Some(rate);
            p
        }
        LiveKind::End { ms } => {
            let mut p = PhaseEvent::end(PhaseKind::Recall, ms).unit(unit);
            p.total = ev.n;
            p
        }
    };
    p.stage = Some(ev.stage.to_string());
    p.detail = ev.detail;
    p
}

/// Where the memory write tail's events are filed: the session and turn that last ingested.
#[derive(Default)]
pub(crate) struct TailTarget(Mutex<Option<(SessionId, u32)>>);

impl TailTarget {
    pub fn set(&self, session: &SessionId, turn: u32) {
        *lock(&self.0) = Some((session.clone(), turn));
    }

    fn get(&self) -> Option<(SessionId, u32)> {
        lock(&self.0).clone()
    }
}

/// Hooks the Mind's write tail (ingest, embed, index) up to the bus as `MemoryWrite` phases.
pub(crate) fn attach_mind(core: &Arc<Core>) {
    let Some(mind) = &core.mind else { return };
    let weak: Weak<Core> = Arc::downgrade(core);
    mind.set_event_sink(MindSink::new(move |e| {
        let Some(core) = weak.upgrade() else { return };
        let Some((session, turn)) = core.ext.get_or_init(TailTarget::default).get() else { return };
        let on = |p: PhaseEvent| Event::Phase(p.on(&session, turn));
        let kind = PhaseKind::MemoryWrite;
        match e {
            MindEvent::Ingested => {
                core.bus.publish(on(PhaseEvent::end(kind, 0).stage("ingest").with_done(1).unit("items")));
            }
            MindEvent::EmbedStart { queued } => {
                core.bus.publish(on(PhaseEvent::start(kind).stage("embed").with_total(u64::from(queued)).unit("items")));
            }
            MindEvent::EmbedProgress { done, total } => {
                core.bus.publish_ephemeral(on(PhaseEvent::progress(kind, u64::from(done), u64::from(total)).stage("embed").unit("items")));
            }
            MindEvent::EmbedDone { installed, ms } => {
                core.bus.publish(on(PhaseEvent::end(kind, ms).stage("embed").with_done(u64::from(installed)).unit("items")));
            }
            MindEvent::Indexed => {
                core.bus.publish(on(PhaseEvent::end(kind, 0).stage("index")));
            }
        }
    }));
}
