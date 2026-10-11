//! Shared daemon state, startup and shutdown.

use std::any::{Any, TypeId};
use std::collections::hash_map::Entry as MapEntry;
use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, Sender};
use marlowe_agent::{Conversation, UserInfo};
use marlowe_brains::Brains;
use marlowe_model::Message;
use marlowe_proto::{
    auth, paths, ApprovalDecision, ApprovalId, DaemonInfo, Envelope, Event, LineReader, Request, SessionId,
    SessionSummary, StatusReport,
};

use crate::bus::Bus;
use crate::session::{self, Enqueue, SessionHandle};
use crate::store::{self, Store};
use crate::{lock, now_ms, DaemonConfig, DaemonDeps, DaemonError, ToolHostFactory};

pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Longest session title taken from the first user message.
const TITLE_CHARS: usize = 60;

/// Pending approvals, daemon-wide.
#[derive(Default)]
pub(crate) struct Approvals {
    next: AtomicU64,
    pending: Mutex<HashMap<ApprovalId, Sender<ApprovalDecision>>>,
}

impl Approvals {
    pub fn register(&self) -> (ApprovalId, Sender<ApprovalDecision>, Receiver<ApprovalDecision>) {
        let id = ApprovalId(self.next.fetch_add(1, Ordering::SeqCst) + 1);
        let (tx, rx) = bounded(1);
        lock(&self.pending).insert(id, tx.clone());
        (id, tx, rx)
    }

    /// Delivers a decision; false when `id` is not pending.
    pub fn resolve(&self, id: ApprovalId, decision: ApprovalDecision) -> bool {
        match lock(&self.pending).remove(&id) {
            Some(tx) => {
                let _ = tx.try_send(decision);
                true
            }
            None => false,
        }
    }

    pub fn remove(&self, id: ApprovalId) {
        lock(&self.pending).remove(&id);
    }

    pub fn deny_all(&self) {
        for (_, tx) in lock(&self.pending).drain() {
            let _ = tx.try_send(ApprovalDecision::Deny);
        }
    }
}

/// Daemon-lifetime state for later tracks, one value per type: E's per-session modes and rules,
/// F's schedules, F2's checkpoint store, J2's experiments, K's MCP registry. Each track keeps its
/// own type in its own file and reaches it with [`CoreExt::get_or_init`], so no track adds a field
/// to [`Core`].
// Unused until those tracks land.
#[allow(dead_code)]
#[derive(Default)]
pub(crate) struct CoreExt {
    map: Mutex<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
}

#[allow(dead_code)]
impl CoreExt {
    /// The value of type `T`, created by `init` on first use. `init` runs under the map's lock:
    /// keep it cheap and start slow work on a thread.
    pub fn get_or_init<T: Any + Send + Sync>(&self, init: impl FnOnce() -> T) -> Arc<T> {
        let mut map = lock(&self.map);
        let entry = map.entry(TypeId::of::<T>()).or_insert_with(|| Arc::new(init()));
        Arc::clone(entry).downcast::<T>().unwrap_or_else(|_| unreachable!("keyed by TypeId"))
    }

    /// The value of type `T`, if one was created.
    pub fn get<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        lock(&self.map).get(&TypeId::of::<T>()).cloned().and_then(|v| v.downcast::<T>().ok())
    }
}

pub(crate) struct Core {
    pub home: PathBuf,
    pub cfg: DaemonConfig,
    pub token: String,
    pub addr: SocketAddr,
    pub bus: Bus,
    pub store: Store,
    pub brains: Arc<Brains>,
    pub tools: ToolHostFactory,
    /// Name and address form; read with [`Core::user`], changed by `SetUser`.
    pub user: crate::user::UserCell,
    /// Serialises `SetUser` writes.
    pub user_write: Mutex<()>,
    /// The memory service, when the binary supplied one.
    pub mind: Option<marlowe_mind::Mind>,
    /// Legacy-offer cache and the import latch.
    pub memory: crate::mind_ops::MemoryState,
    /// The watchers, when the binary supplied them.
    pub sentinel: Option<marlowe_sentinel::Sentinel>,
    /// Spoken voice, when the binary supplied it.
    pub voice: Option<marlowe_voice::Voice>,
    /// The crew supervisor, once started (set right after the core exists, since its approvals
    /// and relays need the core).
    pub crew: OnceLock<crate::crew::CrewLink>,
    /// Which interactive clients are here, for the brief's away rule.
    pub presence: crate::brief::Presence,
    pub approvals: Approvals,
    /// State later tracks keep for the daemon's lifetime (see [`CoreExt`]).
    #[allow(dead_code)]
    pub ext: CoreExt,
    /// Cold loads the user approved this run, as `model|resident,…`: the same load beside the
    /// same models is not asked twice.
    pub approved_loads: Mutex<std::collections::HashSet<String>>,
    sessions: Mutex<HashMap<SessionId, Arc<SessionHandle>>>,
    started: Instant,
    shutting_down: AtomicBool,
    shutdown_tx: Sender<()>,
    next_client: AtomicU64,
    runners: Mutex<Vec<JoinHandle<()>>>,
    writers: Mutex<Vec<JoinHandle<()>>>,
}

/// Why a request failed: the session it concerns and a message for the client.
pub(crate) type Refusal = (Option<SessionId>, String);

impl Core {
    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    pub fn request_shutdown(&self) {
        if !self.shutting_down.swap(true, Ordering::SeqCst) {
            let _ = self.shutdown_tx.send(());
        }
    }

    /// The user as of now.
    pub fn user(&self) -> UserInfo {
        self.user.get()
    }

    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    /// A foreground turn or a crew run is using the models now: background work stays off.
    pub fn foreground_busy(&self) -> bool {
        if lock(&self.sessions).values().any(|h| h.is_busy()) {
            return true;
        }
        self.crew.get().is_some_and(|c| c.supervisor.running() > 0)
    }

    /// True while `id` has a running session runner (its conversation lives in memory).
    pub fn is_live(&self, id: &SessionId) -> bool {
        lock(&self.sessions).contains_key(id)
    }

    pub fn next_client_id(&self) -> u64 {
        self.next_client.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn add_writer(&self, h: JoinHandle<()>) {
        let mut w = lock(&self.writers);
        w.retain(|h| !h.is_finished());
        w.push(h);
    }

    pub fn status(&self) -> StatusReport {
        let mut active: Vec<SessionId> =
            lock(&self.sessions).values().filter(|h| h.is_busy()).map(|h| h.id.clone()).collect();
        active.sort();
        StatusReport {
            daemon_version: VERSION.into(),
            pid: std::process::id(),
            uptime_ms: self.started.elapsed().as_millis() as u64,
            clients: self.bus.client_count() as u32,
            active_sessions: active,
            seq: self.bus.seq(),
            profile_home: self.home.display().to_string(),
        }
    }

    /// Creates a session, announces it to everyone, and confirms it to `client` with `seq: 0`.
    pub fn create_session(self: &Arc<Self>, client: u64, workspace: Option<String>) -> Result<Arc<SessionHandle>, Refusal> {
        if self.is_shutting_down() {
            return Err((None, "the daemon is shutting down".into()));
        }
        let now = now_ms();
        let id = SessionId::new_random();
        let summary = SessionSummary {
            id: id.clone(),
            title: String::new(),
            workspace: workspace.clone(),
            created_ms: now,
            updated_ms: now,
            turns: 0,
        };
        self.store.create(summary).map_err(|e| (None, format!("could not create a session: {e}")))?;
        let convo = Conversation::new(id.clone(), workspace.as_ref().map(PathBuf::from));
        let handle = self.spawn_session(convo, String::new(), workspace.clone());
        let opened = Event::SessionOpened { session: id, title: String::new(), workspace };
        self.bus.publish(opened.clone());
        self.bus.reply(client, opened);
        Ok(handle)
    }

    /// The live handle for `id`, loading the session from the store if it is not active.
    pub fn session(self: &Arc<Self>, id: &SessionId) -> Result<Arc<SessionHandle>, Refusal> {
        if let Some(h) = lock(&self.sessions).get(id) {
            return Ok(h.clone());
        }
        let refuse = |m: String| (Some(id.clone()), m);
        let summary = self.store.get(id).map_err(refuse)?.ok_or_else(|| refuse(format!("unknown session {id}")))?;
        let turns = self.store.load(id).map_err(refuse)?;
        let messages = turns.into_iter().map(|t| Message { role: t.role, content: t.content }).collect();
        let workspace = summary.workspace.clone();
        let mut convo = Conversation::from_messages(id.clone(), workspace.as_ref().map(PathBuf::from), messages, summary.turns);
        convo.exposure.tainted = self.store.tainted(id).map_err(refuse)?;
        Ok(self.spawn_session(convo, summary.title, workspace))
    }

    /// Registers a handle and starts its runner, unless another client registered the session
    /// first; then that handle is returned and `convo` is dropped.
    fn spawn_session(self: &Arc<Self>, convo: Conversation, title: String, workspace: Option<String>) -> Arc<SessionHandle> {
        let (handle, cmd_rx) = SessionHandle::new(convo.session.clone(), title, workspace);
        let handle = Arc::new(handle);
        match lock(&self.sessions).entry(handle.id.clone()) {
            MapEntry::Occupied(e) => return e.get().clone(),
            MapEntry::Vacant(v) => {
                v.insert(handle.clone());
            }
        }
        let join = session::spawn_runner(self.clone(), handle.clone(), convo, cmd_rx);
        let mut runners = lock(&self.runners);
        runners.retain(|h| !h.is_finished());
        runners.push(join);
        handle
    }

    /// Called by an idle runner: unregisters `handle` if it is still the live one and not busy.
    /// Returns true when the runner should exit.
    pub fn evict_if_idle(&self, handle: &Arc<SessionHandle>) -> bool {
        let mut map = lock(&self.sessions);
        let current = map.get(&handle.id).is_some_and(|h| Arc::ptr_eq(h, handle));
        // A session with background jobs running or notices waiting keeps its tool host, which
        // owns the jobs (track B).
        if crate::job_ops::keeps_session(&handle.id) {
            return false;
        }
        if !current {
            return handle.close_if_idle();
        }
        if !handle.close_if_idle() {
            return false;
        }
        map.remove(&handle.id);
        true
    }

    /// Called by a runner that is exiting for any reason: closes and unregisters its handle.
    pub fn release(&self, handle: &Arc<SessionHandle>) {
        let mut map = lock(&self.sessions);
        handle.close();
        if map.get(&handle.id).is_some_and(|h| Arc::ptr_eq(h, handle)) {
            map.remove(&handle.id);
        }
    }

    /// Handles `Ask`: starts a turn, or queues the text when the session is busy.
    pub fn ask(self: &Arc<Self>, client: u64, session: Option<SessionId>, text: String, workspace: Option<String>) -> Result<(), Refusal> {
        if text.trim().is_empty() {
            return Err((session, "nothing to send".into()));
        }
        if self.is_shutting_down() {
            return Err((session, "the daemon is shutting down".into()));
        }
        let handle = match session {
            None => self.create_session(client, workspace)?,
            Some(id) => self.session(&id)?,
        };
        if let Some(title) = handle.take_title(&text, TITLE_CHARS) {
            if let Err(e) = self.store.set_title(&handle.id, &title) {
                self.bus.publish(Event::Error { session: Some(handle.id.clone()), message: format!("title not saved: {e}") });
            }
            self.bus.publish(Event::SessionOpened {
                session: handle.id.clone(),
                title,
                workspace: handle.workspace.clone(),
            });
        }
        let refuse = |m: String| (Some(handle.id.clone()), m);
        match handle.enqueue(text.clone()) {
            Ok(()) => Ok(()),
            // The runner went idle and exited after we looked it up: load it again.
            Err(Enqueue::Closed) => self.session(&handle.id)?.enqueue(text).map_err(|e| refuse(e.to_string())),
            Err(e) => Err(refuse(e.to_string())),
        }
    }

    pub fn interrupt(&self, id: &SessionId) {
        let h = lock(&self.sessions).get(id).cloned();
        if let Some(h) = h {
            h.interrupt();
        }
    }

    /// Replies with the session header and its stored transcript.
    pub fn open_session(&self, client: u64, id: &SessionId) -> Result<(), Refusal> {
        let refuse = |m: String| (Some(id.clone()), m);
        let summary = self.store.get(id).map_err(refuse)?.ok_or_else(|| refuse(format!("unknown session {id}")))?;
        let turns = self.store.load(id).map_err(refuse)?;
        self.bus.reply(client, Event::SessionOpened { session: id.clone(), title: summary.title, workspace: summary.workspace });
        self.bus.reply(client, Event::Transcript { session: id.clone(), turns });
        Ok(())
    }
}

/// A running daemon. [`crate::run`] is `start` + `wait`; tests drive it directly.
pub struct Daemon {
    core: Arc<Core>,
    shutdown_rx: Receiver<()>,
    acceptor: Option<JoinHandle<()>>,
    store_thread: Option<JoinHandle<()>>,
}

impl Daemon {
    /// Opens the profile home's databases, binds `127.0.0.1:0`, writes `daemon.json`, and starts
    /// accepting clients. Fails with `AlreadyRunning` if a live daemon answers for `home`.
    pub fn start(deps: DaemonDeps, home: &Path, cfg: DaemonConfig) -> Result<Daemon, DaemonError> {
        std::fs::create_dir_all(home)?;
        if let Ok(Some(info)) = DaemonInfo::read(home) {
            if probe(&info) {
                return Err(DaemonError::AlreadyRunning(home.display().to_string()));
            }
        }
        let db = paths::cowork_db(home);
        let (store, store_thread) = store::open(&db)?;
        let brains = match Brains::open(&db, deps.providers) {
            Ok(b) => Arc::new(b),
            Err(e) => {
                store.close();
                let _ = store_thread.join();
                return Err(e.into());
            }
        };
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let addr = listener.local_addr()?;
        let token = auth::mint_token()?;
        let (shutdown_tx, shutdown_rx) = bounded(1);
        let bus = Bus::new(cfg.replay_ring);
        let user_set = crate::user::path(home).exists() || deps.user != UserInfo::default();
        let core = Arc::new(Core {
            home: home.to_path_buf(),
            cfg,
            token: token.clone(),
            addr,
            bus,
            store,
            brains,
            tools: deps.tools,
            user: crate::user::UserCell::new(deps.user, user_set),
            user_write: Mutex::new(()),
            mind: deps.mind,
            memory: Default::default(),
            sentinel: deps.sentinel.as_ref().map(|l| l.sentinel.clone()),
            voice: deps.voice.as_ref().map(|l| l.voice.clone()),
            crew: OnceLock::new(),
            presence: Default::default(),
            approvals: Approvals::default(),
            ext: CoreExt::default(),
            approved_loads: Mutex::new(Default::default()),
            sessions: Mutex::new(HashMap::new()),
            started: Instant::now(),
            shutting_down: AtomicBool::new(false),
            shutdown_tx,
            next_client: AtomicU64::new(0),
            runners: Mutex::new(Vec::new()),
            writers: Mutex::new(Vec::new()),
        });
        if let Some(link) = deps.sentinel {
            core.add_writer(crate::watch_ops::forward_sentinel(core.clone(), link.events)?);
        }
        if let Some(link) = deps.voice {
            core.add_writer(crate::watch_ops::forward_voice(core.clone(), link.events)?);
        }
        if let Some(setup) = deps.crew {
            // A crew that will not start leaves the daemon without one, said in the daemon log.
            match crate::crew::start(&core, setup) {
                Ok(forwarder) => {
                    core.add_writer(forwarder);
                    // Results a previous daemon finished but never delivered.
                    crate::crew::relay_due(&core);
                }
                Err(e) => eprintln!("marlowe: {e}"),
            }
        }
        // Per-track start hooks (schedules, experiment deadlines, MCP servers).
        crate::boot::run(&core);
        DaemonInfo { port: addr.port(), token, pid: std::process::id() }.write(home)?;
        let acore = core.clone();
        let acceptor = std::thread::Builder::new().name("marlowe-accept".into()).spawn(move || {
            for conn in listener.incoming() {
                if acore.is_shutting_down() {
                    break;
                }
                match conn {
                    Ok(stream) => {
                        let c = acore.clone();
                        let _ = std::thread::Builder::new()
                            .name("marlowe-client".into())
                            .spawn(move || crate::client::serve(c, stream));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(20)),
                }
            }
        })?;
        Ok(Daemon { core, shutdown_rx, acceptor: Some(acceptor), store_thread: Some(store_thread) })
    }

    pub fn addr(&self) -> SocketAddr {
        self.core.addr
    }

    /// What `daemon.json` says.
    pub fn info(&self) -> DaemonInfo {
        DaemonInfo { port: self.core.addr.port(), token: self.core.token.clone(), pid: std::process::id() }
    }

    pub fn home(&self) -> &Path {
        &self.core.home
    }

    /// Asks the daemon to stop, as `Request::Shutdown` does. `wait` performs the shutdown.
    pub fn shutdown(&self) {
        self.core.request_shutdown();
    }

    /// Blocks until shutdown is requested, then stops running turns (persisting them), denies
    /// pending approvals, shuts the memory service down (final snapshot), announces
    /// `ShuttingDown`, flushes clients, and removes `daemon.json`.
    pub fn wait(mut self) -> Result<(), DaemonError> {
        let _ = self.shutdown_rx.recv();
        let core = self.core.clone();
        core.shutting_down.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect_timeout(&core.addr, Duration::from_secs(1));
        if let Some(a) = self.acceptor.take() {
            let _ = a.join();
        }
        let handles: Vec<Arc<SessionHandle>> = lock(&core.sessions).values().cloned().collect();
        for h in &handles {
            h.stop();
        }
        // Before approvals are denied: a run waiting on one sees the stop, not a refusal.
        crate::crew::shutdown(&core);
        core.approvals.deny_all();
        if let Some(v) = &core.voice {
            v.shutdown();
        }
        if let Some(s) = &core.sentinel {
            s.shutdown();
        }
        let runners: Vec<JoinHandle<()>> = lock(&core.runners).drain(..).collect();
        join_within(runners, core.cfg.drain_timeout);
        // After the runners: their last turns are ingested before the final snapshot.
        if let Some(mind) = &core.mind {
            if let Err(e) = mind.shutdown() {
                core.bus.publish(Event::Error { session: None, message: format!("memory did not shut down cleanly: {e}") });
            }
        }
        core.bus.publish(Event::ShuttingDown);
        core.bus.close_all();
        let writers: Vec<JoinHandle<()>> = lock(&core.writers).drain(..).collect();
        join_within(writers, Duration::from_secs(3));
        core.store.close();
        if let Some(s) = self.store_thread.take() {
            let _ = s.join();
        }
        DaemonInfo::remove_if_ours(&core.home, std::process::id())?;
        Ok(())
    }
}

/// Joins what finishes before `limit`; the rest are left detached.
fn join_within(handles: Vec<JoinHandle<()>>, limit: Duration) {
    let deadline = Instant::now() + limit;
    for h in handles {
        while !h.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if h.is_finished() {
            let _ = h.join();
        }
    }
}

/// True when a daemon answers `info`'s port with a Welcome for its token.
fn probe(info: &DaemonInfo) -> bool {
    let Ok(mut s) = TcpStream::connect_timeout(&info.addr(), Duration::from_millis(500)) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let hello = Request::Hello { token: info.token.clone(), client: "probe".into(), since_seq: None };
    if marlowe_proto::write_line(&mut s, &hello).is_err() {
        return false;
    }
    let Ok(read) = s.try_clone() else { return false };
    let ok = matches!(
        LineReader::new(BufReader::new(read)).next::<Envelope>(),
        Ok(Some(Envelope { event: Event::Welcome { .. }, .. }))
    );
    let _ = s.flush();
    ok
}

#[cfg(test)]
mod ext_tests {
    use super::CoreExt;

    #[derive(Default)]
    struct Modes(std::sync::Mutex<Vec<String>>);

    #[test]
    fn core_ext_keeps_one_value_per_type_for_the_daemons_life() {
        let ext = CoreExt::default();
        assert!(ext.get::<Modes>().is_none());
        ext.get_or_init(Modes::default).0.lock().unwrap().push("plan".into());
        let again = ext.get_or_init(|| -> Modes { unreachable!("created once") });
        assert_eq!(*again.0.lock().unwrap(), vec!["plan".to_string()]);
        assert_eq!(*ext.get_or_init(|| 7u32), 7);
        assert!(ext.get::<Modes>().is_some());
    }
}
