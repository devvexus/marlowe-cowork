//! Marlowe's own llama-server: one process, one model, several slots.
//!
//! The engine starts Ollama's bundled `llama-server.exe` on demand for a model the planner
//! assigned to it, serves every request for that model on one of its slots, and stops it after
//! the keep-alive Ollama would have used. It never runs a model Ollama also has loaded: before a
//! load it asks Ollama to unload the same model (`keep_alive: 0`).
//!
//! # Slots
//!
//! A slot is one conversation's prompt cache. The conductor's conversation keeps slot 0, so crew
//! and side calls never evict it; every other conversation sticks to the slot it used last (found
//! by a hash of its system prompt and first message) and otherwise takes a free one, least
//! recently used first. A request waits when every slot it may use is busy: the crew is bounded
//! by the same count, so it rarely does. The conductor never waits for slot 0: when another
//! request holds it, it takes any free slot.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock, Weak};
use std::time::{Duration, Instant};

use marlowe_model::{CancelToken, ContextAllocations, EngineObserver, EngineShape, Lane, ProviderError};

use super::launch::{self, Spec};
use super::persist;
use super::server::{offloaded, Process, Spawner};
use crate::http::{self, LocalEndpoint};
use crate::ollama_runtime::{parse_ps, unload_body};
use crate::ollama_store::{self, Sampling};

/// How long a request waits for a model switch to drain before it gives up.
const SWITCH_WAIT: Duration = Duration::from_secs(300);
/// A load that failed this recently is not tried again for the same shape.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(15);
const QUICK: Duration = Duration::from_millis(1500);

/// What the engine needs to know about the machine.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Ollama, asked to unload a model before the engine loads it.
    pub ollama: LocalEndpoint,
    /// Ollama's model store.
    pub store: PathBuf,
    /// Where the server's output goes (appended, capped).
    pub log: Option<PathBuf>,
    /// Idle time before the server is stopped; `None` keeps it until something else needs the GPU.
    pub keep_alive: Option<Duration>,
    /// Longest a load may take.
    pub start_timeout: Duration,
    pub ubatch: u32,
    /// Where each slot's cache is written when the server stops and read back when it starts
    /// again; `None` keeps no caches across a stop.
    pub slots_dir: Option<PathBuf>,
    /// The server binary; `None` finds the one Ollama bundles.
    pub binary: Option<PathBuf>,
    /// How often a starting server is asked whether it is ready.
    pub poll: Duration,
    /// How often the idle check runs.
    pub idle_tick: Duration,
}

impl EngineConfig {
    pub fn new(ollama: LocalEndpoint, store: PathBuf) -> Self {
        Self {
            ollama,
            store,
            log: None,
            keep_alive: Some(Duration::from_secs(300)),
            start_timeout: Duration::from_secs(240),
            ubatch: 512,
            slots_dir: None,
            binary: None,
            poll: Duration::from_millis(250),
            idle_tick: Duration::from_secs(2),
        }
    }
}

/// Ollama's `OLLAMA_KEEP_ALIVE`: seconds, or a number with `s`, `m` or `h`; negative keeps the
/// model loaded for good, and unset is five minutes.
pub fn keep_alive_from(value: Option<&str>) -> Option<Duration> {
    let Some(v) = value.map(str::trim).filter(|v| !v.is_empty()) else { return Some(Duration::from_secs(300)) };
    let (num, unit) = match v.char_indices().find(|(_, c)| c.is_ascii_alphabetic()) {
        Some((i, _)) => v.split_at(i),
        None => (v, "s"),
    };
    let n: f64 = num.parse().ok()?;
    if n < 0.0 {
        return None;
    }
    let secs = match unit {
        "ms" => n / 1000.0,
        "s" => n,
        "m" => n * 60.0,
        "h" => n * 3600.0,
        _ => return Some(Duration::from_secs(300)),
    };
    Some(Duration::from_secs_f64(secs))
}

/// What to serve and for whom.
#[derive(Debug, Clone)]
pub struct Want {
    pub model: String,
    pub shape: EngineShape,
    pub lane: Lane,
    /// Identifies the conversation (see [`conversation_key`]).
    pub key: u64,
}

pub use marlowe_model::prefix::conversation_key;

/// `marlowe-dawn` and `marlowe-dawn:latest` are the same model to Ollama.
pub fn same_model(a: &str, b: &str) -> bool {
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

struct Slot {
    busy: bool,
    key: Option<u64>,
    last: Instant,
}

struct Running {
    model: String,
    shape: EngineShape,
    port: u16,
    blob: PathBuf,
    proc: Box<dyn Process>,
    epoch: u64,
    load_secs: f64,
    last_used: Instant,
    on_gpu: Option<(u32, u32)>,
    sampling: Option<Sampling>,
    slots: Vec<Slot>,
}

impl Running {
    fn matches(&self, want: &Want) -> bool {
        same_model(&self.model, &want.model) && self.shape == want.shape
    }

    fn busy(&self) -> usize {
        self.slots.iter().filter(|s| s.busy).count()
    }

    /// The slot a request takes, if one is free for it.
    fn pick(&self, lane: Lane, key: u64) -> Option<usize> {
        let free = |i: &usize| !self.slots[*i].busy;
        if self.slots.len() == 1 {
            return Some(0).filter(free);
        }
        let others = || (1..self.slots.len()).filter(free);
        let oldest = |it: Box<dyn Iterator<Item = usize> + '_>| it.min_by_key(|i| self.slots[*i].last);
        match lane {
            Lane::Conductor => Some(0).filter(free).or_else(|| oldest(Box::new(others()))),
            Lane::Other => others()
                .find(|i| self.slots[*i].key == Some(key))
                .or_else(|| others().find(|i| self.slots[*i].key.is_none()))
                .or_else(|| oldest(Box::new(others()))),
        }
    }
}

#[derive(Default)]
struct State {
    running: Option<Running>,
    loading: Option<String>,
    epoch: u64,
    failed: Option<(String, EngineShape, Instant, ProviderError)>,
}

struct Inner {
    cfg: EngineConfig,
    spawner: Arc<dyn Spawner>,
    st: Mutex<State>,
    cv: Condvar,
    events: RwLock<Option<ContextAllocations>>,
}

impl Inner {
    /// Stops a server on purpose: first its slots' caches are written to disk, so the next load of
    /// the model gives each conversation its cache back (see [`persist`]). Called under the lock.
    fn retire(&self, r: &mut Running) {
        if let Some(dir) = &self.cfg.slots_dir {
            if r.proc.exited().is_none() {
                let used: Vec<(u32, u64)> =
                    r.slots.iter().enumerate().filter(|(_, s)| !s.busy).filter_map(|(i, s)| s.key.map(|k| (i as u32, k))).collect();
                let endpoint = LocalEndpoint::new("127.0.0.1", r.port).expect("loopback");
                persist::save(&endpoint, dir, &r.model, &r.blob, &r.shape, &used);
            }
        }
        r.proc.kill();
    }
}

/// What the engine runs now.
#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub model: String,
    pub shape: EngineShape,
    pub port: u16,
    pub pid: u32,
    pub load_secs: f64,
    /// `offloaded N/M layers` from the load.
    pub on_gpu: Option<(u32, u32)>,
    /// Slots with a request in flight.
    pub busy: usize,
}

/// The engine. Cheap to clone.
#[derive(Clone)]
pub struct LlamaEngine {
    inner: Arc<Inner>,
}

/// A slot held for one request; released on drop.
pub struct Lease {
    inner: Arc<Inner>,
    epoch: u64,
    pub endpoint: LocalEndpoint,
    pub slot: u32,
    pub sampling: Option<Sampling>,
    /// The model's per-slot window, for the request's own check.
    pub ctx_per_slot: u32,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut g = lock(&self.inner.st);
        if let Some(r) = g.running.as_mut().filter(|r| r.epoch == self.epoch) {
            if let Some(s) = r.slots.get_mut(self.slot as usize) {
                s.busy = false;
                s.last = Instant::now();
            }
            r.last_used = Instant::now();
        }
        self.inner.cv.notify_all();
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl LlamaEngine {
    pub fn new(cfg: EngineConfig, spawner: Arc<dyn Spawner>) -> Self {
        let inner = Arc::new(Inner { cfg, spawner, st: Mutex::new(State::default()), cv: Condvar::new(), events: RwLock::new(None) });
        let weak = Arc::downgrade(&inner);
        let tick = inner.cfg.idle_tick;
        std::thread::Builder::new()
            .name("llama-idle".into())
            .spawn(move || idle_loop(weak, tick))
            .ok();
        Self { inner }
    }

    /// Reports loads and unloads to whoever the shared allocations name (the daemon).
    pub fn follow(&self, events: ContextAllocations) {
        *self.inner.events.write().unwrap_or_else(|e| e.into_inner()) = Some(events);
    }

    /// What runs now, if anything.
    pub fn status(&self) -> Option<Status> {
        let mut g = lock(&self.inner.st);
        let r = g.running.as_mut()?;
        let alive = r.proc.exited().is_none();
        alive.then(|| Status {
            model: r.model.clone(),
            shape: r.shape.clone(),
            port: r.port,
            pid: r.proc.id(),
            load_secs: r.load_secs,
            on_gpu: r.on_gpu,
            busy: r.busy(),
        })
    }

    /// The end of the server's log (at most `max` bytes), for the planner.
    pub fn log_tail(&self, max: u64) -> String {
        self.inner.cfg.log.as_deref().and_then(|p| crate::ollama_runtime::read_tail(p, max).ok()).unwrap_or_default()
    }

    /// Whether the engine has `model` loaded.
    pub fn serves(&self, model: &str) -> bool {
        self.status().is_some_and(|s| same_model(&s.model, model))
    }

    /// Stops the server if it runs `model` (or anything, for `None`). Requests in flight fail.
    /// Returns whether a server was stopped.
    pub fn unload(&self, model: Option<&str>) -> bool {
        // Killed under the lock: a request that finds nothing running starts a load at once, and
        // that must not overlap a server still letting go of the GPU.
        let mut g = lock(&self.inner.st);
        let stopped = match g.running.take_if(|r| model.is_none_or(|m| same_model(&r.model, m))) {
            Some(mut r) => {
                self.inner.retire(&mut r);
                true
            }
            None => false,
        };
        drop(g);
        self.inner.cv.notify_all();
        stopped
    }

    /// Takes a slot for the request, loading (or switching) the model first when it must.
    pub fn acquire(&self, want: &Want, cancel: &CancelToken) -> Result<Lease, ProviderError> {
        let inner = &self.inner;
        let began = Instant::now();
        let mut g = lock(&inner.st);
        loop {
            if cancel.is_cancelled() {
                return Err(ProviderError::cancelled());
            }
            if g.running.as_mut().is_some_and(|r| r.proc.exited().is_some()) {
                // Crashed or killed from outside: the next request starts it again.
                g.running = None;
            }
            if g.loading.is_some() {
                g = self.wait(g, cancel);
                continue;
            }
            match g.running.as_mut() {
                Some(r) if r.matches(want) => {
                    if let Some(i) = r.pick(want.lane, want.key) {
                        let slot = &mut r.slots[i];
                        slot.busy = true;
                        slot.key = Some(want.key);
                        slot.last = Instant::now();
                        r.last_used = Instant::now();
                        return Ok(Lease {
                            inner: inner.clone(),
                            epoch: r.epoch,
                            endpoint: LocalEndpoint::new("127.0.0.1", r.port).expect("loopback"),
                            slot: i as u32,
                            sampling: r.sampling.clone(),
                            ctx_per_slot: r.shape.ctx_per_slot,
                        });
                    }
                    g = self.wait(g, cancel);
                }
                Some(r) => {
                    if r.busy() > 0 {
                        if began.elapsed() > SWITCH_WAIT {
                            return Err(ProviderError::server(format!(
                                "the llama.cpp server is busy serving {} and cannot switch to {} yet",
                                r.model, want.model
                            )));
                        }
                        g = self.wait(g, cancel);
                        continue;
                    }
                    // Under the lock, so nothing starts a second server while this one is dying.
                    let mut old = g.running.take().expect("checked");
                    inner.retire(&mut old);
                }
                None => {
                    if let Some((m, s, at, e)) = &g.failed {
                        if same_model(m, &want.model) && *s == want.shape && at.elapsed() < RETRY_AFTER_FAILURE {
                            return Err(e.clone());
                        }
                    }
                    g.loading = Some(want.model.clone());
                    g.epoch += 1;
                    let epoch = g.epoch;
                    drop(g);
                    let result = self.start(want, epoch, cancel);
                    g = lock(&inner.st);
                    g.loading = None;
                    match result {
                        Ok(r) => {
                            g.failed = None;
                            g.running = Some(r);
                        }
                        Err(e) => {
                            if !e.is_cancelled() {
                                g.failed = Some((want.model.clone(), want.shape.clone(), Instant::now(), e.clone()));
                            }
                            inner.cv.notify_all();
                            return Err(e);
                        }
                    }
                    inner.cv.notify_all();
                }
            }
        }
    }

    fn wait<'a>(&self, g: MutexGuard<'a, State>, cancel: &CancelToken) -> MutexGuard<'a, State> {
        let _ = cancel;
        self.inner.cv.wait_timeout(g, Duration::from_millis(100)).unwrap_or_else(|e| e.into_inner()).0
    }

    fn observer(&self) -> Option<Arc<dyn EngineObserver>> {
        self.inner.events.read().unwrap_or_else(|e| e.into_inner()).as_ref()?.engine_observer()
    }

    /// Asks Ollama to let go of `model`, so it is never in memory twice.
    fn make_room(&self, model: &str) {
        let ollama = &self.inner.cfg.ollama;
        let resident = |_: ()| {
            http::get_json(ollama, "/api/ps", QUICK)
                .map(|v| parse_ps(&v))
                .unwrap_or_default()
                .into_iter()
                .find(|p| same_model(&p.name, model))
        };
        let Some(p) = resident(()) else { return };
        let _ = http::post_json(ollama, "/api/generate", &unload_body(&p.name), Duration::from_secs(8));
        let until = Instant::now() + Duration::from_secs(12);
        while resident(()).is_some() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn start(&self, want: &Want, epoch: u64, cancel: &CancelToken) -> Result<Running, ProviderError> {
        let cfg = &self.inner.cfg;
        let detail = format!(
            "{} slot{} x {} tokens, kv {}",
            want.shape.slots,
            if want.shape.slots == 1 { "" } else { "s" },
            want.shape.ctx_per_slot,
            want.shape.kv
        );
        let _meter = self.observer().map(|o| o.loading(&want.model, &detail));
        let began = Instant::now();
        let resolved = ollama_store::resolve_in(&cfg.store, &want.model).map_err(|e| ProviderError::bad_request(e.remedy()))?;
        let binary = match &cfg.binary {
            Some(b) => b.clone(),
            None => launch::server_binary().map_err(|e| ProviderError::bad_request(e.remedy()))?,
        };
        self.make_room(&want.model);
        let backends = launch::backend_candidates(&binary);
        let tries: Vec<Option<PathBuf>> = if backends.is_empty() { vec![None] } else { backends.into_iter().map(Some).collect() };
        let mut why: Vec<String> = Vec::new();
        for backend in tries {
            if cancel.is_cancelled() {
                return Err(ProviderError::cancelled());
            }
            match self.try_start(&binary, backend.as_deref(), &resolved.blob, want, cancel) {
                Ok((proc, port, on_gpu)) => {
                    let n = usize::from(want.shape.slots.max(1));
                    let mut slots: Vec<Slot> = (0..n).map(|_| Slot { busy: false, key: None, last: Instant::now() }).collect();
                    // The conversations this model served before it was stopped read their caches back.
                    if let Some(dir) = &cfg.slots_dir {
                        let endpoint = LocalEndpoint::new("127.0.0.1", port).expect("loopback");
                        for (i, key) in persist::restore(&endpoint, dir, &want.model, &resolved.blob, &want.shape) {
                            if let Some(s) = slots.get_mut(i as usize) {
                                s.key = Some(key);
                            }
                        }
                    }
                    return Ok(Running {
                        model: want.model.clone(),
                        shape: want.shape.clone(),
                        port,
                        blob: resolved.blob.clone(),
                        proc,
                        epoch,
                        load_secs: began.elapsed().as_secs_f64(),
                        last_used: Instant::now(),
                        on_gpu,
                        sampling: resolved.sampling.clone(),
                        slots,
                    });
                }
                Err(Tried::Next(m)) => why.push(m),
                Err(Tried::Fatal(e)) => return Err(e),
            }
        }
        Err(ProviderError::server(format!("llama-server for {} did not start on the GPU: {}", want.model, why.join("; "))))
    }

    fn try_start(
        &self,
        binary: &Path,
        backend: Option<&Path>,
        blob: &Path,
        want: &Want,
        cancel: &CancelToken,
    ) -> Result<(Box<dyn Process>, u16, Option<(u32, u32)>), Tried> {
        let cfg = &self.inner.cfg;
        let io = |e: std::io::Error| Tried::Fatal(ProviderError::server(format!("could not start llama-server: {e}")));
        let port = self.inner.spawner.free_port().map_err(io)?;
        if let Some(dir) = &cfg.slots_dir {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        let spec = Spec {
            blob: blob.to_path_buf(),
            port,
            slots: want.shape.slots.max(1),
            ctx_per_slot: want.shape.ctx_per_slot,
            kv: want.shape.kv.clone(),
            ubatch: cfg.ubatch,
            slot_save: cfg.slots_dir.clone(),
        };
        let plan = launch::plan(binary, backend, &spec);
        // The same line Ollama logs for its runners, so the planner reads this load like any other.
        if let Some(path) = cfg.log.as_deref() {
            if let Ok(mut f) = crate::ollama_log::open_own_log(path, crate::ollama_log::OWN_LOG_CAP) {
                use std::io::Write as _;
                let _ = writeln!(f, "msg=\"starting llama-server\" cmd=\"{} {}\"", plan.binary.display(), plan.args.join(" "));
            }
        }
        let log_from = cfg.log.as_deref().and_then(|p| std::fs::metadata(p).ok()).map_or(0, |m| m.len());
        let mut proc = self.inner.spawner.start(&plan, cfg.log.as_deref()).map_err(io)?;
        let endpoint = LocalEndpoint::new("127.0.0.1", port).expect("loopback");
        let deadline = Instant::now() + cfg.start_timeout;
        loop {
            if cancel.is_cancelled() {
                proc.kill();
                return Err(Tried::Fatal(ProviderError::cancelled()));
            }
            if let Some(code) = proc.exited() {
                let tail = self.log_excerpt(log_from);
                return Err(Tried::Fatal(ProviderError::server(format!(
                    "llama-server exited with code {code} while loading {}{}",
                    want.model,
                    if tail.is_empty() { String::new() } else { format!(": {tail}") }
                ))));
            }
            if http::get_json(&endpoint, "/health", QUICK).is_ok() {
                break;
            }
            if Instant::now() > deadline {
                proc.kill();
                return Err(Tried::Fatal(ProviderError::server(format!(
                    "llama-server did not finish loading {} within {} s",
                    want.model,
                    cfg.start_timeout.as_secs()
                ))));
            }
            std::thread::sleep(cfg.poll);
        }
        let text = cfg.log.as_deref().map(|p| log_since(p, log_from)).unwrap_or_default();
        let on_gpu = offloaded(&text).map(|o| (o.on_gpu, o.total));
        // A backend that did not load leaves a healthy server on the CPU, ten times slower.
        if backend.is_some() && on_gpu.is_some_and(|(n, _)| n == 0) {
            proc.kill();
            return Err(Tried::Next(format!("{} loaded no layers on the GPU", backend.map(|b| b.display().to_string()).unwrap_or_default())));
        }
        Ok((proc, port, on_gpu))
    }

    fn log_excerpt(&self, from: u64) -> String {
        let Some(path) = self.inner.cfg.log.as_deref() else { return String::new() };
        let text = log_since(path, from);
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let tail = lines[lines.len().saturating_sub(4)..].join(" | ");
        tail.chars().take(500).collect()
    }
}

enum Tried {
    /// This backend did not work; try the next.
    Next(String),
    Fatal(ProviderError),
}

/// The part of a log file after byte `from` (the whole file if it was rotated).
pub(crate) fn log_since(path: &Path, from: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else { return String::new() };
    let len = f.metadata().map_or(0, |m| m.len());
    let start = if len < from { 0 } else { from };
    if f.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    let _ = f.take(8 << 20).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

fn idle_loop(weak: Weak<Inner>, tick: Duration) {
    loop {
        std::thread::sleep(tick);
        let Some(inner) = weak.upgrade() else { return };
        let Some(keep) = inner.cfg.keep_alive else { continue };
        let expired = {
            let mut g = lock(&inner.st);
            let mut gone = g.running.take_if(|r| r.busy() == 0 && r.last_used.elapsed() >= keep);
            if let Some(r) = gone.as_mut() {
                inner.retire(r);
            }
            gone
        };
        if let Some(r) = expired {
            if let Some(o) = inner.events.read().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(ContextAllocations::engine_observer) {
                o.note(&format!("llama.cpp unloaded {} after {} idle", r.model, humanize(keep)));
            }
        }
    }
}

fn humanize(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 && s % 3600 == 0 {
        format!("{} h", s / 3600)
    } else if s >= 60 && s % 60 == 0 {
        format!("{} min", s / 60)
    } else {
        format!("{s} s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llama::testkit::*;

    fn want(model: &str, lane: Lane, key: u64) -> Want {
        Want { model: model.into(), shape: EngineShape { slots: 3, ctx_per_slot: 8192, kv: "q8_0".into() }, lane, key }
    }

    #[test]
    fn keep_alive_reads_ollamas_spellings() {
        assert_eq!(keep_alive_from(None), Some(Duration::from_secs(300)));
        assert_eq!(keep_alive_from(Some("10m")), Some(Duration::from_secs(600)));
        assert_eq!(keep_alive_from(Some("90")), Some(Duration::from_secs(90)));
        assert_eq!(keep_alive_from(Some("1h")), Some(Duration::from_secs(3600)));
        assert_eq!(keep_alive_from(Some("-1")), None);
        assert_eq!(keep_alive_from(Some("0")), Some(Duration::ZERO));
    }

    #[test]
    fn the_conductor_keeps_slot_zero_and_each_conversation_keeps_its_slot() {
        let fx = Fixture::new("dusk:22b");
        let e = fx.engine();
        let c = CancelToken::new();
        let a = e.acquire(&want("dusk:22b", Lane::Conductor, 1), &c).unwrap();
        assert_eq!(a.slot, 0);
        // Crew conversations never take slot 0, and come back to the slot they used.
        let x = e.acquire(&want("dusk:22b", Lane::Other, 10), &c).unwrap();
        let y = e.acquire(&want("dusk:22b", Lane::Other, 11), &c).unwrap();
        assert_eq!((x.slot, y.slot), (1, 2));
        let xs = x.slot;
        drop(x);
        let again = e.acquire(&want("dusk:22b", Lane::Other, 10), &c).unwrap();
        assert_eq!(again.slot, xs, "the same conversation returns to its own cache");
        // A third crew conversation takes the free slot, not the conductor's.
        drop(y);
        let z = e.acquire(&want("dusk:22b", Lane::Other, 12), &c).unwrap();
        assert_ne!(z.slot, 0);
        assert_eq!(fx.spawner.starts(), 1, "one server for all of them");
    }

    #[test]
    fn the_conductor_never_waits_for_its_slot() {
        let fx = Fixture::new("dusk:22b");
        let e = fx.engine();
        let c = CancelToken::new();
        let first = e.acquire(&want("dusk:22b", Lane::Conductor, 1), &c).unwrap();
        let second = e.acquire(&want("dusk:22b", Lane::Conductor, 2), &c).unwrap();
        assert_eq!(first.slot, 0);
        assert_ne!(second.slot, 0, "another session's conductor turn takes a free slot instead of queueing");
    }

    #[test]
    fn a_crew_request_waits_for_a_slot_and_a_cancel_ends_the_wait() {
        let fx = Fixture::new("dusk:22b");
        let e = fx.engine();
        let c = CancelToken::new();
        let _a = e.acquire(&want("dusk:22b", Lane::Other, 1), &c).unwrap();
        let _b = e.acquire(&want("dusk:22b", Lane::Other, 2), &c).unwrap();
        // Slots 1 and 2 are taken; slot 0 is the conductor's.
        let waiting = CancelToken::new();
        let w2 = waiting.clone();
        let e2 = e.clone();
        let t = std::thread::spawn(move || e2.acquire(&want("dusk:22b", Lane::Other, 3), &w2).err().map(|e| e.is_cancelled()));
        std::thread::sleep(Duration::from_millis(250));
        waiting.cancel();
        assert_eq!(t.join().unwrap(), Some(true));
    }

    #[test]
    fn loading_asks_ollama_to_unload_the_model_first() {
        let fx = Fixture::new("dusk:22b");
        fx.ollama.resident("dusk:22b");
        let e = fx.engine();
        e.acquire(&want("dusk:22b", Lane::Conductor, 1), &CancelToken::new()).unwrap();
        assert_eq!(fx.ollama.unloaded(), vec!["dusk:22b".to_string()]);
    }

    #[test]
    fn a_server_that_dies_while_loading_is_an_error_naming_the_log() {
        let fx = Fixture::new("dusk:22b");
        fx.spawner.die_at_start("ggml_cuda: failed to allocate CUDA0 buffer of size 14000 MiB");
        let e = fx.engine();
        let err = e.acquire(&want("dusk:22b", Lane::Conductor, 1), &CancelToken::new()).err().unwrap();
        assert!(err.detail.contains("exited") && err.detail.contains("failed to allocate"), "{}", err.detail);
        // Asking again at once repeats the answer instead of starting another doomed process.
        let again = e.acquire(&want("dusk:22b", Lane::Conductor, 1), &CancelToken::new()).err().unwrap();
        assert_eq!(again.detail, err.detail);
        assert_eq!(fx.spawner.starts(), 1);
    }

    #[test]
    fn a_new_shape_restarts_the_server_after_the_old_requests_finish() {
        let fx = Fixture::new("dusk:22b");
        let e = fx.engine();
        let c = CancelToken::new();
        let a = e.acquire(&want("dusk:22b", Lane::Conductor, 1), &c).unwrap();
        drop(a);
        let mut w = want("dusk:22b", Lane::Conductor, 1);
        w.shape.slots = 2;
        let b = e.acquire(&w, &c).unwrap();
        assert_eq!(fx.spawner.starts(), 2);
        assert_eq!(fx.spawner.killed(), 1);
        assert_eq!(e.status().unwrap().shape.slots, 2);
        drop(b);
    }

    #[test]
    fn a_stop_writes_each_conversations_cache_and_the_next_load_gives_it_back_on_its_own_slot() {
        let fx = Fixture::new("dusk:22b");
        let mut cfg = fx.config();
        cfg.slots_dir = Some(fx.dir.path().join("slots"));
        let e = LlamaEngine::new(cfg.clone(), fx.spawner.clone());
        let c = CancelToken::new();
        let a = e.acquire(&want("dusk:22b", Lane::Conductor, 11), &c).unwrap();
        let x = e.acquire(&want("dusk:22b", Lane::Other, 22), &c).unwrap();
        let (a_slot, x_slot) = (a.slot, x.slot);
        drop((a, x));
        assert!(e.unload(None));
        let calls = fx.spawner.seen.slot_calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(calls.iter().all(|c| c.starts_with("save ")), "{calls:?}");
        // A new engine (the next run of Marlowe) loads the model again and restores both.
        let again = LlamaEngine::new(cfg, fx.spawner.clone());
        let a2 = again.acquire(&want("dusk:22b", Lane::Conductor, 11), &c).unwrap();
        let x2 = again.acquire(&want("dusk:22b", Lane::Other, 22), &c).unwrap();
        assert_eq!((a2.slot, x2.slot), (a_slot, x_slot), "each conversation is on the slot that holds its cache");
        let calls = fx.spawner.seen.slot_calls.lock().unwrap().clone();
        assert_eq!(calls.iter().filter(|c| c.starts_with("restore ")).count(), 2, "{calls:?}");
        // A different shape is a different cache: nothing is restored into it.
        drop((a2, x2));
        assert!(again.unload(None));
        let mut other = want("dusk:22b", Lane::Conductor, 11);
        other.shape.slots = 2;
        let before = fx.spawner.seen.slot_calls.lock().unwrap().len();
        again.acquire(&other, &c).unwrap();
        assert!(fx.spawner.seen.slot_calls.lock().unwrap()[before..].iter().all(|c| !c.starts_with("restore ")));
    }

    #[test]
    fn an_idle_server_is_stopped_after_the_keep_alive() {
        let fx = Fixture::new("dusk:22b");
        let mut cfg = fx.config();
        cfg.keep_alive = Some(Duration::from_millis(150));
        cfg.idle_tick = Duration::from_millis(40);
        let e = LlamaEngine::new(cfg, fx.spawner.clone());
        let lease = e.acquire(&want("dusk:22b", Lane::Conductor, 1), &CancelToken::new()).unwrap();
        std::thread::sleep(Duration::from_millis(400));
        assert!(e.status().is_some(), "a request in flight keeps it");
        drop(lease);
        let until = Instant::now() + Duration::from_secs(3);
        while e.status().is_some() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(40));
        }
        assert!(e.status().is_none());
        assert_eq!(fx.spawner.killed(), 1);
    }

    #[derive(Default)]
    struct Heard {
        lines: Mutex<Vec<String>>,
    }

    struct Guard(Arc<Heard>, String);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.lines.lock().unwrap().push(format!("loaded {}", self.1));
        }
    }

    struct Ears(Arc<Heard>);
    impl EngineObserver for Ears {
        fn loading(&self, model: &str, detail: &str) -> Box<dyn Send> {
            self.0.lines.lock().unwrap().push(format!("loading {model} ({detail})"));
            Box::new(Guard(self.0.clone(), model.to_string()))
        }
        fn note(&self, line: &str) {
            self.0.lines.lock().unwrap().push(line.to_string());
        }
    }

    #[test]
    fn a_load_is_a_live_line_while_it_lasts_and_an_idle_unload_is_said() {
        let fx = Fixture::new("dusk:22b");
        let mut cfg = fx.config();
        cfg.keep_alive = Some(Duration::from_millis(100));
        cfg.idle_tick = Duration::from_millis(30);
        let e = LlamaEngine::new(cfg, fx.spawner.clone());
        let heard = Arc::new(Heard::default());
        let alloc = ContextAllocations::new();
        alloc.set_engine_observer(Arc::new(Ears(heard.clone())));
        e.follow(alloc);
        drop(e.acquire(&want("dusk:22b", Lane::Conductor, 1), &CancelToken::new()).unwrap());
        let until = Instant::now() + Duration::from_secs(3);
        while e.status().is_some() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(30));
        }
        let lines = heard.lines.lock().unwrap().clone();
        assert_eq!(lines[0], "loading dusk:22b (3 slots x 8192 tokens, kv q8_0)");
        assert_eq!(lines[1], "loaded dusk:22b", "the guard drops when the load ends");
        assert!(lines[2].starts_with("llama.cpp unloaded dusk:22b after "), "{lines:?}");
    }

    #[test]
    fn unload_stops_the_named_model_only() {
        let fx = Fixture::new("dusk:22b");
        let e = fx.engine();
        let l = e.acquire(&want("dusk:22b", Lane::Conductor, 1), &CancelToken::new()).unwrap();
        drop(l);
        assert!(!e.unload(Some("other:1")));
        assert!(e.serves("dusk:22b"));
        assert!(e.unload(Some("dusk:22b")));
        assert!(!e.serves("dusk:22b"));
    }
}
