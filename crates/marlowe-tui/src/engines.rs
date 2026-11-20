//! The engine window and the release prompt: which llama.cpp backend serves models, what is
//! installed, what is newer upstream, and the buttons that change it.
//!
//! The daemon owns the facts ([`BackendReport`]); this keeps the last of them, the progress of
//! what is running, and the cursor. Every request here is answered by `Backends`,
//! `BackendProgress` and `BackendDone`. The release prompt is raised when nothing else is asking
//! for the user, and a key typed before it could be read goes to the composer instead.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use marlowe_proto::{BackendBusy, BackendOp, BackendReport, BackendUpdate, Request};

use crate::state::{NoteTone, Overlay, State, View};
use crate::text;

/// A printable key this soon after the release prompt appears was meant for the composer.
pub const UPGRADE_GUARD_MS: i64 = 600;

/// What the buttons in the window do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Btn {
    /// Make this backend the one that serves models.
    Select,
    /// First install, or the upgrade to the newest release.
    Install,
    /// Look at the newest build and release.
    Check,
    /// Install the newest build, even within the same release.
    Sync,
    /// Turn the release prompt on or off.
    Prompts,
}

/// One button as the window draws it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Button {
    pub kind: Btn,
    pub label: String,
    pub key: char,
    /// Why it does nothing now, when it does nothing.
    pub off: Option<&'static str>,
}

/// The release prompt, once the daemon has asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeAsk {
    pub backend: String,
    pub title: String,
    pub tag: String,
    pub warning: Option<String>,
    /// 0 yes, 1 no, 2 don't ask again.
    pub sel: usize,
    /// On screen (or waiting for a quiet moment when false).
    pub shown: bool,
    pub guard_until_ms: i64,
}

/// What the client knows about the backends.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EnginesUi {
    pub known: bool,
    pub selected: String,
    pub backends: Vec<BackendReport>,
    /// The latest progress line of a running operation, by backend id.
    pub progress: HashMap<String, BackendBusy>,
    /// The last failure, by backend id; cleared when its next operation starts.
    pub errors: HashMap<String, String>,
    pub upgrade: Option<UpgradeAsk>,
    /// Releases already asked about (or told no) this session: a replay does not ask again.
    pub answered: Vec<(String, String)>,
}

impl EnginesUi {
    pub fn report(&self, id: &str) -> Option<&BackendReport> {
        self.backends.iter().find(|b| b.id == id)
    }

    /// The selected backend's report.
    pub fn current(&self) -> Option<&BackendReport> {
        self.report(&self.selected)
    }

    /// What is running on `r`: the live progress, else what the daemon's report says.
    pub fn busy<'a>(&'a self, r: &'a BackendReport) -> Option<&'a BackendBusy> {
        self.progress.get(&r.id).or(r.busy.as_ref())
    }
}

/// The buttons for one backend, in the order they are drawn.
pub fn buttons(r: &BackendReport, busy: bool) -> Vec<Button> {
    let btn = |kind, label: &str, key, off| Button { kind, label: label.to_string(), key, off };
    let working = busy.then_some("it is busy");
    let mut v = Vec::new();
    if !r.selected {
        let off = working.or((r.installed.is_none()).then_some("install it first"));
        v.push(btn(Btn::Select, "Select", 's', off));
    }
    match (r.state, &r.latest_release) {
        (BackendUpdate::NotInstalled, _) => v.push(btn(Btn::Install, "Install", 'u', working)),
        (BackendUpdate::UpdateAvailable, Some(t)) => v.push(btn(Btn::Install, &format!("Upgrade to {}", text::truncate(&t.tag, 16)), 'u', working)),
        (BackendUpdate::UpdateAvailable, None) => v.push(btn(Btn::Install, "Upgrade", 'u', working)),
        _ => {}
    }
    v.push(btn(Btn::Check, "Check for updates", 'c', working));
    let none_newer = r.builds_newer == Some(0);
    v.push(btn(Btn::Sync, "Sync to upstream", 'b', working.or(none_newer.then_some("no newer build upstream"))));
    v.push(btn(Btn::Prompts, if r.dont_ask { "Release prompts: off" } else { "Release prompts: on" }, 'a', None));
    v
}

/// `v1.0.0 + 197 builds` style label, date and device for the installed line.
pub fn installed_line(r: &BackendReport) -> Option<String> {
    let i = r.installed.as_ref()?;
    let mut parts = vec![if i.label.is_empty() { i.tag.clone() } else { i.label.clone() }];
    if let Some(b) = i.build.as_ref().filter(|b| **b != i.tag && !i.label.contains(b.as_str())) {
        parts.push(format!("build {b}"));
    }
    if let Some(d) = i.date.as_deref().filter(|d| d.len() >= 10) {
        parts.push(d[..10].to_string());
    }
    parts.push(if i.device == "cpu" { "CPU only".to_string() } else { "GPU".to_string() });
    Some(parts.join(" \u{b7} "))
}

/// `3h ago`, `2 days ago`: `then` and `now` in unix seconds.
pub fn ago(then: u64, now: u64) -> String {
    let d = now.saturating_sub(then);
    match d {
        0..=89 => "just now".into(),
        90..=5399 => format!("{} min ago", (d + 30) / 60),
        5400..=129_599 => format!("{}h ago", (d + 1800) / 3600),
        _ => format!("{} days ago", (d + 43_200) / 86_400),
    }
}

impl State {
    /// `engine: llama.cpp` for the title bar, once the daemon has said, and how to colour it.
    pub fn engine_title(&self) -> Option<(String, NoteTone, bool)> {
        let e = &self.engines;
        if !e.known {
            return None;
        }
        let r = e.current()?;
        let busy = e.busy(r).is_some();
        let mut label = format!("engine: {}", r.id);
        let tone = match (r.state, busy) {
            (_, true) => {
                label.push_str(" \u{2026}");
                NoteTone::Dim
            }
            (BackendUpdate::NotInstalled, _) => {
                label.push_str(" \u{b7} not installed");
                NoteTone::Amber
            }
            (BackendUpdate::UpdateAvailable, _) => {
                label.push_str(" \u{2191}");
                NoteTone::Dim
            }
            _ => NoteTone::Dim,
        };
        Some((label, tone, busy))
    }

    /// Opens the engine window on the selected backend.
    pub(crate) fn open_engines(&mut self, out: &mut Vec<Request>) {
        let sel = self.engines.backends.iter().position(|b| b.selected).unwrap_or(0);
        self.overlay = Some(Overlay::Engines { sel, button: 0 });
        if self.conn.is_online() {
            out.push(Request::Backends);
        } else if !self.engines.known {
            self.flash("offline \u{2014} the engine window needs the daemon", NoteTone::Amber);
        }
    }

    pub(crate) fn on_backends(&mut self, selected: &str, backends: &[BackendReport]) {
        self.engines.known = true;
        self.engines.selected = selected.to_string();
        self.engines.backends = backends.to_vec();
        // Only what the daemon still reports as running keeps its progress line.
        let running: Vec<String> = backends.iter().filter(|b| b.busy.is_some()).map(|b| b.id.clone()).collect();
        self.engines.progress.retain(|id, _| running.contains(id));
        if let Some(Overlay::Engines { sel, button }) = &mut self.overlay {
            *sel = (*sel).min(backends.len().saturating_sub(1));
            *button = (*button).min(self.engines.backends.get(*sel).map_or(0, |r| buttons(r, false).len().saturating_sub(1)));
        }
        // A prompt not yet shown that the new report makes moot (installed from another client, or
        // declined) is withdrawn.
        if let Some(u) = self.engines.upgrade.as_ref().filter(|u| !u.shown) {
            if self.release_handled(&u.backend, &u.tag) {
                self.engines.upgrade = None;
            }
        }
        self.offer_models_to_known_user();
    }

    pub(crate) fn on_backend_progress(&mut self, backend: &str, op: BackendOp, message: &str, done: u64, total: u64) {
        let b = BackendBusy { op, message: text::clean(message), done, total };
        self.engines.progress.insert(backend.to_string(), b);
    }

    pub(crate) fn on_backend_done(&mut self, backend: &str, op: BackendOp, ok: bool, message: &str, out: &mut Vec<Request>) {
        self.engines.progress.remove(backend);
        let message = text::clean(message);
        if ok {
            self.engines.errors.remove(backend);
        } else {
            self.engines.errors.insert(backend.to_string(), message.clone());
        }
        // A catalog that is open says which models cannot run: read again once the backend changed.
        if op == BackendOp::Select && ok && self.view == View::Library {
            if let crate::library::LibStep::Catalog { provider, .. } = self.library.step {
                out.push(Request::ListCatalog { provider });
            }
        }
        // The window shows installs and checks; one that ends while it is closed is told here.
        let window = matches!(self.overlay, Some(Overlay::Engines { .. }));
        if op == BackendOp::Select || !ok || !window {
            self.flash(message, if ok { NoteTone::Dim } else { NoteTone::Amber });
        }
    }

    pub(crate) fn on_backend_upgrade(&mut self, backend: &str, title: &str, tag: &str, warning: Option<&String>) {
        let seen = |(b, t): &(String, String)| b == backend && t == tag;
        if self.engines.answered.iter().any(seen) || self.engines.upgrade.as_ref().is_some_and(|u| u.backend == backend && u.tag == tag) {
            return;
        }
        // The event is replayed to a relaunched TUI, which has answered nothing: the daemon's own
        // report says whether the release is still the one to offer.
        if self.release_handled(backend, tag) {
            return;
        }
        self.engines.upgrade = Some(UpgradeAsk {
            backend: backend.to_string(),
            title: title.to_string(),
            tag: tag.to_string(),
            warning: warning.map(|w| text::clean(w)),
            sel: 0,
            shown: false,
            guard_until_ms: 0,
        });
    }

    /// Whether the latest report says `tag` of `backend` is not an upgrade to offer: installed
    /// already, declined for good, or no longer newer than what is installed. Unknown is not handled.
    fn release_handled(&self, backend: &str, tag: &str) -> bool {
        let Some(r) = self.engines.backends.iter().find(|r| r.id == backend) else { return false };
        r.dont_ask || r.state != BackendUpdate::UpdateAvailable || r.installed.as_ref().is_some_and(|i| i.tag == tag || i.release.as_deref() == Some(tag))
    }

    /// Raises a waiting release prompt once nothing else is asking and the composer is empty.
    pub(crate) fn engines_tick(&mut self) {
        let quiet = self.overlay.is_none()
            && self.approvals.is_empty()
            && self.farewell.is_none()
            && !self.booting()
            && !self.onboard.in_progress()
            && self.composer.is_blank()
            && self.conn.is_online();
        let now = self.now_ms;
        if let (true, Some(u)) = (quiet, self.engines.upgrade.as_mut().filter(|u| !u.shown)) {
            u.shown = true;
            u.sel = 0;
            u.guard_until_ms = now + UPGRADE_GUARD_MS;
            self.overlay = Some(Overlay::EngineUpgrade);
        }
    }

    /// A printable key typed before the prompt could be read: the prompt waits, the key goes on.
    pub(crate) fn upgrade_typeahead(&mut self, k: &KeyEvent) -> bool {
        let Some(u) = self.engines.upgrade.as_mut().filter(|u| u.shown) else { return false };
        let printable = matches!(k.code, KeyCode::Char(_)) && !k.modifiers.contains(KeyModifiers::CONTROL);
        if printable && self.now_ms < u.guard_until_ms {
            u.shown = false;
            self.overlay = None;
            return true;
        }
        false
    }

    pub(crate) fn upgrade_key(&mut self, k: KeyEvent, out: &mut Vec<Request>) {
        let Some(u) = self.engines.upgrade.as_mut() else {
            self.overlay = None;
            return;
        };
        match k.code {
            KeyCode::Left | KeyCode::Up | KeyCode::BackTab => u.sel = u.sel.saturating_sub(1),
            KeyCode::Right | KeyCode::Down | KeyCode::Tab => u.sel = (u.sel + 1).min(2),
            KeyCode::Char('y' | 'Y' | '1') => self.answer_upgrade(0, out),
            KeyCode::Char('n' | 'N' | '2') | KeyCode::Esc => self.answer_upgrade(1, out),
            KeyCode::Char('d' | 'D' | '3') => self.answer_upgrade(2, out),
            KeyCode::Enter => {
                let sel = u.sel;
                self.answer_upgrade(sel, out);
            }
            _ => {}
        }
    }

    /// Yes upgrades, no leaves it for the window, and don't-ask turns the prompt off for good.
    pub(crate) fn answer_upgrade(&mut self, choice: usize, out: &mut Vec<Request>) {
        let Some(u) = self.engines.upgrade.take() else { return };
        self.overlay = None;
        self.engines.answered.push((u.backend.clone(), u.tag.clone()));
        match choice {
            0 => {
                self.start(&u.backend, Btn::Install, out);
                self.flash(format!("upgrading {} to {} \u{b7} progress is in the engine window", u.title, u.tag), NoteTone::Dim);
            }
            2 => {
                out.push(Request::BackendDontAsk { backend: u.backend.clone(), dont_ask: true });
                self.flash(format!("no more release prompts for {} \u{b7} the engine window turns them back on", u.title), NoteTone::Dim);
            }
            _ => self.flash(format!("{} {} is waiting in the engine window", u.title, u.tag), NoteTone::Dim),
        }
    }

    /// Sends what `kind` asks of `backend`, and shows it as running at once.
    pub(crate) fn start(&mut self, backend: &str, kind: Btn, out: &mut Vec<Request>) {
        let backend = backend.to_string();
        let (request, op, said) = match kind {
            Btn::Select => (Request::BackendSelect { backend: backend.clone() }, None, "switching \u{b7} saving slot caches, then the engine restarts on the new backend"),
            Btn::Install => (Request::BackendInstall { backend: backend.clone() }, Some(BackendOp::Install), "starting"),
            Btn::Check => (Request::BackendCheck { backend: backend.clone() }, Some(BackendOp::Check), "asking GitHub"),
            Btn::Sync => (Request::BackendSync { backend: backend.clone() }, Some(BackendOp::Sync), "starting"),
            Btn::Prompts => {
                let off = self.engines.report(&backend).is_some_and(|r| r.dont_ask);
                out.push(Request::BackendDontAsk { backend, dont_ask: !off });
                return;
            }
        };
        if kind == Btn::Select {
            self.flash(format!("{said}: {backend}"), NoteTone::Dim);
        }
        self.engines.errors.remove(&backend);
        if let Some(op) = op {
            self.engines.progress.insert(backend, BackendBusy { op, message: said.to_string(), done: 0, total: 0 });
        }
        out.push(request);
    }

    pub(crate) fn engines_key(&mut self, k: KeyEvent, out: &mut Vec<Request>) {
        let Some(Overlay::Engines { sel, button }) = self.overlay.clone() else { return };
        let n = self.engines.backends.len();
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if k.code == KeyCode::Esc || (ctrl && matches!(k.code, KeyCode::Char('k' | 'e'))) || k.code == KeyCode::Char('q') {
            self.overlay = None;
            return;
        }
        let set = |s: &mut State, sel: usize, button: usize| s.overlay = Some(Overlay::Engines { sel, button });
        let list = |s: &State, sel: usize| s.engines.backends.get(sel).map(|r| buttons(r, s.engines.busy(r).is_some())).unwrap_or_default();
        match k.code {
            KeyCode::Up | KeyCode::BackTab => set(self, sel.saturating_sub(1), 0),
            KeyCode::Down | KeyCode::Tab => set(self, (sel + 1).min(n.saturating_sub(1)), 0),
            KeyCode::Left => set(self, sel, button.saturating_sub(1)),
            KeyCode::Right => set(self, sel, (button + 1).min(list(self, sel).len().saturating_sub(1))),
            KeyCode::Enter => {
                if let Some(b) = list(self, sel).get(button).cloned() {
                    self.press(sel, &b, out);
                }
            }
            KeyCode::Char(c) if !ctrl => {
                if let Some((i, b)) = list(self, sel).into_iter().enumerate().find(|(_, b)| b.key == c.to_ascii_lowercase()) {
                    set(self, sel, i);
                    self.press(sel, &b, out);
                }
            }
            _ => {}
        }
    }

    /// A button pressed (key or click): does it, or says why it cannot.
    pub(crate) fn press(&mut self, backend: usize, b: &Button, out: &mut Vec<Request>) {
        let Some(id) = self.engines.backends.get(backend).map(|r| r.id.clone()) else { return };
        if let Some(why) = b.off {
            self.flash(format!("{}: {why}", b.label), NoteTone::Amber);
            return;
        }
        self.start(&id, b.kind, out);
    }

    /// A click in the window: a backend's name moves the cursor, a button presses it.
    pub(crate) fn engines_click(&mut self, backend: usize, button: Option<usize>, out: &mut Vec<Request>) {
        let Some(r) = self.engines.backends.get(backend) else { return };
        let list = buttons(r, self.engines.busy(r).is_some());
        match button {
            Some(i) => {
                self.overlay = Some(Overlay::Engines { sel: backend, button: i });
                if let Some(b) = list.get(i).cloned() {
                    self.press(backend, &b, out);
                }
            }
            None => self.overlay = Some(Overlay::Engines { sel: backend, button: 0 }),
        }
    }

    // ------------------------------------------------------------- first run

    /// The engine to offer when none is installed: its id, title and about how much it downloads.
    pub(crate) fn engine_offer(&self) -> Option<(String, String, u64)> {
        let r = self.engines.current()?;
        (r.state == BackendUpdate::NotInstalled && !r.dont_ask && self.engines.busy(r).is_none()).then(|| (r.id.clone(), r.title.clone(), r.approx_bytes))
    }
}
