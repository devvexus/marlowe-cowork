//! Live observation of the actor's work: recall stages as they run, and the write tail (ingest,
//! embed, index). Observe-only: nothing here reads or changes what retrieval selects, and a
//! missing or full channel loses events, never work.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use marlowe_memory::probe::{Stage, StageProbe};
use marlowe_proto::StageMs;

/// Longest a stage goes between two progress events.
pub const PROGRESS_EVERY: Duration = Duration::from_millis(8);

/// What happened to a recall stage.
#[derive(Debug, Clone, PartialEq)]
pub enum LiveKind {
    Start,
    /// `done` of `total` items, at `rate` items per second since the stage began.
    Progress { done: u64, total: u64, rate: f32 },
    End { ms: u64 },
}

/// One recall stage event, in the order the stages run. `stage` is `pool`, `index`, `load`,
/// `embed`, `bm25`, `dense`, `features`, `gate`, `prune`, `rerank` or `assemble`.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveEvent {
    pub stage: &'static str,
    pub kind: LiveKind,
    /// Items the stage handled, when known (on `End`).
    pub n: Option<u64>,
    /// `embedder · 127 MB`, `cpu`, `gpu`.
    pub detail: Option<String>,
}

/// Where a recall's stage events go. Bounded: when the receiver lags, events are dropped.
#[derive(Clone)]
pub struct LiveSink(Sender<LiveEvent>);

impl LiveSink {
    pub fn new(tx: Sender<LiveEvent>) -> Self {
        Self(tx)
    }

    fn send(&self, e: LiveEvent) {
        let _ = self.0.try_send(e);
    }
}

impl std::fmt::Debug for LiveSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LiveSink")
    }
}

/// What the write tail did after a turn's words were ingested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MindEvent {
    /// A belief was written to the journal and queued for embedding.
    Ingested,
    /// The embed queue began a run over `queued` beliefs.
    EmbedStart { queued: u32 },
    EmbedProgress { done: u32, total: u32 },
    /// The run finished: `installed` vectors in `ms`.
    EmbedDone { installed: u32, ms: u64 },
    /// The queue is empty: every written belief is searchable.
    Indexed,
}

/// Receives [`MindEvent`]s from the actor thread; must not block.
#[derive(Clone)]
pub struct MindSink(Arc<dyn Fn(MindEvent) + Send + Sync>);

impl MindSink {
    pub fn new(f: impl Fn(MindEvent) + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }

    pub(crate) fn emit(&self, e: MindEvent) {
        (self.0)(e)
    }
}

impl std::fmt::Debug for MindSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MindSink")
    }
}

/// Monotonic time since the probe was made.
type Clock<'a> = Box<dyn Fn() -> Duration + 'a>;

struct Open {
    name: &'static str,
    started: Duration,
    last_progress: Option<Duration>,
    total: Option<u64>,
}

/// The [`StageProbe`] the Mind passes to retrieval: times each stage, streams its start, throttled
/// progress and end to the sink, and keeps the per-stage times for the `Recalled` summary. The
/// memory crate announces boundaries and never reads a clock; this is where the clock lives.
pub(crate) struct LiveProbe<'a> {
    sink: Option<&'a LiveSink>,
    clock: Clock<'a>,
    open: Option<Open>,
    label: Option<(&'static str, String)>,
    pub stages: Vec<StageMs>,
}

impl<'a> LiveProbe<'a> {
    pub fn new(sink: Option<&'a LiveSink>) -> Self {
        let t0 = Instant::now();
        Self::with_clock(sink, Box::new(move || t0.elapsed()))
    }

    pub fn with_clock(sink: Option<&'a LiveSink>, clock: Clock<'a>) -> Self {
        Self { sink, clock, open: None, label: None, stages: Vec::new() }
    }

    /// Attaches `detail` to the start of the next stage called `stage`.
    pub fn label(&mut self, stage: &'static str, detail: String) {
        self.label = Some((stage, detail));
    }

    /// Closes the open stage and opens `name` (a stage of the Mind's own, or a pipeline stage by
    /// its display name). Staying in the same name is not a new stage.
    pub fn begin(&mut self, name: &'static str, detail: Option<String>) {
        if self.open.as_ref().is_some_and(|o| o.name == name) {
            return;
        }
        self.end(None);
        let detail = detail.or_else(|| self.label.take_if(|(s, _)| *s == name).map(|(_, d)| d));
        let now = (self.clock)();
        self.open = Some(Open { name, started: now, last_progress: None, total: None });
        if let Some(s) = self.sink {
            s.send(LiveEvent { stage: name, kind: LiveKind::Start, n: None, detail });
        }
    }

    /// Closes the open stage, if any, with `detail` on its end.
    pub fn end(&mut self, detail: Option<String>) {
        let Some(o) = self.open.take() else { return };
        let ms = ((self.clock)().saturating_sub(o.started).as_secs_f64() * 1000.0).round() as u64;
        match self.stages.last_mut() {
            // Two pipeline stages that share a display name read as one.
            Some(last) if last.name == o.name => {
                last.ms += ms;
                last.n = o.total.or(last.n);
            }
            _ => self.stages.push(StageMs { name: o.name.to_string(), ms, n: o.total }),
        }
        if let Some(s) = self.sink {
            s.send(LiveEvent { stage: o.name, kind: LiveKind::End { ms }, n: o.total, detail });
        }
    }

}

/// Display name of a pipeline stage. Candidates and scope are one `pool` step to a reader.
fn display(stage: Stage) -> &'static str {
    match stage {
        Stage::Candidates | Stage::Scope => "pool",
        Stage::Lexical => "bm25",
        Stage::Dense => "dense",
        Stage::Features => "features",
        Stage::Gate => "gate",
        Stage::Prune => "prune",
        Stage::Rerank => "rerank",
        Stage::Assemble => "assemble",
    }
}

impl StageProbe for LiveProbe<'_> {
    fn enter(&mut self, stage: Stage) {
        self.begin(display(stage), None);
    }

    fn finish(&mut self) {
        self.end(None);
    }

    fn progress(&mut self, done: u32, total: u32) {
        let now = (self.clock)();
        let Some(o) = &mut self.open else { return };
        o.total = Some(u64::from(total));
        if o.last_progress.is_some_and(|t| now.saturating_sub(t) < PROGRESS_EVERY) {
            return;
        }
        o.last_progress = Some(now);
        let secs = now.saturating_sub(o.started).as_secs_f32();
        let rate = if secs > 0.0 { done as f32 / secs } else { 0.0 };
        if let Some(s) = self.sink {
            s.send(LiveEvent {
                stage: o.name,
                kind: LiveKind::Progress { done: u64::from(done), total: u64::from(total), rate },
                n: None,
                detail: None,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    fn fake() -> (Rc<Cell<u64>>, impl Fn() -> Duration) {
        let us = Rc::new(Cell::new(0u64));
        let c = us.clone();
        (us, move || Duration::from_micros(c.get()))
    }

    #[test]
    fn stages_stream_in_order_and_progress_is_throttled() {
        let (tx, rx) = crossbeam_channel::bounded(64);
        let sink = LiveSink::new(tx);
        let (us, clock) = fake();
        let mut p = LiveProbe::with_clock(Some(&sink), Box::new(clock));
        p.enter(Stage::Candidates);
        p.enter(Stage::Scope);
        p.enter(Stage::Dense);
        // A thousand progress calls inside 20 ms: at most one per 8 ms, the first at once.
        for i in 0..1000u32 {
            us.set(i as u64 * 20);
            p.progress(i, 1000);
        }
        us.set(21_000);
        p.enter(Stage::Rerank);
        us.set(30_000);
        p.finish();
        let events: Vec<LiveEvent> = rx.try_iter().collect();
        let order: Vec<(&str, &str)> = events
            .iter()
            .filter(|e| !matches!(e.kind, LiveKind::Progress { .. }))
            .map(|e| (e.stage, if matches!(e.kind, LiveKind::Start) { "start" } else { "end" }))
            .collect();
        assert_eq!(
            order,
            vec![("pool", "start"), ("pool", "end"), ("dense", "start"), ("dense", "end"), ("rerank", "start"), ("rerank", "end")],
            "candidates and scope read as one pool step"
        );
        let progress: Vec<&LiveEvent> = events.iter().filter(|e| matches!(e.kind, LiveKind::Progress { .. })).collect();
        assert!(!progress.is_empty() && progress.len() <= 3, "20 ms of progress is at most 3 events, got {}", progress.len());
        for e in &progress {
            let LiveKind::Progress { done, total, .. } = e.kind else { unreachable!() };
            assert!(done <= total);
        }
        assert_eq!(p.stages.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["pool", "dense", "rerank"]);
        assert_eq!(p.stages[1].n, Some(1000));
        assert_eq!(p.stages[1].ms, 21);
    }

    #[test]
    fn a_probe_without_a_sink_still_times_and_a_full_channel_loses_events_not_work() {
        let (us, clock) = fake();
        let mut p = LiveProbe::with_clock(None, Box::new(clock));
        p.enter(Stage::Gate);
        us.set(4_000);
        p.finish();
        assert_eq!(p.stages[0].ms, 4);

        let (tx, rx) = crossbeam_channel::bounded(1);
        let sink = LiveSink::new(tx);
        let mut p = LiveProbe::new(Some(&sink));
        for s in Stage::ALL {
            p.enter(s);
        }
        p.finish();
        assert_eq!(rx.try_iter().count(), 1, "later events were dropped, the probe finished");
        assert!(p.stages.len() >= 7);
    }
}
