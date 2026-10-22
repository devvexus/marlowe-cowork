//! Notifications and sent files (track N): NotifySettings, OpenFile; the `notify`, `send_file` and
//! `notices` tools' daemon side (toast and spoken line, the outbox, `Notified` and `FileSent`).
//!
//! `notify` (BG7): a Windows toast when the toast switch is on, a spoken line when the voice
//! switch is on, the urgency is normal or more and it is outside quiet hours, and always a notice
//! in the inbox. More than one a minute is held back (recorded, not shown or said) unless urgent.
//! Quiet hours silence the voice, never the toast or the notice. The Notification hook fires for
//! every call (the per-session service does that).
//!
//! `send_file` (BG8): the file is copied to `<home>\outbox\<session>\` (it survives a worktree
//! being cleaned up) and announced as `FileSent`; `OpenFile` opens or reveals a file there and
//! nowhere else.
//!
//! Every outward effect goes through a trait with a fake ([`NotifyHooks`]): tests show no toast,
//! speak nothing, open nothing.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use marlowe_agent::{Conversation, Services};
use marlowe_os::toast::{Toast, Toaster, WindowsToaster};
use marlowe_proto::{Event, NoticeSource, NotifyState, QuietHours, Request, SentFile, SessionId, Severity, Urgency};
use marlowe_sentinel::NoticeDraft;
use serde::{Deserialize, Serialize};

use crate::daemon::Core;
use crate::{lock, now_ms};

/// Notifications closer together than this are held back unless urgent.
pub(crate) const RATE_LIMIT_MS: i64 = 60_000;

/// Opens files for the user's click on a file card.
pub trait Opener: Send + Sync {
    /// Opens `path` in its default app.
    fn open(&self, path: &Path) -> Result<(), String>;
    /// Shows `path` in Explorer.
    fn reveal(&self, path: &Path) -> Result<(), String>;
}

/// Opens through the desktop shell. The viewer outlives Marlowe on purpose, so it starts
/// detached (and scrubbed) through `marlowe_os::child`.
pub struct ShellOpener;

fn launch(mut cmd: std::process::Command) -> Result<(), String> {
    let mut spec = marlowe_os::ChildSpec::new();
    spec.containment = marlowe_os::child::Containment::Detached;
    marlowe_os::child::spawn(&mut cmd, &spec).map(|_| ()).map_err(|e| e.to_string())
}

impl Opener for ShellOpener {
    fn open(&self, path: &Path) -> Result<(), String> {
        #[cfg(windows)]
        let mut cmd = std::process::Command::new("explorer.exe");
        #[cfg(not(windows))]
        let mut cmd = std::process::Command::new("xdg-open");
        cmd.arg(path);
        launch(cmd).map_err(|e| format!("could not open {}: {e}", path.display()))
    }

    fn reveal(&self, path: &Path) -> Result<(), String> {
        #[cfg(windows)]
        let mut cmd = {
            let mut c = std::process::Command::new("explorer.exe");
            c.arg(format!("/select,{}", path.display()));
            c
        };
        #[cfg(not(windows))]
        let mut cmd = {
            let mut c = std::process::Command::new("xdg-open");
            c.arg(path.parent().unwrap_or(path));
            c
        };
        let _ = &mut cmd;
        launch(cmd).map_err(|e| format!("could not show {}: {e}", path.display()))
    }
}

/// Says a line aloud.
pub type Speaker = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Serialize, Deserialize)]
struct Saved {
    toast: bool,
    voice: bool,
}

/// The daemon's notification state, in `Core::ext`.
pub(crate) struct Notify {
    state: Mutex<NotifyState>,
    last_ms: AtomicI64,
    toaster: Mutex<Arc<dyn Toaster>>,
    opener: Mutex<Arc<dyn Opener>>,
    speaker: Mutex<Option<Speaker>>,
    /// Tests: the clock (Unix ms) and the local minute of day, and quiet hours.
    clock: Mutex<Option<(i64, u16)>>,
    quiet: Mutex<Option<QuietHours>>,
    /// Marks that boot loaded the saved switches.
    loaded: AtomicBool,
}

impl Default for Notify {
    fn default() -> Self {
        Self {
            state: Mutex::new(NotifyState::default()),
            last_ms: AtomicI64::new(0),
            toaster: Mutex::new(Arc::new(WindowsToaster::new())),
            opener: Mutex::new(Arc::new(ShellOpener)),
            speaker: Mutex::new(None),
            clock: Mutex::new(None),
            quiet: Mutex::new(None),
            loaded: AtomicBool::new(false),
        }
    }
}

pub(crate) fn notify_state(core: &Core) -> Arc<Notify> {
    core.ext.get_or_init(Notify::default)
}

/// The toaster in use (the real one unless a test or the binary replaced it).
pub(crate) fn toaster(core: &Core) -> Arc<dyn Toaster> {
    lock(&notify_state(core).toaster).clone()
}

/// What tests (and the binary, for a different toaster) can replace. Nothing here is needed to
/// run the daemon.
#[derive(Clone)]
pub struct NotifyHooks(pub(crate) Arc<Notify>);

impl NotifyHooks {
    pub fn set_toaster(&self, t: Arc<dyn Toaster>) {
        *lock(&self.0.toaster) = t;
    }

    pub fn set_opener(&self, o: Arc<dyn Opener>) {
        *lock(&self.0.opener) = o;
    }

    /// Replaces the spoken-line output (default: the daemon's voice, when there is one).
    pub fn set_speaker(&self, s: Speaker) {
        *lock(&self.0.speaker) = Some(s);
    }

    /// Fixes the clock: Unix ms and the local minute of the day.
    pub fn set_clock(&self, now_ms: i64, local_minute: u16) {
        *lock(&self.0.clock) = Some((now_ms, local_minute));
    }

    pub fn set_quiet_hours(&self, q: QuietHours) {
        *lock(&self.0.quiet) = Some(q);
    }

    pub fn state(&self) -> NotifyState {
        *lock(&self.0.state)
    }
}

fn now(core: &Core) -> (i64, u16) {
    if let Some(c) = *lock(&notify_state(core).clock) {
        return c;
    }
    use chrono::Timelike;
    let t = chrono::Local::now();
    (now_ms(), (t.hour() * 60 + t.minute()) as u16)
}

fn quiet(core: &Core) -> QuietHours {
    if let Some(q) = *lock(&notify_state(core).quiet) {
        return q;
    }
    core.sentinel.as_ref().map(|s| s.quiet_hours()).unwrap_or_default()
}

fn saved_path(core: &Core) -> PathBuf {
    core.home.join("notify.json")
}

fn save_switches(core: &Core, s: NotifyState) {
    let text = serde_json::to_string(&Saved { toast: s.toast, voice: s.voice }).unwrap_or_default();
    if let Err(e) = std::fs::write(saved_path(core), text) {
        eprintln!("marlowe: notification settings not saved: {e}");
    }
}

pub(crate) fn dispatch(core: &Arc<Core>, client: u64, req: Request) {
    match req {
        Request::NotifySettings { toast, voice } => {
            let n = notify_state(core);
            let (now, changed) = {
                let mut st = lock(&n.state);
                let before = *st;
                if let Some(t) = toast {
                    st.toast = t;
                }
                if let Some(v) = voice {
                    st.voice = v;
                }
                (*st, *st != before)
            };
            if changed {
                save_switches(core, now);
                core.bus.publish(Event::NotifyState(now));
            } else {
                core.bus.reply(client, Event::NotifyState(now));
            }
        }
        Request::OpenFile { path, reveal } => {
            if let Err(e) = open_file(core, Path::new(&path), reveal) {
                core.bus.reply(client, Event::Error { session: None, message: e });
            }
        }
        other => crate::client::not_yet(core, client, &other, "N"),
    }
}

/// Opens or reveals a file, if it is in the outbox.
fn open_file(core: &Core, path: &Path, reveal: bool) -> Result<(), String> {
    let outbox = core.home.join("outbox");
    let real = std::fs::canonicalize(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let root = std::fs::canonicalize(&outbox).map_err(|_| "nothing has been sent to the outbox yet".to_string())?;
    if !real.starts_with(&root) {
        return Err(format!("{} is not in the outbox; only files Marlowe sent can be opened from a card", path.display()));
    }
    // Opening does whatever Windows does with the type, which for many is to run it. Only plain
    // documents and images open from a card; everything else is shown in Explorer, where the user
    // decides.
    if !reveal && !opens_safely(&real) {
        return Err(format!(
            "{} is not a plain document or image, so Marlowe will not open it from a card; reveal it in Explorer and open it yourself if you mean to",
            real.display()
        ));
    }
    let opener = lock(&notify_state(core).opener).clone();
    if reveal {
        opener.reveal(&real)
    } else {
        opener.open(&real)
    }
}

/// The types a card may open: text, data, images and documents a viewer shows without running
/// anything of the file's. A list of what is safe, not of what is known to be dangerous: a type
/// not named here, and a file with no extension, is reveal-only.
const OPENABLE_EXTENSIONS: &[&str] = &[
    "txt", "md", "markdown", "rst", "log", "csv", "tsv", "json", "jsonl", "yaml", "yml", "toml", "tex", "bib", "png", "jpg", "jpeg",
    "gif", "webp", "bmp", "svg", "pdf", "html", "htm",
];

fn opens_safely(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| OPENABLE_EXTENSIONS.iter().any(|p| p.eq_ignore_ascii_case(e)))
}

/// `notify` for `session`: what was done, in a line for the model.
pub(crate) fn notify(core: &Arc<Core>, session: &SessionId, title: &str, body: &str, urgency: Urgency) -> String {
    let n = notify_state(core);
    let (at, minute) = now(core);
    let held = urgency != Urgency::Urgent && {
        let last = n.last_ms.load(Ordering::SeqCst);
        last > 0 && at - last < RATE_LIMIT_MS
    };
    if !held {
        n.last_ms.store(at, Ordering::SeqCst);
    }
    let mut done: Vec<String> = Vec::new();
    // The notice is always recorded, held back or not.
    if let Some(s) = &core.sentinel {
        let severity = match urgency {
            Urgency::Low => Severity::Info,
            Urgency::Normal => Severity::Attention,
            Urgency::Urgent => Severity::Alarm,
        };
        let draft = NoticeDraft::new(NoticeSource::Notify, severity, "notify", title, body, format!("{session}:{title}"));
        if let Ok(Some(notice)) = s.raise(draft) {
            core.bus.publish(Event::Notice(notice));
            done.push("recorded as a notice".into());
        }
    }
    if held {
        let said = "Held back: one notification a minute (urgent ones are not limited)";
        return if done.is_empty() { format!("{said}.") } else { format!("{said}; {}.", done.join(", ")) };
    }
    let switches = *lock(&n.state);
    if switches.toast {
        let toast = Toast { title: title.to_string(), body: body.to_string(), urgent: urgency == Urgency::Urgent, actions: Vec::new(), tag: Some("notify".into()) };
        let toaster = lock(&n.toaster).clone();
        match toaster.show(&toast) {
            Ok(()) => done.insert(0, "toast shown".into()),
            Err(e) => done.insert(0, format!("no toast ({e})")),
        }
    }
    if switches.voice && urgency != Urgency::Low {
        if quiet(core).contains(minute) {
            done.push("not spoken (quiet hours)".into());
        } else {
            let line = if body.trim().is_empty() { format!("{title}.") } else { format!("{title}. {body}") };
            let line = crate::brief::spoken(&line);
            let speaker = lock(&n.speaker).clone();
            match speaker {
                Some(say) => {
                    say(&line);
                    done.push("spoken".into());
                }
                None if core.voice.is_some() => {
                    crate::brief_ops::speak_background(core, &line);
                    done.push("spoken".into());
                }
                None => {}
            }
        }
    }
    core.bus.publish(Event::Notified { session: Some(session.clone()), title: title.to_string(), body: body.to_string(), urgency });
    if done.is_empty() {
        "Notified (nothing is switched on to show it; the notice stands).".to_string()
    } else {
        format!("Notified: {}.", done.join(", "))
    }
}

/// Background work's outcome for `session` that nothing else will say now: a line in the
/// conversation (`Notified`) and a toast when the toast switch is on. Not rate limited (one per
/// finished run), not spoken (a relay's own turn speaks).
pub(crate) fn announce(core: &Arc<Core>, session: &SessionId, title: &str, body: &str) {
    let n = notify_state(core);
    if lock(&n.state).toast {
        let toast = Toast { title: title.to_string(), body: body.to_string(), urgent: false, actions: Vec::new(), tag: Some("crew".into()) };
        let toaster = lock(&n.toaster).clone();
        if let Err(e) = toaster.show(&toast) {
            eprintln!("marlowe: no toast for {title}: {e}");
        }
    }
    core.bus.publish(Event::Notified {
        session: Some(session.clone()),
        title: title.to_string(),
        body: body.to_string(),
        urgency: Urgency::Normal,
    });
}

/// `send_file`: copies `path` to the outbox and announces it.
pub(crate) fn send_file(core: &Arc<Core>, session: &SessionId, path: &Path, note: &str) -> Result<SentFile, String> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).ok_or("that path has no file name")?;
    let dir = core.home.join("outbox").join(sanitize(session.as_str()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create the outbox: {e}"))?;
    let mut dest = dir.join(&name);
    let mut n = 1;
    while dest.exists() {
        let stem = Path::new(&name).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| name.clone());
        let ext = Path::new(&name).extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
        dest = dir.join(format!("{stem}-{n}{ext}"));
        n += 1;
    }
    let bytes = std::fs::copy(path, &dest).map_err(|e| format!("cannot copy {}: {e}", path.display()))?;
    let digest = digest_of(&dest).map_err(|e| format!("cannot read the copy: {e}"))?;
    if digest_of(path).map_or(false, |d| d != digest) {
        let _ = std::fs::remove_file(&dest);
        return Err(format!("{} changed while it was being copied; send it again", path.display()));
    }
    let file = SentFile { name, path: dest.display().to_string(), bytes, digest, note: note.to_string() };
    core.bus.publish(Event::FileSent { session: session.clone(), file: file.clone() });
    Ok(file)
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') { c } else { '_' }).collect()
}

fn digest_of(path: &Path) -> std::io::Result<String> {
    let mut h = blake3::Hasher::new();
    h.update_reader(std::fs::File::open(path)?)?;
    Ok(h.finalize().to_hex().to_string())
}

/// N's services for one session (the notifier, the outbox): the `Background` service, which
/// `task_ops::services` also fills, is built there; this adds nothing a tool reaches directly.
pub(crate) fn services(_core: &Arc<Core>, _convo: &Conversation, services: Services) -> Services {
    services
}

/// Loads the notification switches. Must not block.
pub(crate) fn on_boot(core: &Arc<Core>) {
    let n = notify_state(core);
    if n.loaded.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Some(saved) = std::fs::read_to_string(saved_path(core)).ok().and_then(|t| serde_json::from_str::<Saved>(&t).ok()) {
        *lock(&n.state) = NotifyState { toast: saved.toast, voice: saved.voice };
    }
}
