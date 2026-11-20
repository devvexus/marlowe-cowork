//! The llama.cpp backends: listing them, choosing one, installing or upgrading, looking for
//! newer builds, and the once-a-day release offer. The work is `marlowe-engine-install`; this is
//! the requests, the events and the engine stop around a switch.
//!
//! [`Services`] holds everything outside the daemon (the engines folder, the GitHub transport,
//! the process runner) and talks to clients through a [`Sink`], so tests run it with fakes.

use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use marlowe_engine_install::discover::{self, Depth, Platform};
use marlowe_engine_install::http::Http;
use marlowe_engine_install::install::{self, Context, Target};
use marlowe_engine_install::smoke::{Runner, SystemRunner};
use marlowe_engine_install::EngineError;
use marlowe_engines::{status, Backend, Device, Update};
use marlowe_net::download::{Net, Transport};
use marlowe_proto::{BackendBuild, BackendBusy, BackendOp, BackendReport, BackendUpdate, Event, RemoteTag, Request};

use crate::daemon::Core;
use crate::lock;

/// Progress is broadcast at most this often.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);
/// The daily check waits this long after the daemon starts, so a window is there to ask.
const OFFER_DELAY: Duration = Duration::from_secs(20);
/// How often the daemon looks at whether a day has passed since the last check.
const OFFER_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

/// Environment variable naming a folder to download release zips into and keep (a shared cache).
pub const DOWNLOADS_ENV: &str = "MARLOWE_ENGINES_DOWNLOADS";

/// Where events and replies go.
pub(crate) trait Sink {
    /// To every client, kept for replay.
    fn publish(&self, event: Event);
    /// To every client, not kept (progress).
    fn ephemeral(&self, event: Event);
    /// To the client that asked.
    fn reply(&self, event: Event);
    /// Stops the running llama-server (saving its slot caches) so the next load starts the
    /// selected backend's current version.
    fn stop_engine(&self);
}

pub(crate) struct Services {
    root: Option<PathBuf>,
    downloads: PathBuf,
    keep_downloads: bool,
    transport: Arc<dyn Transport + Send + Sync>,
    runner: Arc<dyn Runner>,
    /// Fixed in tests; detected (architecture, driver) on first use otherwise.
    platform: Option<Platform>,
    token: Option<String>,
    busy: Mutex<HashMap<Backend, BackendBusy>>,
    notes: Mutex<HashMap<Backend, String>>,
}

impl Services {
    /// The real ones: GitHub over TLS, `llama-server` run hidden with the credential variables removed.
    pub(crate) fn real() -> Self {
        let root = marlowe_engines::root_from(marlowe_mind::install_dir());
        let shared = std::env::var_os(DOWNLOADS_ENV).filter(|v| !v.is_empty()).map(PathBuf::from);
        let scrub = marlowe_brains::scrubbed_env().iter().map(|s| s.to_string()).collect();
        Self {
            downloads: shared.clone().or_else(|| root.as_ref().map(|r| r.join("downloads"))).unwrap_or_else(|| std::env::temp_dir().join("marlowe-engine-downloads")),
            keep_downloads: shared.is_some(),
            root,
            transport: Arc::new(Net),
            runner: Arc::new(SystemRunner::new(scrub)),
            platform: None,
            token: None,
            busy: Mutex::default(),
            notes: Mutex::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with(root: &Path, downloads: &Path, transport: Arc<dyn Transport + Send + Sync>, runner: Arc<dyn Runner>, platform: Platform) -> Self {
        Self {
            root: Some(root.to_path_buf()),
            downloads: downloads.to_path_buf(),
            keep_downloads: false,
            transport,
            runner,
            platform: Some(platform),
            token: None,
            busy: Mutex::default(),
            notes: Mutex::default(),
        }
    }

    fn root(&self) -> Result<&Path, String> {
        self.root.as_deref().ok_or_else(|| "there is no install folder (LOCALAPPDATA is not set), so backends cannot be installed".to_string())
    }

    fn platform(&self) -> Result<Platform, EngineError> {
        match self.platform {
            Some(p) => Ok(p),
            None => Platform::detect(),
        }
    }

    fn http(&self) -> Http<'_> {
        match &self.token {
            Some(t) => Http::new(&*self.transport, Some(t.clone())),
            None => Http::from_env(&*self.transport),
        }
    }

    // ------------------------------------------------------------- reports

    /// Every backend's state for the window.
    pub(crate) fn reports(&self) -> Vec<BackendReport> {
        Backend::ALL.iter().map(|b| self.report(*b)).collect()
    }

    fn report(&self, backend: Backend) -> BackendReport {
        let mut r = BackendReport {
            id: backend.id().into(),
            title: backend.title().into(),
            description: backend.description().into(),
            repo: backend.repo().into(),
            busy: lock(&self.busy).get(&backend).cloned(),
            note: lock(&self.notes).get(&backend).cloned(),
            ..Default::default()
        };
        let Ok(root) = self.root() else {
            r.state = BackendUpdate::NotInstalled;
            r.selected = backend == marlowe_engines::DEFAULT;
            return r;
        };
        let s = status(root, backend);
        r.selected = s.selected;
        r.state = match s.update {
            Update::NotInstalled => BackendUpdate::NotInstalled,
            Update::UpToDate => BackendUpdate::UpToDate,
            Update::UpdateAvailable => BackendUpdate::UpdateAvailable,
            Update::Unknown => BackendUpdate::Unknown,
        };
        r.installed = s.installed.as_ref().map(|i| BackendBuild {
            tag: i.tag.clone(),
            label: i.label(),
            release: i.release.clone(),
            build: i.build.clone(),
            date: i.published.clone(),
            device: if i.device == Device::Gpu { "gpu" } else { "cpu" }.into(),
            note: i.note.clone().or_else(|| i.version.clone()),
        });
        r.rollback = s.rollback;
        let tag = |t: Option<(String, String)>| t.map(|(tag, date)| RemoteTag { tag, date: Some(date).filter(|d| !d.is_empty()) });
        r.latest_release = tag(s.latest_release);
        r.newest_build = tag(s.newest_build);
        r.builds_newer = s.builds_newer;
        r.last_checked = s.last_checked;
        r.dont_ask = s.dont_ask;
        r
    }

    fn publish_all(&self, sink: &dyn Sink) {
        sink.publish(Event::Backends { selected: marlowe_engines::state::selected(self.root().unwrap_or(Path::new(""))).id().into(), backends: self.reports() });
    }

    fn note(&self, backend: Backend, text: Option<String>) {
        let mut n = lock(&self.notes);
        match text {
            Some(t) => n.insert(backend, t),
            None => n.remove(&backend),
        };
    }

    /// Marks `backend` busy with `op`; `None` when it already is.
    fn begin(&self, backend: Backend, op: BackendOp) -> Option<Busy<'_>> {
        let mut b = lock(&self.busy);
        if b.contains_key(&backend) {
            return None;
        }
        b.insert(backend, BackendBusy { op, message: String::new(), done: 0, total: 0 });
        Some(Busy { services: self, backend })
    }

    // ------------------------------------------------------------- requests

    pub(crate) fn list(&self, sink: &dyn Sink) {
        sink.reply(Event::Backends { selected: self.selected_id(), backends: self.reports() });
    }

    fn selected_id(&self) -> String {
        self.root().map(|r| marlowe_engines::state::selected(r).id().to_string()).unwrap_or_else(|_| marlowe_engines::DEFAULT.id().to_string())
    }

    pub(crate) fn select(&self, sink: &dyn Sink, backend: Backend) {
        let done = |ok: bool, message: String| sink.publish(Event::BackendDone { backend: backend.id().into(), op: BackendOp::Select, ok, message });
        let changed = match self.root().map(|r| marlowe_engines::state::select(r, backend)) {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => return done(false, format!("could not save the choice: {e}")),
            Err(m) => return done(false, m),
        };
        if changed {
            // The running server belongs to the old backend: stop it (its slot caches are saved
            // first); the next load starts the new one.
            sink.stop_engine();
        }
        self.publish_all(sink);
        let installed = self.root().map(|r| marlowe_engines::installed::server(r, backend).is_ok()).unwrap_or(false);
        done(
            true,
            if installed { format!("{backend} serves models now") } else { format!("{backend} is selected but not installed yet; install it to load models") },
        );
    }

    /// `Install` (the newest release: first install or upgrade) or `Sync` (the newest build).
    pub(crate) fn install(&self, sink: &dyn Sink, backend: Backend, op: BackendOp) {
        let name = backend.id().to_string();
        let end = |ok: bool, message: String| {
            sink.publish(Event::BackendDone { backend: name.clone(), op, ok, message });
        };
        let root = match self.root() {
            Ok(r) => r,
            Err(m) => return end(false, m),
        };
        let Some(_busy) = self.begin(backend, op) else {
            return end(false, format!("{backend} is already being worked on"));
        };
        let platform = match self.platform() {
            Ok(p) => p,
            Err(e) => return end(false, e.to_string()),
        };
        self.note(backend, None);
        self.publish_all(sink);
        let http = self.http();
        let ctx = Context { root, downloads: &self.downloads, http: &http, runner: &*self.runner, platform, keep_downloads: self.keep_downloads };
        let target = if op == BackendOp::Sync { Target::Build } else { Target::Release };
        let before = marlowe_engines::state::load(root).of(backend).current;
        let mut last = Instant::now() - PROGRESS_EVERY;
        let result = install::install(&ctx, backend, target, &mut |p| {
            self.set_busy(backend, op, &p.message, p.done, p.total);
            if last.elapsed() >= PROGRESS_EVERY || (p.total > 0 && p.done >= p.total) {
                last = Instant::now();
                sink.ephemeral(Event::BackendProgress { backend: name.clone(), op, message: p.message, done: p.done, total: p.total });
            }
        });
        drop(_busy);
        match result {
            Ok(rec) => {
                let moved = before.as_deref() != Some(rec.tag.as_str());
                // A server of the old version is still running for the selected backend: stop it so
                // the next load uses what was just installed.
                if moved && marlowe_engines::state::selected(root) == backend {
                    sink.stop_engine();
                }
                let cpu = if rec.device == Device::Cpu { format!(" ({})", rec.note.clone().unwrap_or_default()) } else { String::new() };
                self.publish_all(sink);
                end(true, format!("{backend} {} is installed{cpu}", rec.label()));
            }
            Err(e) => {
                self.note(backend, Some(e.to_string()));
                self.publish_all(sink);
                end(false, e.to_string());
            }
        }
    }

    /// "Check for updates": the newest release and build, answered with the backends.
    pub(crate) fn check(&self, sink: &dyn Sink, backend: Backend) {
        let end = |ok: bool, message: String| sink.publish(Event::BackendDone { backend: backend.id().into(), op: BackendOp::Check, ok, message });
        let root = match self.root() {
            Ok(r) => r,
            Err(m) => return end(false, m),
        };
        let Some(_busy) = self.begin(backend, BackendOp::Check) else {
            return end(false, format!("{backend} is already being worked on"));
        };
        let platform = match self.platform() {
            Ok(p) => p,
            Err(e) => return end(false, e.to_string()),
        };
        self.publish_all(sink);
        let http = self.http();
        let result = discover::check(root, backend, &http, platform, Depth::Build, marlowe_engines::now_secs());
        drop(_busy);
        match result {
            Ok(c) => {
                self.note(backend, c.note.clone());
                self.publish_all(sink);
                end(true, c.note.unwrap_or_else(|| "looked at the newest release and build".into()));
            }
            Err(e) => {
                self.note(backend, Some(e.to_string()));
                self.publish_all(sink);
                end(false, e.to_string());
            }
        }
    }

    pub(crate) fn dont_ask(&self, sink: &dyn Sink, backend: Backend, on: bool) {
        match self.root().map(|r| marlowe_engines::state::set_dont_ask(r, backend, on)) {
            Ok(Ok(())) => self.publish_all(sink),
            Ok(Err(e)) => sink.reply(Event::Error { session: None, message: format!("could not save: {e}") }),
            Err(m) => sink.reply(Event::Error { session: None, message: m }),
        }
    }

    /// The release prompt, for the selected backend only: at most once a day, releases only.
    pub(crate) fn offer(&self, sink: &dyn Sink) {
        let (Ok(root), Ok(platform)) = (self.root(), self.platform()) else { return };
        let backend = marlowe_engines::state::selected(root);
        let http = self.http();
        if let Ok(Some(tag)) = discover::upgrade_offer(root, backend, &http, platform, marlowe_engines::now_secs()) {
            self.publish_all(sink);
            sink.publish(Event::BackendUpgrade { backend: backend.id().into(), title: backend.title().into(), tag });
        }
    }

    fn set_busy(&self, backend: Backend, op: BackendOp, message: &str, done: u64, total: u64) {
        lock(&self.busy).insert(backend, BackendBusy { op, message: message.to_string(), done, total });
    }
}

/// Clears the busy mark when the operation ends, a panic included.
struct Busy<'a> {
    services: &'a Services,
    backend: Backend,
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        lock(&self.services.busy).remove(&self.backend);
    }
}

// ------------------------------------------------------------------ the daemon side

struct CoreSink<'a> {
    core: &'a Core,
    client: u64,
}

impl Sink for CoreSink<'_> {
    fn publish(&self, event: Event) {
        self.core.bus.publish(event);
    }
    fn ephemeral(&self, event: Event) {
        self.core.bus.publish_ephemeral(event);
    }
    fn reply(&self, event: Event) {
        self.core.bus.reply(self.client, event);
    }
    fn stop_engine(&self) {
        let _ = self.core.brains.unload_local_models();
    }
}

fn services(core: &Core) -> Arc<Services> {
    core.ext.get_or_init(Services::real)
}

/// Handles a backend request on a worker thread: an install runs for minutes.
pub(crate) fn dispatch(core: &Arc<Core>, client: u64, req: Request) {
    let c = core.clone();
    let spawned = std::thread::Builder::new().name("marlowe-engines".into()).spawn(move || {
        if catch_unwind(AssertUnwindSafe(|| handle(&c, client, req))).is_err() {
            c.bus.reply(client, Event::Error { session: None, message: "the backend request failed".into() });
        }
    });
    if spawned.is_err() {
        core.bus.reply(client, Event::Error { session: None, message: "the daemon is out of threads".into() });
    }
}

fn handle(core: &Core, client: u64, req: Request) {
    let sink = CoreSink { core, client };
    let svc = services(core);
    let backend = |id: &str| match Backend::parse(id) {
        Some(b) => Some(b),
        None => {
            let known = Backend::ALL.map(|b| b.id()).join(", ");
            sink.reply(Event::Error { session: None, message: format!("{id:?} is not a backend; the backends are {known}") });
            None
        }
    };
    match req {
        Request::Backends => svc.list(&sink),
        Request::BackendSelect { backend: id } => {
            if let Some(b) = backend(&id) {
                svc.select(&sink, b);
            }
        }
        Request::BackendInstall { backend: id } => {
            if let Some(b) = backend(&id) {
                svc.install(&sink, b, BackendOp::Install);
            }
        }
        Request::BackendSync { backend: id } => {
            if let Some(b) = backend(&id) {
                svc.install(&sink, b, BackendOp::Sync);
            }
        }
        Request::BackendCheck { backend: id } => {
            if let Some(b) = backend(&id) {
                svc.check(&sink, b);
            }
        }
        Request::BackendDontAsk { backend: id, dont_ask } => {
            if let Some(b) = backend(&id) {
                svc.dont_ask(&sink, b, dont_ask);
            }
        }
        _ => sink.reply(Event::Error { session: None, message: "not a backend request".into() }),
    }
}

/// The once-a-day release check, off unless the daemon was started for a person
/// (`DaemonConfig::engine_updates`). It never blocks start-up and never prompts for a build.
pub(crate) fn on_boot(core: &Arc<Core>) {
    if !core.cfg.engine_updates {
        return;
    }
    let c = core.clone();
    let _ = std::thread::Builder::new().name("marlowe-engine-offer".into()).spawn(move || {
        let mut wait = OFFER_DELAY;
        loop {
            // Sleeps in steps so a shutdown is not held up by it.
            let until = Instant::now() + wait;
            while Instant::now() < until {
                if c.is_shutting_down() {
                    return;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            if c.is_shutting_down() {
                return;
            }
            let _ = catch_unwind(AssertUnwindSafe(|| services(&c).offer(&CoreSink { core: &c, client: 0 })));
            wait = OFFER_EVERY;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use marlowe_engine_install::smoke::RunOutput;
    use marlowe_net::download::Reply;
    use std::io::Cursor;

    #[derive(Default)]
    struct Recorder {
        events: Mutex<Vec<Event>>,
        stops: Mutex<u32>,
    }

    impl Sink for Recorder {
        fn publish(&self, e: Event) {
            self.events.lock().unwrap().push(e);
        }
        fn ephemeral(&self, e: Event) {
            self.events.lock().unwrap().push(e);
        }
        fn reply(&self, e: Event) {
            self.events.lock().unwrap().push(e);
        }
        fn stop_engine(&self) {
            *self.stops.lock().unwrap() += 1;
        }
    }

    impl Recorder {
        fn last_backends(&self) -> (String, Vec<BackendReport>) {
            self.events
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find_map(|e| match e {
                    Event::Backends { selected, backends } => Some((selected.clone(), backends.clone())),
                    _ => None,
                })
                .expect("a Backends event")
        }
        fn done(&self) -> Vec<(BackendOp, bool, String)> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter_map(|e| match e {
                    Event::BackendDone { op, ok, message, .. } => Some((*op, *ok, message.clone())),
                    _ => None,
                })
                .collect()
        }
    }

    /// GitHub that answers every release call with `404` (a repo with nothing published).
    struct Nothing;
    impl Transport for Nothing {
        fn get(&self, _: &str) -> Result<Reply, String> {
            Ok(Reply::new(404, None, Box::new(Cursor::new(b"{}".to_vec()))))
        }
    }

    struct NoRun;
    impl Runner for NoRun {
        fn run(&self, _: &Path, _: &[&str]) -> Result<RunOutput, String> {
            Err("not run".into())
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("marlowe-engine-ops-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn services_in(root: &Path, transport: Arc<dyn Transport + Send + Sync>) -> Services {
        let p = Platform { arch: marlowe_engines::Arch::X64, cuda: marlowe_engines::Cuda::V12_4 };
        Services::with(root, &root.join("dl"), transport, Arc::new(NoRun), p)
    }

    #[test]
    fn both_backends_are_listed_with_stock_llama_cpp_selected_and_nothing_installed() {
        let root = scratch("list");
        let svc = services_in(&root, Arc::new(Nothing));
        let sink = Recorder::default();
        svc.list(&sink);
        let (selected, backends) = sink.last_backends();
        assert_eq!(selected, "llama.cpp");
        let ids: Vec<&str> = backends.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, ["llama.cpp", "ms-llama.cpp"]);
        assert!(backends[0].selected && !backends[1].selected);
        assert!(backends.iter().all(|b| b.state == BackendUpdate::NotInstalled && b.installed.is_none()));
        assert_eq!(backends[1].repo, "devvexus/ms-llama.cpp");
        assert!(backends[1].description.contains("MS-capable"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn selecting_the_other_backend_stops_the_engine_once_and_is_remembered() {
        let root = scratch("select");
        let svc = services_in(&root, Arc::new(Nothing));
        let sink = Recorder::default();
        svc.select(&sink, Backend::MsLlamaCpp);
        assert_eq!(*sink.stops.lock().unwrap(), 1);
        let (selected, backends) = sink.last_backends();
        assert_eq!(selected, "ms-llama.cpp");
        assert!(backends[1].selected && !backends[0].selected);
        let done = sink.done();
        assert!(done[0].1 && done[0].2.contains("not installed yet"), "{done:?}");
        // The same choice again does not stop the server again.
        svc.select(&sink, Backend::MsLlamaCpp);
        assert_eq!(*sink.stops.lock().unwrap(), 1);
        assert_eq!(marlowe_engines::state::selected(&root), Backend::MsLlamaCpp);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dont_ask_is_saved_and_shown() {
        let root = scratch("ask");
        let svc = services_in(&root, Arc::new(Nothing));
        let sink = Recorder::default();
        svc.dont_ask(&sink, Backend::LlamaCpp, true);
        assert!(sink.last_backends().1[0].dont_ask);
        svc.dont_ask(&sink, Backend::LlamaCpp, false);
        assert!(!sink.last_backends().1[0].dont_ask);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_install_that_cannot_find_a_release_says_so_and_shows_it_in_the_window() {
        let root = scratch("fail");
        let svc = services_in(&root, Arc::new(Nothing));
        let sink = Recorder::default();
        svc.install(&sink, Backend::MsLlamaCpp, BackendOp::Install);
        let done = sink.done();
        assert_eq!(done.len(), 1);
        assert!(!done[0].1 && done[0].2.contains("no release yet"), "{done:?}");
        let (_, backends) = sink.last_backends();
        assert!(backends[1].note.as_deref().is_some_and(|n| n.contains("no release yet")));
        assert!(backends[1].busy.is_none(), "the busy mark is cleared");
        // Nothing installed, so nothing stopped.
        assert_eq!(*sink.stops.lock().unwrap(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_second_operation_on_a_busy_backend_is_refused() {
        let root = scratch("busy");
        let svc = services_in(&root, Arc::new(Nothing));
        let guard = svc.begin(Backend::LlamaCpp, BackendOp::Install).unwrap();
        let sink = Recorder::default();
        svc.install(&sink, Backend::LlamaCpp, BackendOp::Sync);
        assert!(sink.done()[0].2.contains("already being worked on"));
        assert!(svc.reports()[0].busy.is_some());
        drop(guard);
        assert!(svc.reports()[0].busy.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
