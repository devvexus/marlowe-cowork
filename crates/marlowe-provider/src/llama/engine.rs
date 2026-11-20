//! Marlowe's own llama-server: one process, one model, several slots.
//!
//! Every local model runs here; Ollama is only where the files come from ([`super::eligible`]).
//! The engine starts the selected backend's `llama-server` on demand, serves every request for
//! the model on one of its slots, and stops it after the keep-alive. A model that fits whole on
//! the GPU runs there (`-ngl 99 --fit off`); one that does not, by the planner's estimate, the
//! memory free at the start or the driver's spill after the load, runs split: llama.cpp places
//! what fits on the GPU and keeps the rest on the CPU, as Ollama does ([`super::settings`]).
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
//!
//! A model with ONE slot has only the conversation's: the crew and the projects share it while the
//! conversation is idle, and the conversation always goes first (a crew request does not take the
//! slot while a conversation request waits for it). No request waits forever: past
//! [`EngineConfig::slot_wait`] it fails with a clear error and the caller decides. A conversation's
//! cache is not lost to the crew's turn: the slot is written to its own file before another
//! conversation takes it and read back when the conversation returns ([`persist::park`]).
//!
//! # Restarts
//!
//! A change of backend or of its installed version ([`LlamaEngine::restart_when_idle`]) never
//! cuts a request off: the server that started before the change stops once its last request is
//! done (every slot's cache saved first) and the next request starts the new one. Only
//! [`LlamaEngine::unload`] (a quit) stops a server with requests in flight.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock, Weak};
use std::time::{Duration, Instant};

use marlowe_model::{CancelToken, ContextAllocations, EngineObserver, EngineShape, Lane, ProviderError};

use marlowe_engines::Backend;

use super::eligible::{self, Chosen, EngineModel, ModelDefaults};
use super::launch::{self, Spec};
use super::persist;
use super::server::{offloaded, Process, Spawner};
use super::settings::{GpuFit, ServerSettings};
use super::template::{self, Thinking, ThinkingCache};
use super::warm;
use crate::http::{self, LocalEndpoint};
use crate::sources::Sources;

/// How long a request waits for a model switch to drain before it gives up.
const SWITCH_WAIT: Duration = Duration::from_secs(300);
/// How long a request waits for a slot before it gives up.
const SLOT_WAIT: Duration = Duration::from_secs(20 * 60);
/// How long a stop waits for the background reads of the next prompts.
const WARM_GRACE: Duration = Duration::from_secs(8);
/// A load that failed this recently is not tried again for the same shape.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(15);
const QUICK: Duration = Duration::from_millis(1500);

/// What the spill check tolerates, MiB: weights missing from dedicated memory beyond this are
/// somewhere else.
const SPILL_SLACK_MIB: f64 = 1024.0;
/// Shared (system) memory a healthy server may hold as pinned host buffers, MiB, beyond the
/// weights it reads into pinned memory on purpose (the host side when not memory-mapped).
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
    /// Where the model files are read: Ollama's store (Ollama itself is never asked), the Hugging
    /// Face cache and the model folders.
    pub store: Sources,
    /// Where the server's output goes (appended, capped).
    pub log: Option<PathBuf>,
    /// Flash attention, keep-alive, batch sizes, mmap and the GPU fit; changed later with
    /// [`LlamaEngine::set_settings`].
    pub settings: ServerSettings,
    /// Longest a load may take.
    pub start_timeout: Duration,
    /// Where each slot's cache is written when the server stops and read back when it starts
    /// again; `None` keeps no caches across a stop.
    pub slots_dir: Option<PathBuf>,
    /// Where what each model file's template does with earlier thinking is kept; `None` asks again
    /// at every start of the engine.
    pub thinking_cache: Option<PathBuf>,
    /// The server binary; `None` finds the selected backend's.
    pub binary: Option<PathBuf>,
    /// Which backend `binary` is. `None` with no `binary` reads the selected backend; `None` with
    /// one is a build of unknown kind, whose tensor types are not second-guessed.
    pub backend: Option<Backend>,
    /// The version of `backend` (its tag), for the MS generations it runs; `None` is not checked.
    pub backend_tag: Option<String>,
    /// Longest a request waits for a slot of the running model; then it fails.
    pub slot_wait: Duration,
    /// Longest an on-purpose stop waits for a background read of the next prompt to end, so the
    /// slot's cache (the conversation, read in full) is saved with the rest.
    pub warm_grace: Duration,
    /// How often a starting server is asked whether it is ready.
    pub poll: Duration,
    /// How often the idle check runs.
    pub idle_tick: Duration,
    /// The GPU, read before a server starts (does the model fit in the memory free) and after it
    /// loaded (did the driver spill it into system memory). A server starts without the checks
    /// only when this is `None`; with a probe, no reading is not "fits".
    pub vram: Option<Arc<dyn VramProbe>>,
    /// How long the free memory may take to show what a stopped server gave back before a
    /// misfit is final.
    pub vram_settle: Duration,
}

impl EngineConfig {
    pub fn new(store: impl Into<Sources>) -> Self {
        Self {
            store: store.into(),
            log: None,
            settings: ServerSettings::default(),
            start_timeout: Duration::from_secs(240),
            slots_dir: None,
            thinking_cache: None,
            binary: None,
            backend: None,
            backend_tag: None,
            slot_wait: SLOT_WAIT,
            warm_grace: WARM_GRACE,
            poll: Duration::from_millis(250),
            idle_tick: Duration::from_secs(2),
            vram: None,
            vram_settle: Duration::from_secs(2),
        }
    }
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
    /// check at the start then counts the model's GPU weights alone).
    pub need_mib: Option<f64>,
}

pub use marlowe_model::prefix::conversation_key;

/// The selected backend and its installed version, or `None` for a developer's own binary
/// (`MARLOWE_LLAMA_SERVER`) or no install folder.
pub fn selected_backend() -> Option<Chosen> {
    if std::env::var_os(marlowe_engines::SERVER_ENV).is_some_and(|v| !v.is_empty()) {
        return None;
    }
    let root = marlowe_engines::default_root()?;
    let backend = marlowe_engines::state::selected(&root);
    Some(Chosen { backend, tag: marlowe_engines::server(&root, backend).ok().map(|s| s.tag) })
}

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
    /// The conversation (conductor lane) is what `key` names: its cache is worth keeping.
    conv: bool,
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
    /// What the Modelfile adds to every request.
    defaults: Arc<ModelDefaults>,
    /// Split between the GPU and the CPU.
    split: bool,
    slots: Vec<Slot>,
    /// What the model file's template does with earlier thinking.
    thinking: Thinking,
    /// The backend and version this server is (read when it started).
    chosen: Option<Chosen>,
    /// [`State::restarts`] when its start began: older than that, it is to be replaced.
    born: u64,
    /// Conversation caches of a one-slot model written out while the slot served someone else
    /// (conversation key, file in the slots folder), oldest first.
    parked: Vec<(u64, String)>,
}

/// How many parked caches a one-slot model keeps.
const PARKED_MAX: usize = 2;

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
    /// Conversation requests waiting for a slot: on a one-slot model a crew request lets them pass.
    conductor_waiting: usize,
    epoch: u64,
    failed: Option<(String, EngineShape, Instant, ProviderError)>,
    /// Bumped when the backend or its version changed (and by a stop during a load): a server
    /// that began before is replaced as soon as it is idle.
    restarts: u64,
}

struct Inner {
    cfg: EngineConfig,
    settings: RwLock<ServerSettings>,
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
                let endpoint = LocalEndpoint::new("127.0.0.1", r.port).expect("loopback");
                // A crew turn holds the one slot of a one-slot model: the conversation's cache
                // parked behind it is what is worth keeping.
                if let (1, Some((key, file))) = (r.slots.len(), r.parked.last().cloned()) {
                    if !r.slots[0].busy && !r.slots[0].conv && persist::unpark(&endpoint, dir, 0, &file) {
                        r.slots[0].key = Some(key);
                    }
                }
                let used: Vec<(u32, u64)> =
                    r.slots.iter().enumerate().filter(|(_, s)| !s.busy).filter_map(|(i, s)| s.key.map(|k| (i as u32, k))).collect();
                persist::save(&endpoint, dir, &r.model, &r.blob, &r.shape, r.chosen.as_ref(), &used);
            }
            for (_, file) in r.parked.drain(..) {
                persist::discard(dir, &file);
            }
        }
        r.proc.kill();
    }

    /// The backend the next server is: the configured one, else the selected one, unless a
    /// developer's own binary is in use (unknown).
    fn chosen(&self) -> Option<Chosen> {
        let cfg = &self.cfg;
        if cfg.backend.is_some() || cfg.binary.is_some() {
            return cfg.backend.map(|backend| Chosen { backend, tag: cfg.backend_tag.clone() });
        }
        selected_backend()
    }

    /// Stops the server when it began before the last restart request and nothing is using it.
    /// Returns what was stopped. Called under the lock.
    fn retire_stale(&self, g: &mut State) -> Option<Running> {
        let restarts = g.restarts;
        let mut gone = g.running.take_if(|r| r.born < restarts && r.busy() == 0)?;
        self.retire(&mut gone);
        Some(gone)
    }
}

/// Takes a waiting conversation request off the count when its `acquire` ends.
struct QueueGuard<'a> {
    inner: &'a Arc<Inner>,
    queued: &'a std::cell::Cell<bool>,
}

impl Drop for QueueGuard<'_> {
    fn drop(&mut self) {
        if self.queued.get() {
            let mut g = lock(&self.inner.st);
            g.conductor_waiting = g.conductor_waiting.saturating_sub(1);
            drop(g);
            self.inner.cv.notify_all();
        }
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
    /// Split between the GPU and the CPU.
    pub split: bool,
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
    /// What the Modelfile adds to every request: sampler defaults, `SYSTEM`, `MESSAGE`s.
    pub defaults: Arc<ModelDefaults>,
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
            settings: RwLock::new(cfg.settings.clone()),
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

    /// The server settings the next start uses.
    pub fn settings(&self) -> ServerSettings {
        self.inner.settings.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Changes the server settings; a running server keeps the old ones until it restarts.
    pub fn set_settings(&self, s: ServerSettings) {
        *self.inner.settings.write().unwrap_or_else(|e| e.into_inner()) = s;
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
            split: r.split,
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
        self.inner.cfg.log.as_deref().and_then(|p| crate::server_log::read_tail(p, max).ok()).unwrap_or_default()
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
        // A turn that just ended is reading the next prompt into its slot (see `Lease::read_ahead`):
        // that slot is not yet whole, and a cache saved now would miss the conversation. A stop
        // right after a reply lets the read finish first, a few seconds at most.
        let until = Instant::now() + self.inner.cfg.warm_grace;
        while g.running.as_ref().is_some_and(|r| model.is_none_or(|m| same_model(&r.model, m)) && r.slots.iter().any(|s| s.warming)) && Instant::now() < until {
            g = self.inner.cv.wait_timeout(g, Duration::from_millis(50)).unwrap_or_else(|e| e.into_inner()).0;
        }
        // A server still loading is not there to stop: whatever comes up is replaced as soon as
        // it is idle.
        if g.loading.as_ref().is_some_and(|l| model.is_none_or(|m| same_model(l, m))) {
            g.restarts += 1;
        }
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

    /// The backend (or its installed version) changed: the server that runs now, or is loading, is
    /// replaced. Requests in flight finish first (a long generation is not cut off), then every
    /// slot's cache is saved and the server stops; the next request starts the selected backend.
    pub fn restart_when_idle(&self) {
        let mut g = lock(&self.inner.st);
        g.restarts += 1;
        let stopped = self.inner.retire_stale(&mut g);
        drop(g);
        self.inner.cv.notify_all();
        if let (Some(r), Some(o)) = (stopped, self.observer()) {
            o.note(&format!("llama.cpp stopped {} to start the new backend", r.model));
        }
    }

    /// Takes a slot for the request, loading (or switching) the model first when it must.
    pub fn acquire(&self, asked: &Want, cancel: &CancelToken) -> Result<Lease, ProviderError> {
        let inner = &self.inner;
        // The shape a server this request starts came up in, when the engine had to change it
        // (a misfit makes the model split): the request is served by it, not restarted onto its own.
        let mut adopted: Option<Want> = None;
        let began = Instant::now();
        let queued = std::cell::Cell::new(false);
        // Declared before the lock guard, so it drops (and takes the lock) after it is released.
        let _queue = QueueGuard { inner, queued: &queued };
        let mut g = lock(&inner.st);
        let mut slot_began: Option<Instant> = None;
        loop {
            let want = adopted.as_ref().unwrap_or(asked);
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
            if g.running.as_ref().is_some_and(|r| r.born < g.restarts) {
                // Replaced by the new backend: after the requests it is serving.
                if g.running.as_ref().is_some_and(|r| r.busy() > 0) {
                    if began.elapsed() > SWITCH_WAIT {
                        return Err(ProviderError::server("the llama.cpp server is busy and cannot restart onto the new backend yet".to_string()));
                    }
                    g = self.wait(g, cancel);
                } else {
                    inner.retire_stale(&mut g);
                }
                continue;
            }
            let conductor_waiting = g.conductor_waiting;
            match g.running.as_mut() {
                Some(r) if r.matches(want) => {
                    let single = r.slots.len() == 1;
                    let yields = single && want.lane != Lane::Conductor && conductor_waiting > 0;
                    if let Some(i) = r.pick(want.lane, want.key).filter(|_| !yields) {
                        let swap = self.take_over(r, i, want);
                        let lease = Lease {
                            inner: inner.clone(),
                            epoch: r.epoch,
                            endpoint: LocalEndpoint::new("127.0.0.1", r.port).expect("loopback"),
                            slot: i as u32,
                            defaults: r.defaults.clone(),
                            ctx_per_slot: r.shape.ctx_per_slot,
                            thinking: r.thinking.clone(),
                        };
                        drop(g);
                        // The slot is ours (busy): the caches move without the lock held.
                        self.swap_caches(&lease, swap, &want.model);
                        return Ok(lease);
                    }
                    if slot_began.get_or_insert_with(Instant::now).elapsed() > inner.cfg.slot_wait {
                        return Err(ProviderError::server(format!(
                            "no slot of {} came free in {}: it is serving other requests",
                            want.model,
                            humanize(inner.cfg.slot_wait)
                        )));
                    }
                    if want.lane == Lane::Conductor && single && !queued.get() {
                        queued.set(true);
                        g.conductor_waiting += 1;
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
                    let (epoch, born) = (g.epoch, g.restarts);
                    drop(g);
                    let result = self.start(want, epoch, cancel);
                    g = lock(&inner.st);
                    g.loading = None;
                    match result {
                        Ok(mut r) => {
                            r.born = born;
                            if r.shape != want.shape {
                                adopted = Some(Want { shape: r.shape.clone(), ..want.clone() });
                            }
                            g.failed = None;
                            g.running = Some(r);
                        }
                        Err(e) => {
                            if !e.is_cancelled() {
                                g.failed = Some((want.model.clone(), want.shape.clone(), Instant::now(), e.clone()));
                            }
                            drop(g);
                            inner.cv.notify_all();
                            return Err(e);
                        }
                    }
                    inner.cv.notify_all();
                }
            }
        }
    }

    /// Marks slot `i` as `want`'s. On a one-slot model another conversation's cache is leaving it:
    /// what to write out and what to read back (see [`Swap`]). Called under the lock.
    fn take_over(&self, r: &mut Running, i: usize, want: &Want) -> Swap {
        let conductor = want.lane == Lane::Conductor;
        let slot = &mut r.slots[i];
        let (was, was_conv) = (slot.key, slot.conv);
        slot.busy = true;
        slot.key = Some(want.key);
        slot.conv = conductor;
        slot.last = Instant::now();
        r.last_used = Instant::now();
        let mut swap = Swap::default();
        if r.slots.len() != 1 || was == Some(want.key) || self.inner.cfg.slots_dir.is_none() {
            return swap;
        }
        if let Some(prev) = was.filter(|_| was_conv) {
            r.parked.retain(|(k, _)| *k != prev);
            swap.park = Some(prev);
        }
        if conductor {
            swap.back = r.parked.iter().position(|(k, _)| *k == want.key).map(|at| r.parked.remove(at).1);
        }
        swap
    }

    /// Writes the leaving conversation's cache out and reads the returning one's back (best
    /// effort: a failure is a cold cache, as before). The lease is held, so nothing else uses
    /// the slot meanwhile.
    fn swap_caches(&self, lease: &Lease, swap: Swap, model: &str) {
        let Some(dir) = &self.inner.cfg.slots_dir else { return };
        if let Some(key) = swap.park {
            if let Some(file) = persist::park(&lease.endpoint, dir, model, lease.slot, key) {
                let mut g = lock(&self.inner.st);
                if let Some(r) = g.running.as_mut().filter(|r| r.epoch == lease.epoch) {
                    r.parked.push((key, file));
                    while r.parked.len() > PARKED_MAX {
                        persist::discard(dir, &r.parked.remove(0).1);
                    }
                } else {
                    persist::discard(dir, &file);
                }
            }
        }
        if let Some(file) = swap.back {
            persist::unpark(&lease.endpoint, dir, lease.slot, &file);
            persist::discard(dir, &file);
        }
    }

    fn wait<'a>(&self, g: MutexGuard<'a, State>, cancel: &CancelToken) -> MutexGuard<'a, State> {
        let _ = cancel;
        self.inner.cv.wait_timeout(g, Duration::from_millis(100)).unwrap_or_else(|e| e.into_inner()).0
    }

    /// The model does not fit whole on the GPU: the advisor plans it split from here on, and the
    /// user is told in one line.
    fn say_split(&self, model: &str, why: &str) {
        if let Some(a) = self.inner.events.read().unwrap_or_else(|e| e.into_inner()).as_ref() {
            a.engine_declined(model, why);
        }
        if let Some(o) = self.observer() {
            o.note(&format!("{model} runs split between the GPU and the CPU: {why}"));
        }
    }

    /// The shape the planner now asks for after `say_split` (one slot of the default window, split);
    /// the request's own shape when there is no planner, or it does not call it split.
    fn split_shape(&self, cur: &Want) -> EngineShape {
        let advised = self.inner.events.read().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|a| a.engine_shape(&cur.model));
        advised.filter(|s| s.split).unwrap_or_else(|| cur.shape.clone())
    }

    fn observer(&self) -> Option<Arc<dyn EngineObserver>> {
        self.inner.events.read().unwrap_or_else(|e| e.into_inner()).as_ref()?.engine_observer()
    }

    /// The backend the server binary is, and its installed version: the configured one, else the
    /// selected one, unless a developer's own binary is in use (unknown).
    pub fn chosen(&self) -> Option<Chosen> {
        self.inner.chosen()
    }

    /// Whether the GPU has room for the model's GPU part now: with the driver's sysmem fallback a
    /// model that does not fit would load anyway, into system memory, and read at a few tokens a
    /// second. Only what goes on the GPU counts: the input layer stays on the CPU and the experts
    /// `--n-cpu-moe` moves are in RAM. `Err` says why it does not fit; no reading is not "fits".
    fn check_room(&self, want: &Want, model: &EngineModel) -> Result<(), String> {
        let cfg = &self.inner.cfg;
        let Some(probe) = &cfg.vram else { return Ok(()) };
        let need = want.need_mib.unwrap_or(0.0).max(gpu_weights_mib(model, want.shape.cpu_moe) + SPILL_SLACK_MIB / 2.0);
        let until = Instant::now() + cfg.vram_settle;
        let mut free = probe.free_mib();
        // A server this engine just stopped takes a moment to show as free.
        while free.is_none_or(|f| f < need) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(200).min(cfg.vram_settle));
            free = probe.free_mib();
        }
        match free {
            Some(f) if f >= need => Ok(()),
            Some(f) => Err(format!("needs {} GiB on the GPU, {} GiB free", gib(need), gib(f))),
            None => Err("there is no reading of the GPU's free memory to check the load against".into()),
        }
    }

    /// Whether the driver paged the loaded model into system memory: the server holds shared
    /// memory well past its pinned buffers, or far less dedicated memory than the model's GPU
    /// weights. What is meant to be on the CPU is not a spill: memory-mapped, the input layer and
    /// the experts `--n-cpu-moe` keeps there are the file's pages, not GPU memory at all; read
    /// into RAM they are pinned host buffers, which the driver counts as shared.
    /// `None` when it did not, or when the GPU cannot be read for the process.
    fn spilled(&self, pid: u32, model: &EngineModel, shape: &EngineShape, settings: &ServerSettings) -> Option<String> {
        let probe = self.inner.cfg.vram.as_ref()?;
        let (dedicated, shared) = probe.process_mib(pid)?;
        // A server that has loaded a model holds some memory: nothing at all is a counter that
        // could not be read (or has no instance yet), and says nothing about a spill.
        if dedicated <= 0.0 && shared <= 0.0 {
            return None;
        }
        let missing = (gpu_weights_mib(model, shape.cpu_moe) - dedicated).max(0.0);
        let pinned = if settings.mmap && !shape.host_ram { 0.0 } else { host_weights_mib(model, shape.cpu_moe) };
        let allowance = PINNED_ALLOWANCE_MIB + pinned;
        if shared <= allowance && missing <= SPILL_SLACK_MIB {
            return None;
        }
        let moved = if shared > allowance { shared - pinned } else { missing };
        Some(format!(
            "the driver moved {} GiB of it into system memory, where it reads at a fraction of the speed",
            gib(moved)
        ))
    }

    fn start(&self, want: &Want, epoch: u64, cancel: &CancelToken) -> Result<Running, ProviderError> {
        let cfg = &self.inner.cfg;
        let settings = self.settings();
        let detail = format!(
            "{} slot{} x {} tokens, kv {}",
            want.shape.slots,
            if want.shape.slots == 1 { "" } else { "s" },
            want.shape.ctx_per_slot,
            want.shape.kv
        );
        let _meter = self.observer().map(|o| o.loading(&want.model, &detail));
        let began = Instant::now();
        let cant = |why: String| ProviderError::bad_request(format!("{} can't run: {why}", want.model));
        let model = eligible::resolve(&cfg.store, &want.model).map_err(cant)?;
        let chosen = self.chosen();
        let binary = match &cfg.binary {
            Some(b) => b.clone(),
            None => launch::server_binary().map_err(|e| ProviderError::bad_request(e.remedy()))?,
        };
        if let Some(r) = model.refuses_on(chosen.as_ref()) {
            return Err(ProviderError::bad_request(r.engine_error()));
        }
        let whole_only = |why: &str| {
            ProviderError::server(format!("{} does not fit whole on the GPU ({why}), and the engine's gpu_fit is `whole`", want.model))
        };
        // What runs and is remembered as the server's shape: the request's, until a misfit makes the
        // model split and the planner re-plans it (the next request then asks for that shape).
        let mut cur = want.clone();
        let mut split = match settings.gpu_fit {
            GpuFit::Auto => want.shape.split,
            GpuFit::Whole => false,
            GpuFit::Split => true,
        };
        if !split {
            if let Err(why) = self.check_room(want, &model) {
                if settings.gpu_fit == GpuFit::Whole {
                    return Err(whole_only(&why));
                }
                self.say_split(&want.model, &why);
                split = true;
                cur.shape = self.split_shape(&cur);
            }
        }
        let backends = launch::backend_candidates(&binary);
        let tries: Vec<Option<PathBuf>> = if backends.is_empty() { vec![None] } else { backends.into_iter().map(Some).collect() };
        let mut why: Vec<String> = Vec::new();
        for backend in tries {
            if cancel.is_cancelled() {
                return Err(ProviderError::cancelled());
            }
            let mut attempt = self.try_start(&binary, backend.as_deref(), &model, &cur, split, &settings, cancel);
            let spilled = match &attempt {
                Err(Tried::Spilled(m)) => Some(m.clone()),
                _ => None,
            };
            if let Some(moved) = spilled {
                // It loaded and the driver paged it out: the same backend again, split.
                if settings.gpu_fit == GpuFit::Whole {
                    return Err(whole_only(&moved));
                }
                self.say_split(&want.model, &moved);
                split = true;
                cur.shape = self.split_shape(&cur);
                attempt = self.try_start(&binary, backend.as_deref(), &model, &cur, split, &settings, cancel);
            }
            match attempt {
                Ok((proc, port, on_gpu)) => return Ok(self.running(&cur, &model, epoch, began, proc, port, on_gpu, split, chosen)),
                Err(Tried::Next(m) | Tried::Spilled(m)) => why.push(m),
                Err(Tried::Fatal(e)) => return Err(e),
            }
        }
        Err(ProviderError::server(format!("llama-server for {} did not start on the GPU: {}", want.model, why.join("; "))))
    }

    /// The server that came up, with its slots (each conversation it served before a stop given
    /// its cache back).
    #[allow(clippy::too_many_arguments)]
    fn running(
        &self,
        want: &Want,
        model: &EngineModel,
        epoch: u64,
        began: Instant,
        proc: Box<dyn Process>,
        port: u16,
        on_gpu: Option<(u32, u32)>,
        split: bool,
        chosen: Option<Chosen>,
    ) -> Running {
        let n = usize::from(want.shape.slots.max(1));
        let mut slots: Vec<Slot> = (0..n).map(|_| Slot { busy: false, warming: false, key: None, conv: false, last: Instant::now() }).collect();
        let endpoint = LocalEndpoint::new("127.0.0.1", port).expect("loopback");
        let thinking = self.thinking_of(&endpoint, want, &model.blob);
        if let Some(dir) = &self.inner.cfg.slots_dir {
            for (i, key) in persist::restore(&endpoint, dir, &want.model, &model.blob, &want.shape, chosen.as_ref()) {
                if let Some(s) = slots.get_mut(i as usize) {
                    s.key = Some(key);
                }
            }
        }
        if let (true, Some((n, m)), Some(o)) = (split, on_gpu, self.observer()) {
            o.note(&format!("llama.cpp placed {n} of {m} layers of {} on the GPU", want.model));
        }
        Running {
            model: want.model.clone(),
            shape: want.shape.clone(),
            port,
            blob: model.blob.clone(),
            proc,
            epoch,
            load_secs: began.elapsed().as_secs_f64(),
            last_used: Instant::now(),
            on_gpu,
            defaults: Arc::new(model.defaults.clone()),
            split,
            slots,
            thinking,
            chosen,
            born: 0,
            parked: Vec::new(),
        }
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

    #[allow(clippy::too_many_arguments)]
    fn try_start(
        &self,
        binary: &Path,
        backend: Option<&Path>,
        model: &EngineModel,
        want: &Want,
        split: bool,
        settings: &ServerSettings,
        cancel: &CancelToken,
    ) -> Result<(Box<dyn Process>, u16, Option<(u32, u32)>), Tried> {
        let cfg = &self.inner.cfg;
        let io = |e: std::io::Error| Tried::Fatal(ProviderError::server(format!("could not start llama-server: {e}")));
        let port = self.inner.spawner.free_port().map_err(io)?;
        if let Some(dir) = &cfg.slots_dir {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        let spec = Spec {
            blob: model.blob.clone(),
            port,
            slots: want.shape.slots.max(1),
            ctx_per_slot: want.shape.ctx_per_slot,
            kv: want.shape.kv.clone(),
            slot_save: cfg.slots_dir.clone(),
            draft: model.draft.clone(),
            mmproj: model.mmproj.clone(),
            split,
            cpu_moe: want.shape.cpu_moe,
            host_ram: want.shape.host_ram,
            settings: settings.clone(),
        };
        let plan = launch::plan(binary, backend, &spec);
        // The same line Ollama logs for its runners, so the planner reads this load like any other.
        if let Some(path) = cfg.log.as_deref() {
            if let Ok(mut f) = crate::server_log::open_own_log(path, crate::server_log::OWN_LOG_CAP) {
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
                let hint = load_failure_hint(&want.model, &tail, &model.unknown_types());
                return Err(Tried::Fatal(ProviderError::server(format!(
                    "llama-server exited with code {code} while loading {}{}{hint}",
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
        if split {
            // llama.cpp placed the layers against the free memory: what is on the CPU is meant to be.
            return Ok((proc, port, on_gpu));
        }
        // A backend that did not load leaves a healthy server on the CPU, ten times slower.
        if backend.is_some() && on_gpu.is_some_and(|(n, _)| n == 0) {
            proc.kill();
            return Err(Tried::Next(format!("{} loaded no layers on the GPU", backend.map(|b| b.display().to_string()).unwrap_or_default())));
        }
        // `--fit off` does not fail when the card is full under the Windows driver's sysmem
        // fallback: the server is healthy, and slow. The weights are resident now, so look.
        if let Some(why) = self.spilled(proc.id(), model, &want.shape, settings) {
            proc.kill();
            return Err(Tried::Spilled(why));
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

/// What changing hands of a one-slot model's slot moves.
#[derive(Default)]
struct Swap {
    /// The conversation whose cache is leaving: written out first.
    park: Option<u64>,
    /// A parked file of the conversation that is back.
    back: Option<String>,
}

enum Tried {
    /// This backend did not work; try the next.
    Next(String),
    Fatal(ProviderError),
    /// The server loaded whole, and the driver spilled the model into system memory: it runs split.
    Spilled(String),
}

/// The weights the GPU holds with the experts of the first `cpu_moe` blocks on the CPU, MiB:
/// everything but the input layer (llama.cpp keeps it on the CPU) and those experts. The files'
/// sizes when the headers could not be analysed.
fn gpu_weights_mib(model: &EngineModel, cpu_moe: u32) -> f64 {
    match &model.parts {
        Some(p) => p.gpu_with(cpu_moe) as f64 / MIB,
        None => {
            let size = |p: &Path| std::fs::metadata(p).map_or(0.0, |m| m.len() as f64 / MIB);
            size(&model.blob) + model.draft.as_ref().map_or(0.0, |d| size(&d.path)) + model.mmproj.as_deref().map_or(0.0, size)
        }
    }
}

/// The weights on the CPU, MiB: the input layer and the experts of the first `cpu_moe` blocks.
fn host_weights_mib(model: &EngineModel, cpu_moe: u32) -> f64 {
    model.parts.as_ref().map_or(0.0, |p| (p.host + p.gpu - p.gpu_with(cpu_moe)) as f64 / MIB)
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
        let keep = inner.settings.read().unwrap_or_else(|e| e.into_inner()).keep_alive;
        let observer = || inner.events.read().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(ContextAllocations::engine_observer);
        // A server of the backend that was just replaced goes as soon as its last request is done.
        let replaced = inner.retire_stale(&mut lock(&inner.st));
        if let (Some(r), Some(o)) = (replaced, observer()) {
            o.note(&format!("llama.cpp stopped {} to start the new backend", r.model));
        }
        let Some(keep) = keep else { continue };
        let expired = {
            let mut g = lock(&inner.st);
            let mut gone = g.running.take_if(|r| r.busy() == 0 && r.last_used.elapsed() >= keep);
            if let Some(r) = gone.as_mut() {
                inner.retire(r);
            }
            gone
        };
        if let (Some(r), Some(o)) = (expired, observer()) {
            o.note(&format!("llama.cpp unloaded {} after {} idle", r.model, humanize(keep)));
        }
    }
}

/// What to add to "llama-server exited while loading <model>" so the user knows what to do.
/// Found live: Ollama's own library downloads of new architectures (`qwen3.5:0.8b`) can be
/// stored in Ollama's layout, which stock llama.cpp refuses ("qwen35.rope.dimension_sections has
/// wrong array length"); the same model as a GGUF from Hugging Face or a model folder runs.
fn load_failure_hint(model: &str, tail: &str, unknown: &[u32]) -> String {
    if !unknown.is_empty() {
        let ids = unknown.iter().map(u32::to_string).collect::<Vec<_>>().join(", ");
        return format!(" (the file uses ggml types {ids}, which this llama.cpp may not know: `Sync to upstream` in the engine window installs the newest build)");
    }
    let from_ollama = !model.starts_with(crate::sources::FILE_PREFIX);
    let layout = ["wrong array length", "error loading model hyperparameters", "unknown model architecture", "missing tensor"].iter().any(|s| tail.contains(s));
    if from_ollama && layout {
        return " (this download is from Ollama's library, which sometimes stores a model in a layout of its own that llama.cpp cannot read: \
                 the same model as a GGUF from Hugging Face or your model folder will run)"
            .to_string();
    }
    String::new()
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

    #[test]
    fn a_load_failure_says_what_to_do_about_ollamas_own_layout() {
        let tail = "llama_model_load: error loading model hyperparameters: key qwen35.rope.dimension_sections has wrong array length; expected 4, got 3";
        assert!(load_failure_hint("qwen3.5:0.8b", tail, &[]).contains("Ollama's library"));
        // A file the user picked from Hugging Face or a folder is not Ollama's layout.
        assert_eq!(load_failure_hint("gguf/hf/org/model/x", tail, &[]), "");
        // Unknown tensor types point at the newest build instead.
        assert!(load_failure_hint("qwen3.5:0.8b", tail, &[61]).contains("Sync to upstream"));
        // Any other exit adds nothing.
        assert_eq!(load_failure_hint("qwen3.5:0.8b", "CUDA error: out of memory", &[]), "");
    }

    fn want(model: &str, lane: Lane, key: u64) -> Want {
        Want { model: model.into(), shape: EngineShape { slots: 3, ctx_per_slot: 8192, kv: "q8_0".into(), split: false, cpu_moe: 0, host_ram: false }, lane, key, need_mib: None }
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

    fn one_slot(model: &str, lane: Lane, key: u64) -> Want {
        let mut w = want(model, lane, key);
        w.shape.slots = 1;
        w
    }

    #[test]
    fn on_a_one_slot_model_the_crew_takes_the_idle_conversations_slot() {
        let fx = Fixture::new("dusk:22b");
        let e = fx.engine();
        let c = CancelToken::new();
        let conv = e.acquire(&one_slot("dusk:22b", Lane::Conductor, 1), &c).unwrap();
        assert_eq!(conv.slot, 0);
        drop(conv);
        let crew = e.acquire(&one_slot("dusk:22b", Lane::Other, 10), &c).unwrap();
        assert_eq!(crew.slot, 0, "the conversation is idle: the agent shares its slot");
        drop(crew);
        assert_eq!(fx.spawner.starts(), 1);
    }

    #[test]
    fn on_a_one_slot_model_a_waiting_conversation_request_goes_before_the_crew() {
        let fx = Fixture::new("dusk:22b");
        let e = fx.engine();
        let c = CancelToken::new();
        let holder = e.acquire(&one_slot("dusk:22b", Lane::Other, 10), &c).unwrap();
        let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        let spawn = |lane: Lane, key: u64, name: &'static str| {
            let (e, order) = (e.clone(), order.clone());
            std::thread::spawn(move || {
                let lease = e.acquire(&one_slot("dusk:22b", lane, key), &CancelToken::new()).unwrap();
                order.lock().unwrap().push(name);
                std::thread::sleep(Duration::from_millis(150));
                drop(lease);
            })
        };
        // The crew request queues first, the conversation's second; both wait for the holder.
        let crew = spawn(Lane::Other, 11, "crew");
        std::thread::sleep(Duration::from_millis(200));
        let conv = spawn(Lane::Conductor, 1, "conversation");
        std::thread::sleep(Duration::from_millis(300));
        drop(holder);
        crew.join().unwrap();
        conv.join().unwrap();
        assert_eq!(*order.lock().unwrap(), vec!["conversation", "crew"]);
    }

    #[test]
    fn no_request_waits_for_a_slot_forever() {
        let fx = Fixture::new("dusk:22b");
        let mut cfg = fx.config();
        cfg.slot_wait = Duration::from_millis(300);
        let e = LlamaEngine::new(cfg, fx.spawner.clone());
        let c = CancelToken::new();
        let _held = e.acquire(&one_slot("dusk:22b", Lane::Conductor, 1), &c).unwrap();
        let began = Instant::now();
        let err = e.acquire(&one_slot("dusk:22b", Lane::Other, 10), &c).err().unwrap();
        assert!(err.detail.starts_with("no slot of dusk:22b came free"), "{}", err.detail);
        assert!(began.elapsed() < Duration::from_secs(5));
        // A conversation request that gave up no longer holds the crew back.
        let err = e.acquire(&one_slot("dusk:22b", Lane::Conductor, 2), &c).err().unwrap();
        assert!(err.detail.contains("no slot"), "{}", err.detail);
        drop(_held);
        assert!(e.acquire(&one_slot("dusk:22b", Lane::Other, 10), &c).is_ok());
    }

    #[test]
    fn the_launch_carries_the_models_draft_and_projector() {
        let fx = Fixture::with_layers("dawn:9b", true, true);
        let e = fx.engine();
        e.acquire(&want("dawn:9b", Lane::Conductor, 1), &CancelToken::new()).unwrap();
        let plan = fx.spawner.plans.lock().unwrap()[0].clone();
        let at = |f: &str| plan.args.iter().position(|a| a == f).map(|i| plan.args[i + 1].clone());
        assert_eq!((at("--spec-type").as_deref(), at("--spec-draft-n-max").as_deref()), (Some("draft-mtp"), Some("3")));
        assert!(at("--spec-draft-model").is_some() && at("--mmproj").is_some(), "{:?}", plan.args);
    }

    #[test]
    fn a_model_the_engine_cannot_run_is_an_error_with_the_reason_and_starts_nothing() {
        let fx = Fixture::new("dusk:22b");
        crate::llama::eligible::tests::store(
            &fx.dir.path().join("store"),
            "lora:1",
            serde_json::json!({"model_format": "gguf"}),
            &[(crate::ollama_store::MODEL_MEDIA_TYPE, &crate::llama::testkit::fixture_gguf()[..]), ("application/vnd.ollama.image.adapter", &b"lora"[..])],
        );
        let e = fx.engine();
        let err = e.acquire(&want("lora:1", Lane::Conductor, 1), &CancelToken::new()).err().unwrap();
        assert!(err.detail.starts_with("lora:1 can't run: it applies a LoRA adapter"), "{}", err.detail);
        assert_eq!(fx.spawner.starts(), 0);
    }

    #[test]
    fn an_ms_model_on_stock_llama_cpp_is_refused_and_runs_on_ms_llama_cpp() {
        let fx = Fixture::new("dusk:22b");
        let ms = crate::llama::eligible::tests::ms_gguf(&[46], "MS2.7");
        crate::llama::eligible::tests::store(&fx.dir.path().join("store"), "q:ms", serde_json::json!({"model_format": "gguf"}), &[(crate::ollama_store::MODEL_MEDIA_TYPE, &ms[..])]);
        let mut cfg = fx.config();
        cfg.backend = Some(Backend::LlamaCpp);
        let err = LlamaEngine::new(cfg.clone(), fx.spawner.clone()).acquire(&want("q:ms", Lane::Conductor, 1), &CancelToken::new()).err().unwrap();
        assert_eq!(
            err.detail,
            "q:ms is an MS (Mad Shift) model: it runs on ms-llama.cpp. Select ms-llama.cpp in the engine window (click `engine` in the title bar)."
        );
        assert_eq!(fx.spawner.starts(), 0);
        cfg.backend = Some(Backend::MsLlamaCpp);
        LlamaEngine::new(cfg, fx.spawner.clone()).acquire(&want("q:ms", Lane::Conductor, 1), &CancelToken::new()).unwrap();
        assert_eq!(fx.spawner.starts(), 1);
    }

    #[test]
    fn an_ms_llama_cpp_that_no_longer_runs_the_files_generation_is_refused_before_launch() {
        let fx = Fixture::new("dusk:22b");
        let ms = crate::llama::eligible::tests::ms_gguf(&[46], "MS2.7");
        crate::llama::eligible::tests::store(&fx.dir.path().join("store"), "q:ms", serde_json::json!({"model_format": "gguf"}), &[(crate::ollama_store::MODEL_MEDIA_TYPE, &ms[..])]);
        let mut cfg = fx.config();
        cfg.backend = Some(Backend::MsLlamaCpp);
        cfg.backend_tag = Some("ms-v2.0".into());
        let err = LlamaEngine::new(cfg.clone(), fx.spawner.clone()).acquire(&want("q:ms", Lane::Conductor, 1), &CancelToken::new()).err().unwrap();
        assert!(err.detail.contains("q:ms is an MS Gen 1 model: it needs ms-llama.cpp ms-v1.0, and the installed ms-v2.0 only runs Gen 2 files"), "{}", err.detail);
        assert_eq!(fx.spawner.starts(), 0);
        cfg.backend_tag = Some("ms-v1.0".into());
        LlamaEngine::new(cfg, fx.spawner.clone()).acquire(&want("q:ms", Lane::Conductor, 1), &CancelToken::new()).unwrap();
        assert_eq!(fx.spawner.starts(), 1);
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

    /// What a quit does (the daemon's close unloads the engine's model): on either backend the
    /// caches are written first, then the one server is killed, and nothing is left running.
    #[test]
    fn a_quit_saves_the_caches_and_leaves_no_server_on_either_backend() {
        for backend in [Backend::LlamaCpp, Backend::MsLlamaCpp] {
            let fx = Fixture::new("dusk:22b");
            let mut cfg = fx.config();
            cfg.slots_dir = Some(fx.dir.path().join("slots"));
            cfg.backend = Some(backend);
            let e = LlamaEngine::new(cfg, fx.spawner.clone());
            let c = CancelToken::new();
            drop((e.acquire(&want("dusk:22b", Lane::Conductor, 11), &c).unwrap(), e.acquire(&want("dusk:22b", Lane::Other, 22), &c).unwrap()));
            assert_eq!(fx.spawner.starts(), 1);
            assert!(e.unload(None), "{backend:?}");
            assert_eq!(fx.spawner.seen.slot_calls.lock().unwrap().iter().filter(|c| c.starts_with("save ")).count(), 2, "{backend:?}");
            assert_eq!(fx.spawner.killed(), 1, "{backend:?}: the server was killed");
            assert!(e.status().is_none(), "{backend:?}: nothing runs after the quit");
        }
    }

    #[test]
    fn a_quit_right_after_a_reply_waits_for_the_read_ahead_so_its_cache_is_saved() {
        let fx = Fixture::new("dusk:22b");
        let mut cfg = fx.config();
        cfg.slots_dir = Some(fx.dir.path().join("slots"));
        let e = LlamaEngine::new(cfg, fx.spawner.clone());
        fx.spawner.completion_takes(Duration::from_millis(600), 200);
        let lease = e.acquire(&want("dusk:22b", Lane::Conductor, 11), &CancelToken::new()).unwrap();
        lease.read_ahead("the next prompt".into());
        std::thread::sleep(Duration::from_millis(100));
        let began = Instant::now();
        assert!(e.unload(None));
        assert!(began.elapsed() >= Duration::from_millis(300), "the stop let the read end");
        let calls = fx.spawner.seen.slot_calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1, "the conversation's slot was saved: {calls:?}");
    }

    #[test]
    fn a_read_ahead_that_will_not_end_does_not_hold_up_a_quit() {
        let fx = Fixture::new("dusk:22b");
        let mut cfg = fx.config();
        cfg.warm_grace = Duration::from_millis(200);
        let e = LlamaEngine::new(cfg, fx.spawner.clone());
        fx.spawner.completion_takes(Duration::from_secs(3), 200);
        let lease = e.acquire(&want("dusk:22b", Lane::Conductor, 11), &CancelToken::new()).unwrap();
        lease.read_ahead("the next prompt".into());
        std::thread::sleep(Duration::from_millis(100));
        let began = Instant::now();
        assert!(e.unload(None));
        assert!(began.elapsed() < Duration::from_secs(2));
        assert_eq!(fx.spawner.killed(), 1);
    }

    #[test]
    fn an_idle_server_is_stopped_after_the_keep_alive() {
        let fx = Fixture::new("dusk:22b");
        let mut cfg = fx.config();
        cfg.settings.keep_alive = Some(Duration::from_millis(150));
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
        cfg.settings.keep_alive = Some(Duration::from_millis(100));
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

    fn args_of(fx: &Fixture, i: usize) -> Vec<String> {
        fx.spawner.plans.lock().unwrap()[i].args.clone()
    }

    #[test]
    fn a_model_bigger_than_the_free_gpu_runs_split_and_never_whole() {
        let fx = Fixture::new("dusk:22b");
        // 13.6 GiB planned, 11.9 GiB free: `-ngl 99` would load it anyway, into system memory.
        let (e, _vram, told, heard) = with_gpu(&fx, Some(11.9 * 1024.0));
        let mut w = want("dusk:22b", Lane::Conductor, 1);
        w.need_mib = Some(13.6 * 1024.0);
        e.acquire(&w, &CancelToken::new()).unwrap();
        assert_eq!(fx.spawner.starts(), 1);
        let a = args_of(&fx, 0);
        assert!(a.windows(2).any(|p| p == ["--fit", "on"]) && !a.iter().any(|x| x == "-ngl"), "{a:?}");
        assert!(e.status().unwrap().split);
        assert_eq!(told.0.lock().unwrap().clone(), vec!["dusk:22b: needs 13.6 GiB on the GPU, 11.9 GiB free".to_string()]);
        assert!(heard.lines.lock().unwrap().iter().any(|l| l == "dusk:22b runs split between the GPU and the CPU: needs 13.6 GiB on the GPU, 11.9 GiB free"));
    }

    #[test]
    fn with_gpu_fit_whole_a_model_that_does_not_fit_is_an_error() {
        let fx = Fixture::new("dusk:22b");
        let (e, _vram, _, _) = with_gpu(&fx, Some(11.9 * 1024.0));
        e.set_settings(ServerSettings { gpu_fit: GpuFit::Whole, ..e.settings() });
        let mut w = want("dusk:22b", Lane::Conductor, 1);
        w.need_mib = Some(13.6 * 1024.0);
        let err = e.acquire(&w, &CancelToken::new()).err().expect("refused");
        assert!(err.detail.contains("does not fit whole on the GPU (needs 13.6 GiB on the GPU, 11.9 GiB free)"), "{}", err.detail);
        assert_eq!(fx.spawner.starts(), 0);
    }

    #[test]
    fn no_gpu_reading_never_starts_a_whole_server() {
        let fx = Fixture::new("dusk:22b");
        let (e, _vram, told, _) = with_gpu(&fx, None);
        e.acquire(&want("dusk:22b", Lane::Conductor, 1), &CancelToken::new()).unwrap();
        assert!(args_of(&fx, 0).windows(2).any(|p| p == ["--fit", "on"]), "split: llama.cpp measures for itself");
        assert!(told.0.lock().unwrap()[0].contains("no reading of the GPU"));
    }

    #[test]
    fn a_shape_the_planner_made_split_starts_split_without_a_check() {
        let fx = Fixture::new("dusk:22b");
        let (e, _vram, told, _) = with_gpu(&fx, Some(20_000.0));
        let mut w = want("dusk:22b", Lane::Conductor, 1);
        w.shape.split = true;
        e.acquire(&w, &CancelToken::new()).unwrap();
        assert!(args_of(&fx, 0).windows(2).any(|p| p == ["--fit", "on"]));
        assert!(told.0.lock().unwrap().is_empty(), "nothing new to tell the planner");
    }

    #[test]
    fn with_room_on_the_gpu_the_server_starts_whole() {
        let fx = Fixture::new("dusk:22b");
        let (e, _vram, told, _) = with_gpu(&fx, Some(20_000.0));
        let mut w = want("dusk:22b", Lane::Conductor, 1);
        w.need_mib = Some(13.6 * 1024.0);
        e.acquire(&w, &CancelToken::new()).unwrap();
        assert_eq!(fx.spawner.starts(), 1);
        assert!(args_of(&fx, 0).windows(2).any(|p| p == ["-ngl", "99"]));
        assert!(told.0.lock().unwrap().is_empty() && !e.status().unwrap().split);
    }

    #[test]
    fn memory_a_stopped_server_gave_back_gets_a_moment_to_show_as_free() {
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
        assert!(args_of(&fx, 0).windows(2).any(|p| p == ["-ngl", "99"]), "it fit once the memory showed");
    }

    #[test]
    fn a_model_the_driver_paged_into_system_memory_is_stopped_and_started_again_split() {
        let fx = Fixture::new("dusk:22b");
        let (e, vram, told, heard) = with_gpu(&fx, Some(20_000.0));
        // The server is healthy and holds 4 GiB of shared memory beyond its pinned buffers.
        *vram.process.lock().unwrap() = Some((11_000.0, 4_500.0));
        e.acquire(&want("dusk:22b", Lane::Conductor, 1), &CancelToken::new()).unwrap();
        assert_eq!((fx.spawner.starts(), fx.spawner.killed()), (2, 1), "the spilled server was stopped");
        assert!(args_of(&fx, 0).windows(2).any(|p| p == ["-ngl", "99"]) && args_of(&fx, 1).windows(2).any(|p| p == ["--fit", "on"]));
        assert!(e.status().unwrap().split);
        assert_eq!(told.0.lock().unwrap().len(), 1);
        assert!(heard.lines.lock().unwrap().iter().any(|l| l.starts_with("dusk:22b runs split between the GPU and the CPU: the driver moved 4.4 GiB")));
    }

    /// A model of two blocks (1 GiB each on the GPU, 0.5 GiB of it experts) beside an 8 GiB n-gram
    /// table: the header alone, sized by the tensor shapes.
    fn ngram_model() -> Vec<u8> {
        let kv = [
            ("general.architecture", serde_json::json!("qwen35")),
            ("qwen35.block_count", serde_json::json!(2)),
            ("qwen35.context_length", serde_json::json!(32_768)),
            ("tokenizer.chat_template", serde_json::json!(crate::llama::eligible::tests::TEMPLATE)),
        ];
        // F32 throughout: 4 bytes a weight.
        let gib_rows = 1024 * 1024 * 1024 / 4 / 1024;
        marlowe_hw::gguf::write_header(
            &kv,
            &[
                ("per_layer_token_embd.weight", 0, vec![1024, 8 * gib_rows]),
                ("blk.0.attn_q.weight", 0, vec![1024, gib_rows / 2]),
                ("blk.0.ffn_up_exps.weight", 0, vec![1024, gib_rows / 2]),
                ("blk.1.attn_q.weight", 0, vec![1024, gib_rows / 2]),
                ("blk.1.ffn_up_exps.weight", 0, vec![1024, gib_rows / 2]),
            ],
        )
    }

    #[test]
    fn the_room_and_the_spill_checks_count_only_what_goes_on_the_gpu() {
        let fx = Fixture::with_main("ngram:2b", ngram_model());
        // 2 GiB free: the GPU part (2 GiB of blocks, 0.5 GiB of experts on the CPU) fits; the 8 GiB
        // table never goes there.
        let (e, vram, told, _) = with_gpu(&fx, Some(2.0 * 1024.0));
        let mut w = want("ngram:2b", Lane::Conductor, 1);
        w.shape.cpu_moe = 1;
        // The server holds the 1.5 GiB GPU part; the mapped table is the file's pages, not shared memory.
        *vram.process.lock().unwrap() = Some((1.5 * 1024.0, 200.0));
        e.acquire(&w, &CancelToken::new()).unwrap();
        assert_eq!((fx.spawner.starts(), fx.spawner.killed()), (1, 0), "{:?}", told.0.lock().unwrap());
        let a = args_of(&fx, 0);
        assert!(a.windows(2).any(|p| p == ["-ngl", "99"]) && a.windows(2).any(|p| p == ["--n-cpu-moe", "1"]), "{a:?}");
        assert!(a.windows(2).any(|p| p == ["--load-mode", "mmap"]) && a.windows(2).any(|p| p == ["--lazy-mode", "on"]), "{a:?}");
        assert!(told.0.lock().unwrap().is_empty());
        assert!(e.unload(None));

        // Read into RAM, the table and the experts are pinned host buffers the driver counts as
        // shared memory: 8.7 GiB of shared memory is what was asked for, not a spill.
        w.shape.host_ram = true;
        *vram.process.lock().unwrap() = Some((1.5 * 1024.0, 8.7 * 1024.0));
        e.acquire(&w, &CancelToken::new()).unwrap();
        assert_eq!((fx.spawner.starts(), fx.spawner.killed()), (2, 1), "only the unload stopped a server");
        assert!(args_of(&fx, 1).windows(2).any(|p| p == ["--load-mode", "none"]));
        assert!(told.0.lock().unwrap().is_empty(), "{:?}", told.0.lock().unwrap());
        assert!(e.unload(None));

        // Memory-mapped, the same shared memory is the driver's doing: a spill.
        w.shape.host_ram = false;
        e.acquire(&w, &CancelToken::new()).unwrap();
        assert!(told.0.lock().unwrap().iter().any(|t| t.contains("into system memory")), "{:?}", told.0.lock().unwrap());
    }

    #[test]
    fn the_room_check_without_an_estimate_counts_the_gpu_part_of_the_weights() {
        let fx = Fixture::with_main("ngram:2b", ngram_model());
        // 1.5 GiB free: the 2 GiB of blocks do not fit whole (the 8 GiB table is not counted).
        let (e, _vram, told, _) = with_gpu(&fx, Some(1.5 * 1024.0));
        e.acquire(&want("ngram:2b", Lane::Conductor, 1), &CancelToken::new()).unwrap();
        assert_eq!(told.0.lock().unwrap().clone(), vec!["ngram:2b: needs 2.5 GiB on the GPU, 1.5 GiB free".to_string()]);
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

    fn until(what: &str, mut done: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_new_backend_waits_for_the_request_in_flight_and_saves_its_slot() {
        let fx = Fixture::new("dusk:22b");
        let mut cfg = fx.config();
        cfg.slots_dir = Some(fx.dir.path().join("slots"));
        let e = LlamaEngine::new(cfg, fx.spawner.clone());
        let c = CancelToken::new();
        let lease = e.acquire(&want("dusk:22b", Lane::Conductor, 11), &c).unwrap();
        // The user picks the other backend (or a Sync finishes) while a reply is generating.
        e.restart_when_idle();
        std::thread::sleep(Duration::from_millis(250));
        assert_eq!(fx.spawner.killed(), 0, "the request in flight is not cut off");
        assert!(e.status().is_some());
        // A request that comes meanwhile waits for the new server, not the old one.
        let (e2, spawner) = (e.clone(), fx.spawner.clone());
        let next = std::thread::spawn(move || {
            drop(e2.acquire(&want("dusk:22b", Lane::Conductor, 11), &CancelToken::new()).unwrap());
            spawner.starts()
        });
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(fx.spawner.starts(), 1, "still waiting for the request in flight");
        drop(lease);
        assert_eq!(next.join().unwrap(), 2, "the new server serves it");
        assert_eq!(fx.spawner.killed(), 1, "the old one stopped when its request ended");
        let calls = fx.spawner.seen.slot_calls.lock().unwrap().clone();
        assert!(calls.iter().any(|c| c.starts_with("save ")), "the slot was saved before the stop: {calls:?}");
    }

    #[test]
    fn an_idle_server_of_the_old_backend_is_stopped_by_the_switch() {
        let fx = Fixture::new("dusk:22b");
        let e = fx.engine();
        drop(e.acquire(&want("dusk:22b", Lane::Conductor, 1), &CancelToken::new()).unwrap());
        e.restart_when_idle();
        assert!(e.status().is_none());
        assert_eq!(fx.spawner.killed(), 1);
    }

    #[test]
    fn a_switch_during_a_load_does_not_leave_the_old_backends_server_running() {
        let fx = Fixture::new("dusk:22b");
        fx.spawner.start_takes(Duration::from_millis(500));
        let e = fx.engine();
        let e2 = e.clone();
        let loading = std::thread::spawn(move || drop(e2.acquire(&want("dusk:22b", Lane::Conductor, 1), &CancelToken::new()).unwrap()));
        std::thread::sleep(Duration::from_millis(150));
        e.restart_when_idle();
        loading.join().unwrap();
        // The server that came up began before the switch: it goes when idle, and a request
        // after the switch starts another.
        until("the old server to stop", || fx.spawner.killed() == 1);
        drop(e.acquire(&want("dusk:22b", Lane::Conductor, 1), &CancelToken::new()).unwrap());
        assert_eq!(fx.spawner.starts(), 2);
    }

    #[test]
    fn on_a_one_slot_model_the_conversations_cache_survives_a_crew_turn() {
        let fx = Fixture::new("dusk:22b");
        let mut cfg = fx.config();
        cfg.slots_dir = Some(fx.dir.path().join("slots"));
        let e = LlamaEngine::new(cfg, fx.spawner.clone());
        let c = CancelToken::new();
        drop(e.acquire(&one_slot("dusk:22b", Lane::Conductor, 1), &c).unwrap());
        // A crew agent takes the idle slot: the conversation's cache is written out first.
        drop(e.acquire(&one_slot("dusk:22b", Lane::Other, 10), &c).unwrap());
        let calls = || fx.spawner.seen.slot_calls.lock().unwrap().clone();
        assert_eq!(calls().len(), 1, "{:?}", calls());
        assert!(calls()[0].starts_with("save 0 ") && calls()[0].contains("-k1.bin"), "{:?}", calls());
        // A second crew turn changes nothing about the conversation's file.
        drop(e.acquire(&one_slot("dusk:22b", Lane::Other, 10), &c).unwrap());
        assert_eq!(calls().len(), 1, "{:?}", calls());
        // The conversation returns: its cache is read back into the slot.
        drop(e.acquire(&one_slot("dusk:22b", Lane::Conductor, 1), &c).unwrap());
        assert!(calls().last().unwrap().starts_with("restore 0 ") && calls().last().unwrap().contains("-k1.bin"), "{:?}", calls());
        assert_eq!(fx.spawner.starts(), 1);
        // A stop with the crew on the slot keeps the conversation's cache, not the agent's.
        drop(e.acquire(&one_slot("dusk:22b", Lane::Other, 10), &c).unwrap());
        assert!(e.unload(None));
        let after = calls();
        assert!(after.iter().rev().take(2).any(|c| c.starts_with("restore 0 ")) && after.last().unwrap().starts_with("save 0 "), "{after:?}");
    }

    #[test]
    fn a_cache_saved_by_another_backend_or_version_is_not_restored() {
        let fx = Fixture::new("dusk:22b");
        let mut cfg = fx.config();
        cfg.slots_dir = Some(fx.dir.path().join("slots"));
        cfg.backend = Some(Backend::LlamaCpp);
        cfg.backend_tag = Some("v1.0.0".into());
        let c = CancelToken::new();
        let first = LlamaEngine::new(cfg.clone(), fx.spawner.clone());
        drop(first.acquire(&want("dusk:22b", Lane::Conductor, 11), &c).unwrap());
        assert!(first.unload(None));
        // The same backend and version gets it back.
        let same = LlamaEngine::new(cfg.clone(), fx.spawner.clone());
        drop(same.acquire(&want("dusk:22b", Lane::Conductor, 11), &c).unwrap());
        assert_eq!(fx.spawner.seen.slot_calls.lock().unwrap().iter().filter(|c| c.starts_with("restore ")).count(), 1);
        assert!(same.unload(None));
        // Another version, or the other backend, starts cold.
        for (backend, tag) in [(Backend::LlamaCpp, "v1.1.0"), (Backend::MsLlamaCpp, "ms-v1.0")] {
            let before = fx.spawner.seen.slot_calls.lock().unwrap().len();
            let mut other = cfg.clone();
            (other.backend, other.backend_tag) = (Some(backend), Some(tag.into()));
            let e = LlamaEngine::new(other, fx.spawner.clone());
            drop(e.acquire(&want("dusk:22b", Lane::Conductor, 11), &c).unwrap());
            assert!(fx.spawner.seen.slot_calls.lock().unwrap()[before..].iter().all(|c| !c.starts_with("restore ")), "{backend:?} {tag}");
            assert!(e.unload(None));
        }
    }

    /// An advisor that plans the model whole until the engine says it did not fit, then split.
    #[derive(Default)]
    struct Replans(Mutex<bool>);
    impl marlowe_model::EngineAdvisor for Replans {
        fn shape(&self, _: &str) -> Option<EngineShape> {
            Some(if *self.0.lock().unwrap() {
                EngineShape { slots: 1, ctx_per_slot: 4096, kv: "q8_0".into(), split: true, cpu_moe: 0, host_ram: false }
            } else {
                EngineShape { slots: 3, ctx_per_slot: 8192, kv: "q8_0".into(), split: false, cpu_moe: 0, host_ram: false }
            })
        }
        fn declined(&self, _: &str, _: &str) {
            *self.0.lock().unwrap() = true;
        }
    }

    #[test]
    fn a_model_made_split_by_a_misfit_is_not_loaded_twice() {
        let fx = Fixture::new("dusk:22b");
        let vram = FakeVram::new(Some(11.9 * 1024.0));
        let mut cfg = fx.config();
        cfg.vram = Some(vram);
        cfg.vram_settle = Duration::ZERO;
        let e = LlamaEngine::new(cfg, fx.spawner.clone());
        let alloc = ContextAllocations::new();
        alloc.set_engine_advisor(Arc::new(Replans::default()));
        e.follow(alloc.clone());
        let c = CancelToken::new();
        let ask = |alloc: &ContextAllocations| {
            let mut w = want("dusk:22b", Lane::Conductor, 1);
            w.shape = alloc.engine_shape("dusk:22b").unwrap();
            w.need_mib = Some(13.6 * 1024.0);
            w
        };
        // The first request asks for the whole shape; the check turns it into the planner's split one.
        drop(e.acquire(&ask(&alloc), &c).unwrap());
        let st = e.status().unwrap();
        assert!(st.split && st.shape.split && st.shape.slots == 1, "{st:?}");
        // The next request carries that shape (the planner now says split): the server is kept.
        let again = ask(&alloc);
        assert!(again.shape.split);
        drop(e.acquire(&again, &c).unwrap());
        assert_eq!((fx.spawner.starts(), fx.spawner.killed()), (1, 0));
    }
}
