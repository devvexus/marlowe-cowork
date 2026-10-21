//! A turn's phase clock: every named step between Enter and the first token, as events for the
//! client and as measured spans for the turn's summary and the database.
//!
//! The clock's zero is Enter: the client's own stamp when the `Ask` carried one, else the daemon
//! receiving it, else the runner starting the turn. `Start` and `End` publish in order (replayed);
//! `Progress` is ephemeral. Spans are only what was measured, never an estimate.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use marlowe_agent::AgentEvent;
use marlowe_brains::LoadCost;
use marlowe_mind::{LiveEvent, LiveKind, MindEvent, MindSink};
use marlowe_model::{ModelRef, Usage};
use marlowe_proto::{
    Event, PhaseEvent, PhaseKind, PhaseSpan, PhaseState, ReadStat, SessionId, TurnNumbers,
};

use crate::daemon::Core;
use crate::live::{self, Emit, LoadPlan, Probes, Watch};
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
    model: Option<ModelRef>,
    /// The live meter of the model call in flight (load, prefill estimate, wait reason).
    watch: Option<Arc<Watch>>,
    /// When each open phase started, so its span begins there and not where its `End` landed.
    starts: HashMap<(PhaseKind, Option<String>), u64>,
}

/// How often the watch reads the GPU and publishes estimates.
const WATCH_TICK: Duration = Duration::from_millis(125);

/// The prompt size the last call of each session read in full, to guess what the next one finds
/// already in the engine's cache.
#[derive(Default)]
struct LastPrompt(Mutex<HashMap<SessionId, (String, u64)>>);

/// The span a phase nests inside, if it does: those are excluded when spans are summed against
/// the time to first token.
fn parent_of(kind: PhaseKind, stage: &Option<String>) -> Option<PhaseKind> {
    match kind {
        PhaseKind::Recall if stage.is_some() => Some(PhaseKind::Recall),
        PhaseKind::Wire | PhaseKind::Admit if stage.is_some() => Some(PhaseKind::Wire),
        PhaseKind::Prefill | PhaseKind::Load => Some(PhaseKind::Wire),
        _ => None,
    }
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
                lock(&self.inner).starts.insert((ev.kind, ev.stage.clone()), self.elapsed_ms());
                self.core.bus.publish(Event::Phase(ev));
            }
            PhaseState::End => {
                let ms = ev.ms.unwrap_or(0);
                let opened = lock(&self.inner).starts.remove(&(ev.kind, ev.stage.clone()));
                let start_ms = start_ms.or(opened).unwrap_or_else(|| self.elapsed_ms().saturating_sub(ms));
                let span = PhaseSpan {
                    kind: ev.kind,
                    stage: ev.stage.clone(),
                    start_ms,
                    ms,
                    n: ev.total.or(ev.done),
                    detail: ev.detail.clone(),
                    parent: parent_of(ev.kind, &ev.stage),
                    est: false,
                };
                lock(&self.inner).spans.push(span);
                self.core.bus.publish(Event::Phase(ev));
            }
        }
    }

    /// The loop's events: phases go out, the wire and first-token marks are kept.
    pub fn on_agent(self: &Arc<Self>, ev: &AgentEvent) {
        match ev {
            AgentEvent::Phase(p) => {
                // The load ends before the wire does: it was inside it.
                if p.kind == PhaseKind::Wire && p.state == PhaseState::End && p.stage.is_none() {
                    self.first_frame();
                }
                self.emit(p.clone());
                match (p.kind, p.state) {
                    (PhaseKind::Prefill, PhaseState::Start) => self.request_sent(p.total.unwrap_or(0)),
                    (PhaseKind::Prefill, PhaseState::End) => self.read_ended(p),
                    _ => {}
                }
            }
            AgentEvent::Usage(u) => self.calibrate(u),
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

    pub fn set_model(&self, model: &ModelRef) {
        lock(&self.inner).model = Some(model.clone());
    }

    /// The cold load this turn admitted, if it did. Its meter starts at once (reading the
    /// runners' memory before the model is asked for) and publishes once the request is sent.
    pub fn set_load(self: &Arc<Self>, load: Option<LoadCost>) {
        // A load is only shown when the engine said the model is not loaded, and the hardware
        // sources are here to watch it: with no answer, it may well be loaded.
        let watchable = self.core.brains.factory().hardware().is_some();
        let plan = load.as_ref().filter(|c| c.resident_known && watchable).map(|c| LoadPlan { model: c.model.model_id.clone(), need_mib: c.need_mib });
        let watch = plan.map(|p| self.new_watch(Some(p)));
        let mut g = lock(&self.inner);
        g.load = load;
        g.watch = watch;
    }

    fn new_watch(self: &Arc<Self>, load: Option<LoadPlan>) -> Arc<Watch> {
        let model = lock(&self.inner).model.clone();
        let weak = Arc::downgrade(self);
        let emit: Emit = Arc::new(move |e| {
            if let Some(clock) = weak.upgrade() {
                clock.emit(e);
            }
        });
        Arc::new(Watch::start(load, probes(&self.core, model), emit, WATCH_TICK))
    }

    /// The request for a model call is on its way: the live meter starts publishing.
    fn request_sent(self: &Arc<Self>, est_prompt: u64) {
        let last = {
            let model = lock(&self.inner).model.as_ref().map(ToString::to_string);
            let memory = self.core.ext.get_or_init(LastPrompt::default);
            let seen = lock(&memory.0).get(&self.session).cloned();
            seen.filter(|(m, _)| Some(m) == model.as_ref()).map_or(0, |(_, n)| n)
        };
        let waiting = lock(&self.inner).watch.take();
        let watch = waiting.unwrap_or_else(|| self.new_watch(None));
        watch.sent(est_prompt.saturating_sub(last));
        lock(&self.inner).watch = Some(watch);
    }

    /// The engine's first frame: the load (if it was still open) ends, the estimates stop.
    fn first_frame(&self) {
        let watch = lock(&self.inner).watch.take();
        if let Some(w) = watch {
            w.first_frame();
        }
    }

    /// The call reported what it read: the meter is done, and the call's numbers are kept.
    fn read_ended(&self, p: &PhaseEvent) {
        self.first_frame();
        let (Some(new), Some(total)) = (p.done, p.total) else { return };
        let model = lock(&self.inner).model.as_ref().map(ToString::to_string).unwrap_or_default();
        lock(&self.core.ext.get_or_init(LastPrompt::default).0).insert(self.session.clone(), (model, total));
        self.set_read(ReadStat { new_tok: new, cached_tok: total.saturating_sub(new), ms: p.ms.unwrap_or(0) });
    }

    /// Keeps the engine's measured prefill and decode rates for the next estimate.
    fn calibrate(&self, u: &Usage) {
        let Some(model) = lock(&self.inner).model.clone() else { return };
        let mut samples = Vec::new();
        if let (Some(n), Some(ms)) = (u.prompt_eval, u.prefill_ms) {
            if n >= live::MIN_RATE_TOKENS && ms > 0 {
                samples.push(("prefill", n as f64 * 1000.0 / ms as f64));
            }
        }
        if let Some(ms) = u.decode_ms {
            if u.output >= 16 && ms > 0 {
                samples.push(("decode", u.output as f64 * 1000.0 / ms as f64));
            }
        }
        if samples.is_empty() {
            return;
        }
        self.core.brains.with_library(|l| {
            for (kind, sample) in samples {
                let key = live::rate_key(kind, &model.to_string());
                let stored = l.calibration(&key).ok().flatten();
                let _ = l.set_calibration(&key, live::blend(stored, sample));
            }
        });
    }

    /// What the first model call read (set by the prefill phase's end).
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

/// What the live meter reads: the GPU and the engine's model list through the planner's hardware
/// sources, the crew supervisor, and the stored prefill rate.
fn probes(core: &Arc<Core>, model: Option<ModelRef>) -> Probes {
    let hw = core.brains.factory().hardware().cloned();
    let name = model.as_ref().map(|m| m.model_id.clone()).unwrap_or_default();
    let (gpu, models) = (hw.clone(), hw);
    let (for_crew, for_rate) = (Arc::downgrade(core), Arc::downgrade(core));
    let (crew_model, rate_model) = (model.clone(), model);
    Probes {
        runner_mib: Box::new(move || gpu.as_ref().and_then(|h| live::runners_mib(h.gpu.as_ref()))),
        resident: Box::new(move || {
            let loaded = models.as_ref()?.models.resident()?;
            Some(loaded.iter().any(|n| live::same_model(n, &name)))
        }),
        crew_run: Box::new(move || {
            let core = for_crew.upgrade()?;
            let run = core.crew.get()?.supervisor.local_stream_on(crew_model.as_ref()?)?;
            Some(run.short().to_string())
        }),
        prefill_rate: Box::new(move || {
            let core = for_rate.upgrade()?;
            let key = live::rate_key("prefill", &rate_model.as_ref()?.to_string());
            core.brains.with_library(|l| l.calibration(&key).ok().flatten())
        }),
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
