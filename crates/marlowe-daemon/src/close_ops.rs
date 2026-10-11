//! Closing the window (track N, Matthew's decision of 2026-10-10): closing the terminal means
//! everything stops, the way users expect. The daemon lives only until its shutdown completes:
//!
//! 1. **pause** all work where it is: in-flight model requests are cancelled at the current step
//!    and every completed step's checkpoint is kept;
//! 2. **unload** every model Marlowe loaded first (Ollama `keep_alive: 0`, the voice sidecar
//!    stopped), so the GPU is free within seconds;
//! 3. **save** everything: sessions, the journal and the memory snapshot, and every crew run and
//!    workflow marked interrupted with where it stopped;
//! 4. **exit**.
//!
//! What starts it: the TUI's close handler sends `WindowClosed`; and as the fallback that needs
//! nothing from a dying process, the last screen client (`tui`) disconnecting without a `Detach`
//! counts as a close once a short grace has passed (a client that reconnects within it cancels
//! the stop). A clean `Detach { keep_running: false }` stops everything too.
//!
//! Running on in the background is an explicit opt-in: the quit dialog's `keep running` choice
//! (`Detach { keep_running: true }`) or the `keep running` setting, off by default. While it is
//! on and no window is attached there is a tray icon and a toast with `[Stop now]`, so a
//! background Marlowe is never invisible.
//!
//! The next launch asks: `Event::Interrupted` lists the runs the close paused ("2 crew runs, 1
//! workflow"), and `ResumeInterrupted { resume: true }` resumes them from their checkpoints.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use marlowe_crew::{Run, RunId, INTERRUPTED};
use marlowe_os::toast::{Toast, ToastAction, Toaster};
use marlowe_os::tray::{Tray, WindowsTray, STOP};
use marlowe_proto::{Event, InterruptedRun, Request};

use crate::daemon::Core;
use crate::lock;

/// The client name of the terminal window: the one whose closing means "stop".
pub(crate) const SCREEN_CLIENT: &str = "tui";
const KEEP_KEY: &str = "keep_running";

/// Frees the models Marlowe loaded; returns what it unloaded.
pub type Unloader = Arc<dyn Fn() -> Vec<String> + Send + Sync>;

pub(crate) struct Close {
    /// The persisted `keep running` setting.
    setting: AtomicBool,
    /// This run's quit-dialog choice.
    choice: AtomicBool,
    closing: AtomicBool,
    /// A window close (not a plain `marlowe shutdown`): runs are left interrupted, not running.
    window_close: AtomicBool,
    screens: Mutex<HashSet<u64>>,
    clean: Mutex<HashSet<u64>>,
    seen_screen: AtomicBool,
    attach_gen: AtomicU64,
    tray: Mutex<Arc<dyn Tray>>,
    unloader: Mutex<Option<Unloader>>,
    /// What each phase did, in order (`cancel`, `unload: model`, `save`, `exit`).
    log: Mutex<Vec<String>>,
    reason: Mutex<String>,
    loaded: AtomicBool,
    /// The background indicator (tray icon and toast) is up.
    shown: AtomicBool,
}

impl Default for Close {
    fn default() -> Self {
        Self {
            setting: AtomicBool::new(false),
            choice: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            window_close: AtomicBool::new(false),
            screens: Mutex::new(HashSet::new()),
            clean: Mutex::new(HashSet::new()),
            seen_screen: AtomicBool::new(false),
            attach_gen: AtomicU64::new(0),
            tray: Mutex::new(Arc::new(WindowsTray::new())),
            unloader: Mutex::new(None),
            log: Mutex::new(Vec::new()),
            reason: Mutex::new(String::new()),
            loaded: AtomicBool::new(false),
            shown: AtomicBool::new(false),
        }
    }
}

fn close_state(core: &Core) -> Arc<Close> {
    core.ext.get_or_init(Close::default)
}

pub(crate) fn close_state_of(core: &Core) -> Arc<Close> {
    close_state(core)
}

/// What tests (and the binary) can replace or read. The daemon needs none of it to run.
#[derive(Clone)]
pub struct CloseHooks(pub(crate) Arc<Close>);

impl CloseHooks {
    pub fn set_tray(&self, t: Arc<dyn Tray>) {
        *lock(&self.0.tray) = t;
    }

    /// Replaces what unloads the models (default: every library model the local engine has loaded).
    pub fn set_unloader(&self, u: Unloader) {
        *lock(&self.0.unloader) = Some(u);
    }

    /// The phases the last close went through, in order.
    pub fn log(&self) -> Vec<String> {
        lock(&self.0.log).clone()
    }

    pub fn closing(&self) -> bool {
        self.0.closing.load(Ordering::SeqCst)
    }

    pub fn keep_running(&self) -> bool {
        background(&self.0)
    }
}

fn background(c: &Close) -> bool {
    c.setting.load(Ordering::SeqCst) || c.choice.load(Ordering::SeqCst)
}

fn note(c: &Close, line: impl Into<String>) {
    lock(&c.log).push(line.into());
}

pub(crate) fn dispatch(core: &Arc<Core>, client: u64, req: Request) {
    let c = close_state(core);
    // Only a window can say its window closed or that it is leaving; a script or a headless run
    // that sends it must not stop everything.
    if matches!(req, Request::WindowClosed | Request::Detach { .. }) && !lock(&c.screens).contains(&client) {
        core.bus.reply(client, Event::Error { session: None, message: "only a terminal window can close or detach".into() });
        return;
    }
    match req {
        Request::WindowClosed => {
            lock(&c.clean).insert(client);
            window_gone(core, "the window was closed");
        }
        Request::Detach { keep_running } => {
            lock(&c.clean).insert(client);
            if keep_running {
                // The background indicator comes up when this client's connection ends.
                c.choice.store(true, Ordering::SeqCst);
                note(&c, "detach: keep running");
            } else {
                window_gone(core, "the window was closed");
            }
        }
        Request::ResumeInterrupted { resume } => resume_interrupted(core, client, resume),
        Request::KeepRunning { on } => {
            if let Some(on) = on {
                if c.setting.swap(on, Ordering::SeqCst) != on {
                    let _ = core.store.set_kv(KEEP_KEY, if on { "1" } else { "0" });
                    core.bus.publish(Event::KeepRunning { on });
                    return;
                }
            }
            core.bus.reply(client, Event::KeepRunning { on: c.setting.load(Ordering::SeqCst) });
        }
        other => crate::client::not_yet(core, client, &other, "N"),
    }
}

/// A client finished its handshake. A screen client is counted, a pending stop is cancelled, and
/// a screen is offered the work the last close interrupted.
pub(crate) fn client_attached(core: &Arc<Core>, client: u64, name: &str) {
    if name != SCREEN_CLIENT {
        return;
    }
    let c = close_state(core);
    lock(&c.screens).insert(client);
    c.seen_screen.store(true, Ordering::SeqCst);
    c.attach_gen.fetch_add(1, Ordering::SeqCst);
    // A window is back: the background indicator goes, and a quit-dialog choice was for that quit.
    c.choice.store(false, Ordering::SeqCst);
    c.shown.store(false, Ordering::SeqCst);
    lock(&c.tray).hide();
    if let Some((runs, summary)) = interrupted(core) {
        core.bus.reply(client, Event::Interrupted { runs, summary });
    }
}

/// A client's connection ended.
pub(crate) fn client_gone(core: &Arc<Core>, client: u64, name: &str) {
    if name != SCREEN_CLIENT {
        return;
    }
    let c = close_state(core);
    let was_screen = lock(&c.screens).remove(&client);
    let clean = lock(&c.clean).remove(&client);
    if !was_screen || core.is_shutting_down() || !lock(&c.screens).is_empty() {
        return;
    }
    if clean {
        // It said goodbye: a close was begun already, or the user chose to keep running.
        if background(&c) {
            enter_background(core);
        }
        return;
    }
    // The last window went without saying goodbye: after a grace, a close.
    let gen = c.attach_gen.load(Ordering::SeqCst);
    let weak = Arc::downgrade(core);
    let grace = core.cfg.close_grace;
    let spawned = std::thread::Builder::new().name("marlowe-close-grace".into()).spawn(move || {
        std::thread::sleep(grace);
        let Some(core) = weak.upgrade() else { return };
        let c = close_state(&core);
        if c.attach_gen.load(Ordering::SeqCst) == gen && lock(&c.screens).is_empty() && !core.is_shutting_down() {
            window_gone(&core, "the last window disconnected");
        }
    });
    if spawned.is_err() {
        window_gone(core, "the last window disconnected");
    }
}

/// The window is gone: stop everything, or run on in the background if the user chose that.
pub(crate) fn window_gone(core: &Arc<Core>, reason: &str) {
    let c = close_state(core);
    if !c.seen_screen.load(Ordering::SeqCst) || background(&c) {
        if background(&c) {
            enter_background(core);
        }
        return;
    }
    begin_close(core, reason);
}

/// Stops everything now: marks the close and asks for the shutdown, which [`before_stop`] and
/// the daemon's `wait` carry out in order. Safe to call twice.
pub(crate) fn begin_close(core: &Arc<Core>, reason: &str) {
    let c = close_state(core);
    if c.closing.swap(true, Ordering::SeqCst) {
        return;
    }
    c.window_close.store(true, Ordering::SeqCst);
    *lock(&c.reason) = reason.to_string();
    note(&c, format!("close: {reason}"));
    core.bus.publish(Event::Closing { reason: reason.to_string() });
    lock(&c.tray).hide();
    core.request_shutdown();
}

/// No window, and the user chose to keep running: the tray icon and a toast with `[Stop now]`.
fn enter_background(core: &Arc<Core>) {
    let c = close_state(core);
    if !lock(&c.screens).is_empty() || c.closing.load(Ordering::SeqCst) {
        return;
    }
    if c.shown.swap(true, Ordering::SeqCst) {
        return;
    }
    note(&c, "background");
    let tray = lock(&c.tray).clone();
    let weak = Arc::downgrade(core);
    tray.on_action(Arc::new({
        let weak = weak.clone();
        move |id: &str| {
            if id == STOP {
                if let Some(core) = weak.upgrade() {
                    begin_close(&core, "stopped from the tray icon");
                }
            }
        }
    }));
    if let Err(e) = tray.show("Marlowe is running in the background") {
        note(&c, format!("no tray icon: {e}"));
    }
    let toaster: Arc<dyn Toaster> = crate::notify_ops::toaster(core);
    toaster.on_action(Arc::new(move |id: &str| {
        if id == "stop" {
            if let Some(core) = weak.upgrade() {
                begin_close(&core, "stopped from the notification");
            }
        }
    }));
    let toast = Toast {
        title: "Marlowe is still running".into(),
        body: "Its window is closed; work continues in the background.".into(),
        urgent: true,
        actions: vec![ToastAction { label: "Stop now".into(), id: "stop".into() }],
        tag: Some("background".into()),
    };
    if let Err(e) = toaster.show(&toast) {
        note(&c, format!("no toast: {e}"));
    }
}

/// Phase 1 of the daemon's shutdown: sessions stop, crew runs are told to stop (in-flight model
/// calls end now). Returns the crew runs that were in flight, for [`settle`].
pub(crate) fn pause(core: &Arc<Core>) -> Vec<RunId> {
    let c = close_state(core);
    note(&c, "cancel");
    // A plain shutdown stops the runs the same way; they stay `running` and resume at the next
    // start, as they always have. Only a window close leaves them interrupted.
    core.crew.get().map(|l| l.supervisor.cancel_for_close()).unwrap_or_default()
}

/// Phase 2: every model Marlowe loaded is unloaded and the voice sidecar stopped, before anything
/// is saved, so the GPU is free as early as possible.
pub(crate) fn unload(core: &Arc<Core>) {
    let c = close_state(core);
    let unloader = lock(&c.unloader).clone();
    let unloaded = match unloader {
        Some(u) => u(),
        None => {
            let (done, failed) = core.brains.unload_local_models();
            for f in failed {
                note(&c, format!("unload failed: {f}"));
            }
            done
        }
    };
    for m in &unloaded {
        note(&c, format!("unload: {m}"));
    }
    if let Some(v) = &core.voice {
        v.shutdown();
        note(&c, "unload: voice");
    }
    if unloaded.is_empty() {
        note(&c, "unload: none");
    }
}

/// Phase 3a: the crew. A window close leaves runs interrupted (paused, with where they stopped);
/// a plain shutdown leaves them running, so the next start resumes them as it always has.
pub(crate) fn settle(core: &Arc<Core>, in_flight: &[RunId]) {
    let c = close_state(core);
    note(&c, "save");
    if c.window_close.load(Ordering::SeqCst) {
        if let Some(link) = core.crew.get() {
            let runs: Vec<Run> = link.supervisor.settle_interrupted(in_flight);
            for r in &runs {
                note(&c, format!("interrupted: {} ({})", r.id.short(), r.note.as_deref().unwrap_or("")));
            }
        }
    } else {
        crate::crew::shutdown(core);
    }
}

/// The last phase, once everything is saved.
pub(crate) fn finished(core: &Arc<Core>) {
    note(&close_state(core), "exit");
}

/// Work a window close paused and the user has not answered for.
fn interrupted(core: &Core) -> Option<(Vec<InterruptedRun>, String)> {
    let link = core.crew.get()?;
    let runs: Vec<InterruptedRun> = link
        .supervisor
        .interrupted()
        .into_iter()
        .map(|r| InterruptedRun {
            run: r.id.clone(),
            title: r.title.clone(),
            workflow: r.workflow.is_some(),
            stopped_at: r.note.as_deref().and_then(|n| n.strip_prefix(INTERRUPTED)).map(|n| n.trim_start_matches(':').trim().to_string()).unwrap_or_default(),
            at_ms: r.updated_ms,
        })
        .collect();
    if runs.is_empty() {
        return None;
    }
    let summary = InterruptedRun::tally(&runs);
    Some((runs, summary))
}

/// The user's answer to "We were still working when you closed Marlowe last time".
fn resume_interrupted(core: &Arc<Core>, client: u64, resume: bool) {
    let Some((runs, _)) = interrupted(core) else { return };
    let Some(link) = core.crew.get() else { return };
    for r in runs {
        let outcome = if resume { link.supervisor.resume(&r.run) } else { link.supervisor.keep_paused(&r.run) };
        if let Err(e) = outcome {
            core.bus.reply(client, Event::Error { session: None, message: format!("run {}: {e}", r.run.short()) });
        }
    }
}

/// The prompt's sentence, as the TUI words it.
pub fn resume_prompt(summary: &str) -> String {
    format!("We were still working when you closed Marlowe last time ({summary}). Resume?")
}

/// Loads the `keep running` setting. Must not block.
pub(crate) fn on_boot(core: &Arc<Core>) {
    let c = close_state(core);
    if c.loaded.swap(true, Ordering::SeqCst) {
        return;
    }
    if core.store.kv(KEEP_KEY).ok().flatten().is_some_and(|v| v == "1") {
        c.setting.store(true, Ordering::SeqCst);
    }
}
