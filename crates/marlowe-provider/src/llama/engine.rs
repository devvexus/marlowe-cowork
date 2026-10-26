//! Marlowe's own llama-server: one process, one model, several slots.
//!
//! The engine starts Ollama's bundled `llama-server.exe` on demand for a model the planner
//! assigned to it, serves every request for that model on one of its slots, and stops it after
//! the keep-alive Ollama would have used. It never runs a model Ollama also has loaded: before a
//! load it asks Ollama to unload the same model (`keep_alive: 0`), and the other models Ollama
//! holds too when the card is short of what the load needs.
//!
//! # Slots
//!
//! A slot is one conversation's prompt cache. The conductor's conversation keeps slot 0, so crew
//! and side calls never evict it; every other conversation sticks to the slot it used last (found
//! by a hash of its system prompt and first message) and otherwise takes a free one, least
//! recently used first. A request waits when every slot it may use is busy: the crew is bounded
//! by the same count, so it rarely does. The conductor never waits for slot 0: when another
//! request holds it, it takes any free slot, unless the slot is reading ahead for that very
//! conversation ([`super::warm`]): its next request then follows the read.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock, Weak};
use std::time::{Duration, Instant};

use marlowe_model::{CancelToken, ContextAllocations, EngineObserver, EngineShape, Lane, ProviderError};

use super::launch::{self, Spec};
use super::persist;
use super::server::{offloaded, Process, Spawner};
use super::template::{self, Thinking, ThinkingCache};
use super::warm;
use crate::http::{self, LocalEndpoint};
use crate::ollama_runtime::{parse_ps, unload_body};
use crate::ollama_store::{self, Sampling};

/// How long a request waits for a model switch to drain before it gives up.
const SWITCH_WAIT: Duration = Duration::from_secs(300);
/// A load that failed this recently is not tried again for the same shape.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(15);
const QUICK: Duration = Duration::from_millis(1500);

/// What the spill check tolerates, MiB: weights missing from dedicated memory beyond this are
/// somewhere else.
const SPILL_SLACK_MIB: f64 = 1024.0;
/// Shared (system) memory a healthy server may hold as pinned host buffers, MiB.
const PINNED_ALLOWANCE_MIB: f64 = 1536.0;
const MIB: f64 = 1024.0 * 1024.0;

/// What the engine can read of the GPU. Under the Windows driver's sysmem fallback `--fit off`
/// does not fail when the card is full: the driver pages the model into system memory and the
/// server loads, slowly. So the engine looks before it starts a server and again after it loaded.
pub trait VramProbe: Send + Sync + std::fmt::Debug {
    /// Free dedicated GPU memory now, MiB. `None`: there is no reading.
    fn free_mib(&self) -> Option<f64>;
    /// Dedicated and shared (system) GPU memory process `pid` holds, MiB. `None`: no reading.
    fn process_mib(&self, pid: u32) -> Option<(f64, f64)>;
}

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
    /// Where what each model file's template does with earlier thinking is kept; `None` asks again
    /// at every start of the engine.
    pub thinking_cache: Option<PathBuf>,
    /// The server binary; `None` finds the one Ollama bundles.
    pub binary: Option<PathBuf>,
    /// How often a starting server is asked whether it is ready.
    pub poll: Duration,
    /// How often the idle check runs.
    pub idle_tick: Duration,
    /// The GPU, read before a server starts (does the model fit in the memory free) and after it
    /// loaded (did the driver spill it into system memory). A server starts without the checks
    /// only when this is `None`; with a probe, no reading is not "fits".
    pub vram: Option<Arc<dyn VramProbe>>,
    /// How long the free memory may take to show what Ollama just unloaded before a misfit is final.
    pub vram_settle: Duration,
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
            thinking_cache: None,
            binary: None,
            poll: Duration::from_millis(250),
            idle_tick: Duration::from_secs(2),
            vram: None,
            vram_settle: Duration::from_secs(2),
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
    /// GPU memory the planner expects the shape to take, MiB; `None` when it has no estimate (the
    /// check at the start then counts the model file alone).
    pub need_mib: Option<f64>,
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
    /// Reading the next request's prefix for the conversation `key`: that conversation's next
    /// request waits for it, any other may not take the slot.
    warming: bool,
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
    /// What the model file's template does with earlier thinking.
    thinking: Thinking,
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
        // The conversation's own cache is being brought up to date: wait for it, never go
        // around it to a slot that holds something else.
        if self.slots.iter().any(|s| s.warming && s.key == Some(key)) {
            return None;
        }
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
    /// The model the engine would not run, and why (until it loads).
    declined: Option<(String, String)>,
}

/// Why a start produced no server.
enum Failure {
    Error(ProviderError),
    /// The engine will not run this model now; Ollama must. The reason reads after
    /// "<model> runs on Ollama:".
    Declined(String),
}

impl From<ProviderError> for Failure {
    fn from(e: ProviderError) -> Self {
        Failure::Error(e)
    }
}

struct Inner {
    cfg: EngineConfig,
    spawner: Arc<dyn Spawner>,
    st: Mutex<State>,
    cv: Condvar,
    events: RwLock<Option<ContextAllocations>>,
    thinking: ThinkingCache,
    reads: Mutex<ReadAheads>,
}

/// How the background reads of the next request's prefix went.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadAheads {
    pub started: u32,
    pub finished: u32,
    pub failed: u32,
    /// The server's last complaint.
    pub last_error: Option<String>,
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
    /// What the model file's template does with earlier thinking.
    pub thinking: Thinking,
}

impl Lease {
    /// Keeps the slot and reads `prefix` into it in the background, then lets it go. The
    /// conversation's next request waits for the read instead of going to another slot.
    pub fn read_ahead(self, prefix: String) {
        {
            let mut g = lock(&self.inner.st);
            if let Some(s) = g.running.as_mut().filter(|r| r.epoch == self.epoch).and_then(|r| r.slots.get_mut(self.slot as usize)) {
                s.warming = true;
            }
        }
        lock(&self.inner.reads).started += 1;
        // If the thread cannot start, the closure is dropped with the lease and the slot is free.
        let _ = std::thread::Builder::new().name("llama-read-ahead".into()).spawn(move || {
            let result = warm::read_ahead(&self.endpoint, self.slot, &prefix);
            let inner = self.inner.clone();
            let failed = {
                let mut reads = lock(&inner.reads);
                match &result {
                    Ok(()) => reads.finished += 1,
                    Err(e) => {
                        reads.failed += 1;
                        reads.last_error = Some(e.clone());
                    }
                }
                result.err().filter(|_| reads.failed == 1)
            };
            drop(self);
            if let Some(e) = failed {
                // Once is enough: the next request simply reads what it needs, as before.
                if let Some(o) = inner.events.read().unwrap_or_else(|p| p.into_inner()).as_ref().and_then(ContextAllocations::engine_observer) {
                    o.note(&format!("llama.cpp could not read the next prompt ahead of time: {e}"));
                }
            }
        });
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut g = lock(&self.inner.st);
        if let Some(r) = g.running.as_mut().filter(|r| r.epoch == self.epoch) {
            if let Some(s) = r.slots.get_mut(self.slot as usize) {
                s.busy = false;
                s.warming = false;
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
        let thinking = ThinkingCache::new(cfg.thinking_cache.clone());
        let inner = Arc::new(Inner {
            cfg,
            spawner,
            st: Mutex::new(State::default()),
            cv: Condvar::new(),
            events: RwLock::new(None),
            thinking,
            reads: Mutex::new(ReadAheads::default()),
        });
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

    /// How the background reads of the next request's prefix have gone.
    pub fn read_aheads(&self) -> ReadAheads {
        lock(&self.inner.reads).clone()
    }

    /// What the loaded model's template does with earlier thinking, if a model is loaded.
    pub fn thinking(&self) -> Option<Thinking> {
        lock(&self.inner.st).running.as_ref().map(|r| r.thinking.clone())
    }

    /// The end of the server's log (at most `max` bytes), for the planner.
    pub fn log_tail(&self, max: u64) -> String {
        self.inner.cfg.log.as_deref().and_then(|p| crate::ollama_runtime::read_tail(p, max).ok()).unwrap_or_default()
    }

    /// Why the engine would not run `model` (it did not fit, or the driver spilled it), if that is
    /// why the last start failed: the caller serves the request on Ollama.
    pub fn declined(&self, model: &str) -> Option<String> {
        lock(&self.inner.st).declined.as_ref().filter(|(m, _)| same_model(m, model)).map(|(_, why)| why.clone())
    }

    /// Whether a server for exactly this model and shape is up: a request for it starts nothing.
    pub fn runs(&self, want: &Want) -> bool {
        self.status().is_some_and(|s| same_model(&s.model, &want.model) && s.shape == want.shape)
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
                            thinking: r.thinking.clone(),
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
                    g.declined = None;
                    g.epoch += 1;
                    let epoch = g.epoch;
                    drop(g);
                    let result = self.start(want, epoch, cancel);
                    g = lock(&inner.st);
                    g.loading = None;
                    match result {
                        Ok(r) => {
                            g.failed = None;
                            g.declined = None;
                            g.running = Some(r);
                        }
                        Err(f) => {
                            let (e, why) = match f {
                                Failure::Error(e) => (e, None),
                                Failure::Declined(why) => (ProviderError::server(format!("{} runs on Ollama: {why}", want.model)), Some(why)),
                            };
                            if !e.is_cancelled() {
                                g.failed = Some((want.model.clone(), want.shape.clone(), Instant::now(), e.clone()));
                            }
                            if let Some(why) = &why {
                                g.declined = Some((want.model.clone(), why.clone()));
                            }
                            drop(g);
                            inner.cv.notify_all();
                            if let Some(why) = why {
                                self.say_declined(&want.model, &why);
                            }
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

    /// The advisor keeps the model on Ollama from here on, and the user is told in one line.
    fn say_declined(&self, model: &str, why: &str) {
        if let Some(a) = self.inner.events.read().unwrap_or_else(|e| e.into_inner()).as_ref() {
            a.engine_declined(model, why);
        }
        if let Some(o) = self.observer() {
            o.note(&format!("{model} runs on Ollama: {why}"));
        }
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

    /// Asks Ollama to let go of every model it holds but `model`: the planner counts what
    /// Ollama's runners hold as free for a load, so a start that finds the card short of it makes
    /// that true, as Ollama itself would for a model it loads. Returns the models it unloaded.
    fn release_ollama(&self, model: &str) -> Vec<String> {
        let ollama = &self.inner.cfg.ollama;
        let held = |_: ()| -> Vec<String> {
            http::get_json(ollama, "/api/ps", QUICK).map(|v| parse_ps(&v)).unwrap_or_default().into_iter().map(|p| p.name).collect()
        };
        let others: Vec<String> = held(()).into_iter().filter(|n| !same_model(n, model)).collect();
        for name in &others {
            let _ = http::post_json(ollama, "/api/generate", &unload_body(name), Duration::from_secs(8));
        }
        let until = Instant::now() + Duration::from_secs(12);
        while held(()).iter().any(|n| others.contains(n)) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(200));
        }
        others
    }

    /// Whether the GPU has room for the model now, measured after Ollama let go of it. A model
    /// that does not fit is not started: with the driver's sysmem fallback it would load anyway,
    /// into system memory, and read at a few tokens a second. No reading is not "fits".
    fn check_room(&self, want: &Want, blob: &Path) -> Result<(), Failure> {
        let cfg = &self.inner.cfg;
        let Some(probe) = &cfg.vram else { return Ok(()) };
        let weights = std::fs::metadata(blob).map_or(0.0, |m| m.len() as f64 / MIB);
        let need = want.need_mib.unwrap_or(0.0).max(weights + SPILL_SLACK_MIB / 2.0);
        let until = Instant::now() + cfg.vram_settle;
        let mut free = probe.free_mib();
        if free.is_some_and(|f| f < need) {
            // Short of room: the planner counted what Ollama holds for another model as free.
            let freed = self.release_ollama(&want.model);
            if !freed.is_empty() {
                if let Some(o) = self.observer() {
                    o.note(&format!("Ollama let go of {} to make room for {}", freed.join(", "), want.model));
                }
                free = probe.free_mib();
            }
        }
        while free.is_none_or(|f| f < need) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(200).min(cfg.vram_settle));
            free = probe.free_mib();
        }
        match free {
            Some(f) if f >= need => Ok(()),
            Some(f) => Err(Failure::Declined(format!("needs {} GiB on the GPU, {} GiB free", gib(need), gib(f)))),
            None => Err(Failure::Declined("there is no reading of the GPU's free memory to check the load against".into())),
        }
    }

    /// Whether the driver paged the loaded model into system memory: the server holds shared
    /// memory well past its pinned buffers, or far less dedicated memory than the model's weights.
    /// `None` when it did not, or when the GPU cannot be read for the process.
    fn spilled(&self, pid: u32, blob: &Path) -> Option<String> {
        let probe = self.inner.cfg.vram.as_ref()?;
        let (dedicated, shared) = probe.process_mib(pid)?;
        // A server that has loaded a model holds some memory: nothing at all is a counter that
        // could not be read (or has no instance yet), and says nothing about a spill.
        if dedicated <= 0.0 && shared <= 0.0 {
            return None;
        }
        let weights = std::fs::metadata(blob).map_or(0.0, |m| m.len() as f64 / MIB);
        let missing = (weights - dedicated).max(0.0);
        if shared <= PINNED_ALLOWANCE_MIB && missing <= SPILL_SLACK_MIB {
            return None;
        }
        let moved = if shared > PINNED_ALLOWANCE_MIB { shared } else { missing };
        Some(format!(
            "the driver moved {} GiB of it into system memory, where it reads at a fraction of the speed",
            gib(moved)
        ))
    }

    fn start(&self, want: &Want, epoch: u64, cancel: &CancelToken) -> Result<Running, Failure> {
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
        let resolved = ollama_store::resolve_in(&cfg.store, &want.model).map_err(|e| Failure::from(ProviderError::bad_request(e.remedy())))?;
        let binary = match &cfg.binary {
            Some(b) => b.clone(),
            None => launch::server_binary().map_err(|e| Failure::from(ProviderError::bad_request(e.remedy())))?,
        };
        self.make_room(&want.model);
        self.check_room(want, &resolved.blob)?;
        let backends = launch::backend_candidates(&binary);
        let tries: Vec<Option<PathBuf>> = if backends.is_empty() { vec![None] } else { backends.into_iter().map(Some).collect() };
        let mut why: Vec<String> = Vec::new();
        for backend in tries {
            if cancel.is_cancelled() {
                return Err(ProviderError::cancelled().into());
            }
            match self.try_start(&binary, backend.as_deref(), &resolved.blob, want, cancel) {
                Ok((proc, port, on_gpu)) => {
                    let n = usize::from(want.shape.slots.max(1));
                    let mut slots: Vec<Slot> = (0..n).map(|_| Slot { busy: false, warming: false, key: None, last: Instant::now() }).collect();
                    let endpoint = LocalEndpoint::new("127.0.0.1", port).expect("loopback");
                    let thinking = self.thinking_of(&endpoint, want, &resolved.blob);
                    // The conversations this model served before it was stopped read their caches back.
                    if let Some(dir) = &cfg.slots_dir {
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
                        thinking,
                    });
                }
                Err(Tried::Next(m)) => why.push(m),
                Err(Tried::Fatal(e)) => return Err(e.into()),
                Err(Tried::Declined(why)) => return Err(Failure::Declined(why)),
            }
        }
        Err(ProviderError::server(format!("llama-server for {} did not start on the GPU: {}", want.model, why.join("; "))).into())
    }

    /// What the template of this model file does with earlier thinking: asked of the loaded server
    /// once per file, remembered by the file's digest.
    fn thinking_of(&self, endpoint: &LocalEndpoint, want: &Want, blob: &Path) -> Thinking {
        let digest = template::digest_of(blob);
        if let Some(t) = self.inner.thinking.get(&digest) {
            return t;
        }
        let t = template::probe(endpoint);
        self.inner.thinking.put(&digest, &t);
        if t != Thinking::Unknown {
            if let Some(o) = self.observer() {
                o.note(&format!("{}: {}", want.model, t.describe()));
            }
        }
        t
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
        // `--fit off` does not fail when the card is full under the Windows driver's sysmem
        // fallback: the server is healthy, and slow. The weights are resident now, so look.
        if let Some(why) = self.spilled(proc.id(), blob) {
            proc.kill();
            return Err(Tried::Declined(why));
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
    /// The server loaded, and the driver spilled the model into system memory: Ollama runs it.
    Declined(String),
}

/// `13.6` (GiB, one decimal) from MiB.
fn gib(mib: f64) -> String {
    format!("{:.1}", mib / 1024.0)
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
        Want { model: model.into(), shape: EngineShape { slots: 3, ctx_per_slot: 8192, kv: "q8_0".into() }, lane, key, need_mib: None }
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

    /// An advisor that only records what the engine tells it.
    #[derive(Default)]
    struct Told(Mutex<Vec<String>>);
    impl marlowe_model::EngineAdvisor for Told {
        fn shape(&self, _: &str) -> Option<EngineShape> {
            None
        }
        fn declined(&self, model_id: &str, why: &str) {
            self.0.lock().unwrap().push(format!("{model_id}: {why}"));
        }
    }

    /// An engine with a fake GPU, an advisor and an observer listening.
    fn with_gpu(fx: &Fixture, free: Option<f64>) -> (LlamaEngine, Arc<FakeVram>, Arc<Told>, Arc<Heard>) {
        let vram = FakeVram::new(free);
        let mut cfg = fx.config();
        cfg.vram = Some(vram.clone());
        cfg.vram_settle = Duration::ZERO;
        let e = LlamaEngine::new(cfg, fx.spawner.clone());
        let (told, heard) = (Arc::new(Told::default()), Arc::new(Heard::default()));
        let alloc = ContextAllocations::new();
        alloc.set_engine_advisor(told.clone());
        alloc.set_engine_observer(Arc::new(Ears(heard.clone())));
        e.follow(alloc);
        (e, vram, told, heard)
    }

    #[test]
    fn a_model_bigger_than_the_free_gpu_is_not_started_and_ollama_is_told() {
        let fx = Fixture::new("dusk:22b");
        // 13.6 GiB planned, 11.9 GiB free: llama-server would load it anyway, into system memory.
        let (e, _vram, told, heard) = with_gpu(&fx, Some(11.9 * 1024.0));
        let mut w = want("dusk:22b", Lane::Conductor, 1);
        w.need_mib = Some(13.6 * 1024.0);
        let err = e.acquire(&w, &CancelToken::new()).err().expect("refused");
        assert_eq!(err.detail, "dusk:22b runs on Ollama: needs 13.6 GiB on the GPU, 11.9 GiB free");
        assert_eq!(fx.spawner.starts(), 0, "no server was started");
        assert_eq!(e.declined("dusk:22b").as_deref(), Some("needs 13.6 GiB on the GPU, 11.9 GiB free"));
        assert_eq!(told.0.lock().unwrap().clone(), vec!["dusk:22b: needs 13.6 GiB on the GPU, 11.9 GiB free".to_string()]);
        assert_eq!(heard.lines.lock().unwrap().last().map(String::as_str), Some("dusk:22b runs on Ollama: needs 13.6 GiB on the GPU, 11.9 GiB free"));
    }

    #[test]
    fn no_gpu_reading_does_not_start_a_server() {
        let fx = Fixture::new("dusk:22b");
        let (e, _vram, told, _) = with_gpu(&fx, None);
        let err = e.acquire(&want("dusk:22b", Lane::Conductor, 1), &CancelToken::new()).err().expect("refused");
        assert!(err.detail.contains("no reading of the GPU"), "{}", err.detail);
        assert_eq!(fx.spawner.starts(), 0);
        assert_eq!(told.0.lock().unwrap().len(), 1);
    }

    #[test]
    fn with_room_on_the_gpu_the_server_starts() {
        let fx = Fixture::new("dusk:22b");
        let (e, _vram, told, _) = with_gpu(&fx, Some(20_000.0));
        let mut w = want("dusk:22b", Lane::Conductor, 1);
        w.need_mib = Some(13.6 * 1024.0);
        e.acquire(&w, &CancelToken::new()).unwrap();
        assert_eq!(fx.spawner.starts(), 1);
        assert!(told.0.lock().unwrap().is_empty() && e.declined("dusk:22b").is_none());
    }

    #[test]
    fn memory_ollama_just_let_go_of_gets_a_moment_to_show_as_free() {
        let fx = Fixture::new("dusk:22b");
        let vram = FakeVram::new(Some(2_000.0));
        let mut cfg = fx.config();
        cfg.vram = Some(vram.clone());
        cfg.vram_settle = Duration::from_secs(5);
        let e = LlamaEngine::new(cfg, fx.spawner.clone());
        let v = vram.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            *v.free.lock().unwrap() = Some(20_000.0);
        });
        let mut w = want("dusk:22b", Lane::Conductor, 1);
        w.need_mib = Some(13.6 * 1024.0);
        e.acquire(&w, &CancelToken::new()).unwrap();
        assert_eq!(fx.spawner.starts(), 1);
    }

    #[test]
    fn a_model_the_driver_paged_into_system_memory_is_stopped_and_handed_to_ollama() {
        let fx = Fixture::new("dusk:22b");
        let (e, vram, told, heard) = with_gpu(&fx, Some(20_000.0));
        // The server is healthy and holds 4 GiB of shared memory beyond its pinned buffers.
        *vram.process.lock().unwrap() = Some((11_000.0, 4_500.0));
        let err = e.acquire(&want("dusk:22b", Lane::Conductor, 1), &CancelToken::new()).err().expect("refused");
        assert!(err.detail.starts_with("dusk:22b runs on Ollama: the driver moved 4.4 GiB of it into system memory"), "{}", err.detail);
        assert_eq!((fx.spawner.starts(), fx.spawner.killed()), (1, 1), "the server was stopped");
        assert!(e.status().is_none());
        assert_eq!(told.0.lock().unwrap().len(), 1);
        assert!(heard.lines.lock().unwrap().iter().any(|l| l.starts_with("dusk:22b runs on Ollama: the driver moved")));
    }

    /// A card whose free memory is what is left while Ollama holds any model.
    #[derive(Debug)]
    struct HeldByOllama {
        ollama: LocalEndpoint,
        free_when_held: f64,
        free_when_not: f64,
    }
    impl VramProbe for HeldByOllama {
        fn free_mib(&self) -> Option<f64> {
            let held = http::get_json(&self.ollama, "/api/ps", QUICK).map(|v| !parse_ps(&v).is_empty()).unwrap_or(false);
            Some(if held { self.free_when_held } else { self.free_when_not })
        }
        fn process_mib(&self, _: u32) -> Option<(f64, f64)> {
            Some((8_000.0, 100.0))
        }
    }

    #[test]
    fn another_model_ollama_holds_is_let_go_when_the_card_is_short_of_the_load() {
        let fx = Fixture::new("dusk:22b");
        // The planner counted what Ollama's runner holds as free; the server starts after it lets go.
        fx.ollama.resident("other:9b");
        let mut cfg = fx.config();
        cfg.vram = Some(Arc::new(HeldByOllama { ollama: fx.ollama.endpoint(), free_when_held: 2_000.0, free_when_not: 20_000.0 }));
        cfg.vram_settle = Duration::ZERO;
        let e = LlamaEngine::new(cfg, fx.spawner.clone());
        let heard = Arc::new(Heard::default());
        let alloc = ContextAllocations::new();
        alloc.set_engine_observer(Arc::new(Ears(heard.clone())));
        e.follow(alloc);
        let mut w = want("dusk:22b", Lane::Conductor, 1);
        w.need_mib = Some(13.6 * 1024.0);
        e.acquire(&w, &CancelToken::new()).unwrap();
        assert_eq!((fx.spawner.starts(), fx.ollama.unloaded()), (1, vec!["other:9b".to_string()]));
        assert!(heard.lines.lock().unwrap().iter().any(|l| l == "Ollama let go of other:9b to make room for dusk:22b"), "{:?}", heard.lines.lock().unwrap());
    }

    #[test]
    fn a_card_that_stays_short_after_ollama_let_go_still_declines() {
        let fx = Fixture::new("dusk:22b");
        fx.ollama.resident("other:9b");
        let mut cfg = fx.config();
        cfg.vram = Some(Arc::new(HeldByOllama { ollama: fx.ollama.endpoint(), free_when_held: 2_000.0, free_when_not: 6_000.0 }));
        cfg.vram_settle = Duration::ZERO;
        let e = LlamaEngine::new(cfg, fx.spawner.clone());
        let mut w = want("dusk:22b", Lane::Conductor, 1);
        w.need_mib = Some(13.6 * 1024.0);
        let err = e.acquire(&w, &CancelToken::new()).err().expect("refused");
        assert_eq!(err.detail, "dusk:22b runs on Ollama: needs 13.6 GiB on the GPU, 5.9 GiB free");
        assert_eq!(fx.spawner.starts(), 0);
    }

    #[test]
    fn a_process_the_counters_report_nothing_for_is_not_a_spill() {
        let fx = Fixture::new("dusk:22b");
        let (e, vram, told, _) = with_gpu(&fx, Some(20_000.0));
        // PDH answered without an instance for the server (or there is no PDH): zero and zero is
        // no reading, not a model that is nowhere.
        *vram.process.lock().unwrap() = Some((0.0, 0.0));
        e.acquire(&want("dusk:22b", Lane::Conductor, 1), &CancelToken::new()).unwrap();
        assert_eq!((fx.spawner.starts(), fx.spawner.killed()), (1, 0));
        assert!(told.0.lock().unwrap().is_empty());
    }

    #[test]
    fn a_healthy_server_and_one_the_gpu_cannot_see_are_kept() {
        let fx = Fixture::new("dusk:22b");
        let (e, vram, _, _) = with_gpu(&fx, Some(20_000.0));
        e.acquire(&want("dusk:22b", Lane::Conductor, 1), &CancelToken::new()).unwrap();
        assert!(e.unload(None));
        // No reading for the process says nothing about a spill: the server stays.
        *vram.process.lock().unwrap() = None;
        e.acquire(&want("dusk:22b", Lane::Conductor, 1), &CancelToken::new()).unwrap();
        assert_eq!((fx.spawner.starts(), fx.spawner.killed()), (2, 1));
    }

    #[test]
    fn the_load_time_runs_from_the_start_of_the_server_to_its_being_up() {
        let fx = Fixture::new("dusk:22b");
        fx.spawner.start_takes(Duration::from_millis(400));
        let e = fx.engine();
        let began = Instant::now();
        drop(e.acquire(&want("dusk:22b", Lane::Conductor, 1), &CancelToken::new()).unwrap());
        let st = e.status().unwrap();
        assert!(st.load_secs >= 0.4 && st.load_secs <= began.elapsed().as_secs_f64(), "{}", st.load_secs);
        // A request for a server that is up starts nothing.
        let w = want("dusk:22b", Lane::Conductor, 1);
        assert!(e.runs(&w));
        let mut other = w.clone();
        other.shape.slots = 2;
        assert!(!e.runs(&other), "another shape restarts the server");
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
