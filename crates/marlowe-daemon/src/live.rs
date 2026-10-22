//! Live work between "request sent" and "first frame": the cold-load meter, the prefill
//! estimate and the reason for a wait. One small thread per model call, alive only while the
//! engine is silent; it publishes through the turn clock and never touches the turn thread.
//!
//! Everything here is either measured or marked `est`: the load's done bytes are the engine
//! runners' real GPU memory growth against the planner's predicted total; the prefill progress is
//! Ollama's own (its runner log, [`PrefillLog`]): tokens reached, cached and fresh, its rate. With
//! no log to read there is no progress, no percentage and no rate: elapsed only.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use marlowe_hw::prefill::{Prefill, PrefillTracker};
use marlowe_hw::{GpuSource, LogFeed};
use marlowe_proto::{PhaseEvent, PhaseKind};

use crate::lock;

/// Where the watch publishes: the turn clock's `emit`, which routes `Progress` to the ephemeral
/// path and `Start`/`End` to the ordered one.
pub(crate) type Emit = Arc<dyn Fn(PhaseEvent) + Send + Sync>;

/// What the watch can read. Each probe is cheap and may answer `None` (no data).
pub(crate) struct Probes {
    /// Dedicated GPU memory held by the engine's runners now, MiB.
    pub runner_mib: Box<dyn Fn() -> Option<f64> + Send + Sync>,
    /// Whether the model is loaded by the engine now.
    pub resident: Box<dyn Fn() -> Option<bool> + Send + Sync>,
    /// The wait line when a request on this model queues behind a crew run (`waiting for crew
    /// run 6f8210de · Ollama runs qwen35 models one request at a time`); `None` when no crew run
    /// holds what the request needs, however slow the read.
    pub queued_behind: Box<dyn Fn() -> Option<String> + Send + Sync>,
    /// The rate measured on full prefills of this model, tokens per second. Never shown: it only
    /// says how long a silent read may take before the wait gets a reason.
    pub prefill_rate: Box<dyn Fn() -> Option<f64> + Send + Sync>,
}

/// Ollama's own output for the model call in flight: the request is matched to the runner's task
/// by when it was sent and how big its prompt is (see [`marlowe_hw::prefill`]).
pub(crate) struct PrefillLog {
    feed: Mutex<Box<dyn LogFeed>>,
    st: Mutex<LogSt>,
}

#[derive(Default)]
struct LogSt {
    /// Lines that arrived before the request was sent (a model load's own output), kept so the
    /// first prompt after them is not missed.
    before: Vec<String>,
    tracker: Option<PrefillTracker>,
}

/// Most lines held before the request goes out.
const BEFORE_CAP: usize = 4_000;

impl PrefillLog {
    pub fn new(feed: Box<dyn LogFeed>) -> Arc<Self> {
        Arc::new(Self { feed: Mutex::new(feed), st: Mutex::new(LogSt::default()) })
    }

    /// The request is on the wire; its prompt is about `est_prompt` tokens.
    pub fn begin(&self, est_prompt: u64) {
        let mut st = lock(&self.st);
        let mut tracker = PrefillTracker::new(est_prompt);
        std::mem::take(&mut st.before).iter().for_each(|l| tracker.feed(l));
        st.tracker = Some(tracker);
    }

    /// Reads what the engine has written since the last call; what it says about this request.
    pub fn poll(&self) -> Option<Prefill> {
        let lines = lock(&self.feed).poll();
        let mut st = lock(&self.st);
        match st.tracker.as_mut() {
            Some(t) => lines.iter().for_each(|l| t.feed(l)),
            None => {
                st.before.extend(lines);
                let over = st.before.len().saturating_sub(BEFORE_CAP);
                st.before.drain(..over);
            }
        }
        st.tracker.as_ref().and_then(|t| t.current().cloned())
    }
}

/// A cold load the planner predicted.
#[derive(Clone)]
pub(crate) struct LoadPlan {
    /// The model's name for the line.
    pub model: String,
    /// The planner's bytes for the load, MiB. `None`: unknown, no total.
    pub need_mib: Option<f64>,
}

#[derive(Default)]
struct St {
    load: Option<OpenLoad>,
    /// When the prompt read began (the request going out, or the load ending).
    prefill_from: Option<Instant>,
    waiting: Option<Instant>,
    stopped: bool,
}

struct OpenLoad {
    from: Instant,
    model: String,
    /// Measured growth, MiB; only ever rises.
    done_mib: u64,
}

struct Shared {
    sent: AtomicBool,
    stop: AtomicBool,
    /// Our estimate of the tokens this call reads fresh.
    est_new: AtomicU64,
    st: Mutex<St>,
    log: Option<Arc<PrefillLog>>,
    emit: Emit,
}

/// The watch of one model call. Dropping it stops the thread.
pub(crate) struct Watch {
    shared: Arc<Shared>,
}

impl Watch {
    /// Starts the thread. With a `load`, the runners' memory is read once now (the baseline) so
    /// the growth after the request goes out is the model's. Nothing is published until `sent`.
    pub fn start(load: Option<LoadPlan>, probes: Probes, log: Option<Arc<PrefillLog>>, emit: Emit, tick: Duration) -> Watch {
        let shared = Arc::new(Shared {
            sent: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            est_new: AtomicU64::new(0),
            st: Mutex::new(St::default()),
            log,
            emit,
        });
        let s = shared.clone();
        let spawned = thread::Builder::new().name("marlowe-live".into()).spawn(move || run(&s, load, &probes, tick));
        if spawned.is_err() {
            // No thread, no live numbers; the call itself is unaffected.
            shared.stop.store(true, Ordering::SeqCst);
        }
        Watch { shared }
    }

    /// The request is on its way; `est_new` is our estimate of the tokens it reads fresh.
    pub fn sent(&self, est_new: u64) {
        self.shared.est_new.store(est_new, Ordering::SeqCst);
        self.shared.sent.store(true, Ordering::SeqCst);
    }

    /// The engine answered: ends what was open (a load it never reported finishing, a wait) and
    /// stops the thread.
    pub fn first_frame(&self) {
        let s = &self.shared;
        s.stop.store(true, Ordering::SeqCst);
        let mut st = lock(&s.st);
        st.stopped = true;
        end_load(s, &mut st);
        end_wait(s, &mut st);
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
    }
}

fn end_load(s: &Shared, st: &mut St) {
    if let Some(l) = st.load.take() {
        let ms = l.from.elapsed().as_millis() as u64;
        let mut end = PhaseEvent::end(PhaseKind::Load, ms).stage("model").detail(l.model).unit("MiB");
        if l.done_mib > 0 {
            end = end.with_done(l.done_mib);
        }
        (s.emit)(end);
        st.prefill_from.get_or_insert_with(Instant::now);
    }
}

fn end_wait(s: &Shared, st: &mut St) {
    if let Some(from) = st.waiting.take() {
        (s.emit)(PhaseEvent::end(PhaseKind::Admit, from.elapsed().as_millis() as u64).stage("gpu"));
    }
}

fn run(s: &Shared, load: Option<LoadPlan>, probes: &Probes, tick: Duration) {
    // Already loaded after all (a race with the planner's view): there is no load to show.
    let load = load.filter(|_| (probes.resident)() != Some(true));
    let baseline = load.as_ref().and_then(|_| (probes.runner_mib)());
    while !s.sent.load(Ordering::SeqCst) {
        if s.stop.load(Ordering::SeqCst) {
            return;
        }
        thread::sleep(Duration::from_millis(5).min(tick));
    }
    let sent_at = Instant::now();
    {
        let mut st = lock(&s.st);
        if st.stopped {
            return;
        }
        match &load {
            Some(plan) => {
                st.load = Some(OpenLoad { from: sent_at, model: plan.model.clone(), done_mib: 0 });
                let mut start = PhaseEvent::start(PhaseKind::Load).stage("model").detail(plan.model.clone()).unit("MiB");
                if let Some(need) = plan.need_mib {
                    start = start.with_total(need.round() as u64);
                    start.est = true;
                }
                (s.emit)(start);
            }
            None => st.prefill_from = Some(sent_at),
        }
    }
    let rate = (probes.prefill_rate)().filter(|r| *r > 0.0 && r.is_finite());
    let mut ticks = 0u32;
    let mut last: Option<(u64, u64, Option<u64>, Option<u32>)> = None;
    while !s.stop.load(Ordering::SeqCst) {
        thread::sleep(tick);
        ticks += 1;
        let mut st = lock(&s.st);
        if st.stopped {
            return;
        }
        if st.load.is_some() {
            // The model counts as loaded when the engine lists it; the first frame ends it
            // otherwise. Asked every few ticks: it is a request to the engine.
            if ticks % 4 == 0 && (probes.resident)() == Some(true) {
                end_load(s, &mut st);
                continue;
            }
            if let (Some(base), Some(now)) = (baseline, (probes.runner_mib)()) {
                let grew = (now - base).max(0.0).round() as u64;
                let open = st.load.as_mut().expect("checked above");
                open.done_mib = open.done_mib.max(grew);
                if open.done_mib > 0 {
                    let done = open.done_mib;
                    let mut p = PhaseEvent::progress(PhaseKind::Load, done, done).stage("model").unit("MiB");
                    p.total = load.as_ref().and_then(|l| l.need_mib).map(|need| {
                        // Never full before it ends: a total the growth has passed is raised.
                        (need.round() as u64).max((done as f64 / 0.99).ceil() as u64)
                    });
                    p.est = true;
                    (s.emit)(p);
                }
            }
            continue;
        }
        let Some(from) = st.prefill_from else { continue };
        let elapsed = from.elapsed();
        let est_new = s.est_new.load(Ordering::SeqCst);
        // How long the read should take at the rate measured before; used only to decide when a
        // silent wait needs its reason said, never shown.
        let est_ms = rate.filter(|_| est_new > 0).map_or(0.0, |r| est_new as f64 / r * 1000.0);
        // The engine's own numbers, once its output says anything about this request.
        if let Some(p) = s.log.as_ref().and_then(|l| l.poll()) {
            let seen = (p.done, p.total, p.cached, p.rate.map(|r| (r * 100.0) as u32));
            if last != Some(seen) {
                last = Some(seen);
                let mut e = PhaseEvent::progress(PhaseKind::Prefill, p.done, p.total).unit("tok");
                e.cached = p.cached;
                e.rate = p.rate.map(|r| r as f32);
                (s.emit)(e);
            }
        }
        // Silent well past what the read should take, and crew runs hold every request the
        // runner serves on this model: the request is queued behind them, and the line says why.
        if st.waiting.is_none() && elapsed.as_secs_f64() * 1000.0 > (est_ms * 1.5).max(500.0) {
            if let Some(line) = (probes.queued_behind)() {
                st.waiting = Some(Instant::now());
                (s.emit)(PhaseEvent::start(PhaseKind::Admit).stage("gpu").detail(line));
            }
        }
    }
}

/// Dedicated memory held by the engine's runners now, MiB; `None` when there is no GPU reading.
pub(crate) fn runners_mib(gpu: &dyn GpuSource) -> Option<f64> {
    gpu.snapshot().map(|s| s.runners.iter().map(|(_, mib)| *mib).sum())
}

/// What an unload freed, MiB: runner memory before less after. `None` unless both readings exist.
pub(crate) fn freed_mib(before: Option<f64>, after: Option<f64>) -> Option<f64> {
    Some((before? - after?).max(0.0))
}

/// `a` and `b` name the same Ollama model (`name` and `name:latest` are one).
pub(crate) fn same_model(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        let s = s.trim().to_ascii_lowercase();
        if s.contains(':') {
            s
        } else {
            format!("{s}:latest")
        }
    };
    norm(a) == norm(b)
}

/// Calibration key for a model's measured rates.
pub(crate) fn rate_key(kind: &str, model: &str) -> String {
    // `prefill_tok_s:` held rates taken from Ollama's usage, which counts cached tokens as read;
    // they are forgotten ([`OLD_PREFILL_KEYS`]) and this name starts clean.
    match kind {
        "prefill" => format!("prefill_measured_tok_s:{model}"),
        _ => format!("{kind}_tok_s:{model}"),
    }
}

/// The stored prefill rates from before the rate was only learned from measured full reads.
pub(crate) const OLD_PREFILL_KEYS: &str = "prefill_tok_s:";

/// Folds a measured sample into the stored rate (`None`: first sample).
pub(crate) fn blend(stored: Option<f64>, sample: f64) -> f64 {
    match stored {
        Some(old) if old > 0.0 => old * 0.7 + sample * 0.3,
        _ => sample,
    }
}

/// Fewest evaluated tokens that make a prefill rate worth keeping; shorter reads are mostly
/// overhead.
pub(crate) const MIN_RATE_TOKENS: u64 = 256;

#[cfg(test)]
mod tests {
    use super::*;
    use marlowe_hw::{GpuBudget, GpuSnapshot};
    use marlowe_proto::PhaseState;

    fn collect() -> (Emit, Arc<Mutex<Vec<PhaseEvent>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        (Arc::new(move |e| s.lock().unwrap().push(e)), seen)
    }

    fn probes(runner: impl Fn() -> Option<f64> + Send + Sync + 'static, resident: impl Fn() -> Option<bool> + Send + Sync + 'static) -> Probes {
        Probes {
            runner_mib: Box::new(runner),
            resident: Box::new(resident),
            queued_behind: Box::new(|| None),
            prefill_rate: Box::new(|| None),
        }
    }

    /// Runner memory that rises by `step` MiB every read, from `from`.
    fn growing(from: f64, step: f64) -> impl Fn() -> Option<f64> + Send + Sync {
        let n = AtomicU64::new(0);
        move || Some(from + step * n.fetch_add(1, Ordering::SeqCst) as f64)
    }

    fn wait_for(seen: &Arc<Mutex<Vec<PhaseEvent>>>, what: impl Fn(&[PhaseEvent]) -> bool) {
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end {
            if what(&seen.lock().unwrap()) {
                return;
            }
            thread::sleep(Duration::from_millis(2));
        }
        panic!("never happened: {:?}", seen.lock().unwrap());
    }

    fn loading() -> Option<LoadPlan> {
        Some(LoadPlan { model: "dusk-27b".into(), need_mib: Some(13_414.0) })
    }

    #[test]
    fn the_load_meter_grows_monotonically_stays_under_full_and_ends_on_the_first_frame() {
        let (emit, seen) = collect();
        // 0 -> 6.1 -> 13.1 GB, then more than predicted.
        let steps = [0.0, 3_000.0, 6_246.0, 6_246.0, 13_414.0, 14_000.0, 14_000.0];
        let i = AtomicU64::new(0);
        let runner = move || Some(steps[(i.fetch_add(1, Ordering::SeqCst) as usize).min(steps.len() - 1)]);
        let w = Watch::start(loading(), probes(runner, || Some(false)), None, emit, Duration::from_millis(2));
        w.sent(0);
        wait_for(&seen, |e| e.iter().filter(|p| p.state == PhaseState::Progress).count() >= 4);
        w.first_frame();
        let ev = seen.lock().unwrap().clone();
        assert_eq!(ev[0].state, PhaseState::Start);
        assert_eq!(ev[0].kind, PhaseKind::Load);
        assert!(ev[0].est && ev[0].total == Some(13_414), "the predicted total is an estimate");
        let progress: Vec<&PhaseEvent> = ev.iter().filter(|p| p.state == PhaseState::Progress).collect();
        assert!(progress.windows(2).all(|w| w[0].done <= w[1].done), "monotonic");
        assert!(progress.iter().all(|p| p.est && p.done.unwrap() < p.total.unwrap()), "never full before it ends");
        let end = ev.last().unwrap();
        assert_eq!((end.kind, end.state), (PhaseKind::Load, PhaseState::End));
        assert!(!end.est && end.ms.is_some(), "the end is measured");
        assert!(end.done.unwrap() >= 6_246, "snaps to what was measured: {end:?}");
        assert_eq!(ev.iter().filter(|p| p.state == PhaseState::End).count(), 1);
    }

    #[test]
    fn no_runner_data_or_no_predicted_bytes_shows_no_bar() {
        // Unified memory: the GPU reading never shows a runner.
        let (emit, seen) = collect();
        let w = Watch::start(loading(), probes(|| Some(0.0), || Some(false)), None, emit, Duration::from_millis(2));
        w.sent(0);
        thread::sleep(Duration::from_millis(40));
        w.first_frame();
        let ev = seen.lock().unwrap().clone();
        assert!(ev.iter().all(|p| p.state != PhaseState::Progress), "{ev:?}");
        assert_eq!(ev.last().unwrap().state, PhaseState::End);
        // No reading at all, and no predicted total.
        let (emit, seen) = collect();
        let plan = Some(LoadPlan { model: "m".into(), need_mib: None });
        let w = Watch::start(plan, probes(|| None, || None), None, emit, Duration::from_millis(2));
        w.sent(0);
        thread::sleep(Duration::from_millis(30));
        w.first_frame();
        let ev = seen.lock().unwrap().clone();
        assert_eq!(ev[0].total, None);
        assert!(ev.iter().all(|p| p.state != PhaseState::Progress));
    }

    #[test]
    fn growth_without_a_total_reports_done_only() {
        let (emit, seen) = collect();
        let plan = Some(LoadPlan { model: "m".into(), need_mib: None });
        let w = Watch::start(plan, probes(growing(500.0, 700.0), || Some(false)), None, emit, Duration::from_millis(2));
        w.sent(0);
        wait_for(&seen, |e| e.iter().any(|p| p.state == PhaseState::Progress));
        w.first_frame();
        let ev = seen.lock().unwrap().clone();
        let p = ev.iter().find(|p| p.state == PhaseState::Progress).unwrap();
        assert!(p.done.unwrap() > 0 && p.total.is_none());
    }

    #[test]
    fn the_engine_listing_the_model_ends_the_load_before_the_first_frame() {
        let (emit, seen) = collect();
        let polls = Arc::new(AtomicU64::new(0));
        let p = polls.clone();
        // Not resident for the pre-check, then resident after a few polls.
        let resident = move || Some(p.fetch_add(1, Ordering::SeqCst) >= 3);
        let w = Watch::start(loading(), probes(growing(0.0, 100.0), resident), None, emit, Duration::from_millis(2));
        w.sent(0);
        wait_for(&seen, |e| e.iter().any(|p| p.kind == PhaseKind::Load && p.state == PhaseState::End));
        let n = seen.lock().unwrap().len();
        w.first_frame();
        assert_eq!(seen.lock().unwrap().len(), n, "the first frame does not end it twice");
    }

    #[test]
    fn a_model_already_loaded_has_no_load_line() {
        let (emit, seen) = collect();
        let w = Watch::start(loading(), probes(|| Some(0.0), || Some(true)), None, emit, Duration::from_millis(2));
        w.sent(0);
        thread::sleep(Duration::from_millis(20));
        w.first_frame();
        assert!(seen.lock().unwrap().iter().all(|p| p.kind != PhaseKind::Load));
    }

    fn with_rate(rate: Option<f64>, queued: Option<&'static str>) -> Probes {
        Probes {
            runner_mib: Box::new(|| None),
            resident: Box::new(|| Some(true)),
            queued_behind: Box::new(move || queued.map(str::to_string)),
            prefill_rate: Box::new(move || rate),
        }
    }

    /// A feed that hands out its lines one batch per poll.
    struct Script(Vec<Vec<&'static str>>);
    impl LogFeed for Script {
        fn poll(&mut self) -> Vec<String> {
            if self.0.is_empty() {
                return Vec::new();
            }
            self.0.remove(0).into_iter().map(str::to_string).collect()
        }
    }

    #[test]
    fn a_stored_rate_alone_shows_no_progress_at_all() {
        // Whatever was measured before, this read has no numbers of its own: elapsed only.
        let (emit, seen) = collect();
        let w = Watch::start(None, with_rate(Some(10_000.0), None), None, emit, Duration::from_millis(2));
        w.sent(100);
        thread::sleep(Duration::from_millis(60));
        w.first_frame();
        assert!(seen.lock().unwrap().is_empty(), "{:?}", seen.lock().unwrap());
    }

    #[test]
    fn the_runner_log_gives_measured_progress_with_the_cached_part_named() {
        let batches = vec![
            vec![],
            vec![
                "slot   operator(): id  0 | task 3 | new prompt, n_ctx_slot = 32768, n_keep = 4, task.n_tokens = 12611",
                "slot   operator(): id  0 | task 3 | restored context checkpoint (pos_min = 6646, pos_max = 6646, n_tokens = 6647, n_past = 6647, size = 174.9 MiB)",
                "slot   operator(): id  0 | task 3 | cached n_tokens = 6647, memory_seq_rm [6647, end)",
            ],
            vec!["slot print_timing: id  0 | task 3 | prompt processing, n_tokens = 7671, progress = 0.61, t = 9.88 s / 103.64 tokens per second"],
        ];
        let (emit, seen) = collect();
        let log = PrefillLog::new(Box::new(Script(batches)));
        log.begin(12_000);
        let w = Watch::start(None, with_rate(None, None), Some(log), emit, Duration::from_millis(2));
        w.sent(5_000);
        wait_for(&seen, |e| e.iter().any(|p| p.done == Some(7_671)));
        w.first_frame();
        let ev = seen.lock().unwrap().clone();
        assert!(ev.iter().all(|p| p.kind == PhaseKind::Prefill && p.state == PhaseState::Progress && !p.est), "measured, never estimated: {ev:?}");
        let first = ev.first().unwrap();
        assert_eq!((first.done, first.total, first.cached), (Some(6_647), Some(12_611), Some(6_647)));
        let last = ev.last().unwrap();
        assert_eq!((last.done, last.total, last.cached, last.rate), (Some(7_671), Some(12_611), Some(6_647), Some(103.64)));
        assert!(ev.windows(2).all(|w| w[0].done <= w[1].done));
    }

    #[test]
    fn lines_about_another_request_show_nothing() {
        let batches = vec![vec!["slot   operator(): id  0 | task 9 | new prompt, n_ctx_slot = 32768, n_keep = 4, task.n_tokens = 90"]];
        let (emit, seen) = collect();
        let log = PrefillLog::new(Box::new(Script(batches)));
        log.begin(12_000);
        let w = Watch::start(None, with_rate(None, None), Some(log), emit, Duration::from_millis(2));
        w.sent(12_000);
        thread::sleep(Duration::from_millis(40));
        w.first_frame();
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn lines_written_before_the_request_goes_out_are_kept_for_it() {
        let log = PrefillLog::new(Box::new(Script(vec![vec![
            "slot   operator(): id  0 | task 1 | new prompt, n_ctx_slot = 32768, n_keep = 4, task.n_tokens = 8000",
            "slot   operator(): id  0 | task 1 | cached n_tokens = 0, memory_seq_rm [0, end)",
        ]])));
        assert!(log.poll().is_none(), "no request yet");
        log.begin(8_000);
        let p = log.poll().unwrap();
        assert_eq!((p.total, p.cached), (8_000, Some(0)));
    }

    #[test]
    fn a_wait_names_its_crew_run_only_when_the_request_queues_behind_it() {
        // Silent past the estimate and queued behind a crew run: the reason is stated, then ended.
        let line = "waiting for crew run b8ee1234 · Ollama runs qwen35 models one request at a time";
        let (emit, seen) = collect();
        let w = Watch::start(None, with_rate(Some(1_000_000.0), Some(line)), None, emit, Duration::from_millis(100));
        w.sent(10);
        wait_for(&seen, |e| e.iter().any(|p| p.kind == PhaseKind::Admit));
        w.first_frame();
        let ev = seen.lock().unwrap().clone();
        let start = ev.iter().find(|p| p.kind == PhaseKind::Admit && p.state == PhaseState::Start).unwrap();
        assert_eq!(start.detail.as_deref(), Some(line));
        assert_eq!(start.stage.as_deref(), Some("gpu"));
        assert!(ev.iter().any(|p| p.kind == PhaseKind::Admit && p.state == PhaseState::End && p.ms.is_some()));
        // Not queued (the runner serves more than the crew holds): nothing is said, however long.
        let (emit, seen) = collect();
        let w = Watch::start(None, with_rate(Some(1_000_000.0), None), None, emit, Duration::from_millis(100));
        w.sent(10);
        thread::sleep(Duration::from_millis(700));
        w.first_frame();
        assert!(seen.lock().unwrap().iter().all(|p| p.kind != PhaseKind::Admit));
    }

    #[test]
    fn a_stopped_watch_ends_its_thread_and_a_watch_never_sent_publishes_nothing() {
        let (emit, seen) = collect();
        let w = Watch::start(loading(), probes(|| Some(0.0), || Some(false)), None, emit, Duration::from_millis(2));
        drop(w);
        thread::sleep(Duration::from_millis(30));
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn calibration_helpers() {
        assert!(same_model("marlowe-dawn", "marlowe-dawn:latest"));
        assert!(!same_model("a:9b", "a:22b"));
        assert_eq!(rate_key("prefill", "ollama/x"), "prefill_measured_tok_s:ollama/x");
        assert_eq!(blend(None, 1000.0), 1000.0);
        assert!((blend(Some(1000.0), 2000.0) - 1300.0).abs() < 1e-9);
        assert_eq!(freed_mib(Some(13_414.0), Some(300.0)), Some(13_114.0));
        assert_eq!(freed_mib(None, Some(1.0)), None);
    }

    struct Fake(Mutex<Vec<Option<GpuSnapshot>>>);
    impl GpuSource for Fake {
        fn snapshot(&self) -> Option<GpuSnapshot> {
            self.0.lock().unwrap().remove(0)
        }
    }

    #[test]
    fn runner_memory_sums_the_engines_processes() {
        let snap = |runners: Vec<(u32, f64)>| GpuSnapshot { gpu: GpuBudget { name: "fake".into(), total_mib: 0.0, used_mib: 0.0, free_mib: 0.0, runners_mib: 0.0, runners_shared_mib: 0.0, headroom_mib: 0.0, bandwidth_gbs: None }, runners, ram_total_mib: None, ram_free_mib: None };
        let f = Fake(Mutex::new(vec![Some(snap(vec![(1, 100.0), (2, 50.5)])), None]));
        assert_eq!(runners_mib(&f), Some(150.5));
        assert_eq!(runners_mib(&f), None);
    }
}
