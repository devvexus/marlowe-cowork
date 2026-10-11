//! The Chatterbox sidecar process (`tools/voice/chatterbox_sidecar.py`): spawned lazily in its own
//! kill-on-close Job Object with a scrubbed environment and no console window, kept alive across
//! utterances, respawned when it has exited (its idle timeout, or a crash). One request at a time:
//! JSON lines in, JSON lines out. A request comes back in parts (one per clause, each a WAV file in
//! the cache dir, read and deleted here), so the first clause plays while the next is generated.
//! stdout is read on its own thread into a channel; stderr is drained on another into the daemon
//! log (line length and count bounded), so neither pipe can fill.
//!
//! Every live sidecar's job is registered (weakly), so [`owns_process`] can tell the GPU gates
//! that Marlowe's own voice is not foreign compute.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError};
use marlowe_os::JobObject;
use serde_json::Value;

use crate::{lock, Part};
use crate::settings::ChatterboxSettings;

/// The sidecar script this binary was built with; written to the cache dir when no override is set.
const SCRIPT: &str = include_str!("../../../tools/voice/chatterbox_sidecar.py");
const SCRIPT_NAME: &str = "chatterbox_sidecar.py";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// A WAV larger than this is refused (about 20 minutes of 24 kHz 16-bit mono).
const MAX_WAV: u64 = 64 * 1024 * 1024;
/// stderr lines longer than this are clipped in the log.
const LOG_LINE: usize = 300;
/// stderr lines logged per sidecar process; the rest are only kept in the tail.
const LOG_LINES: usize = 1000;
/// stderr lines kept for error messages.
const TAIL: usize = 12;
/// How long a sidecar may take to exit after its stdin closes before its job is terminated.
const EXIT_GRACE: Duration = Duration::from_millis(300);
const POLL: Duration = Duration::from_millis(50);

/// Jobs of the live sidecars. Weak: dropping a sidecar still kills its tree.
static OWNED: Mutex<Vec<Weak<JobObject>>> = Mutex::new(Vec::new());

/// Whether `pid` is a process of a live voice sidecar (the interpreter a venv launcher starts
/// included: everything the sidecar starts stays in its job).
pub fn owns_process(pid: u32) -> bool {
    let jobs: Vec<Arc<JobObject>> = lock(&OWNED).iter().filter_map(Weak::upgrade).collect();
    jobs.iter().any(|j| j.contains(pid))
}

fn register(job: &Arc<JobObject>) {
    let mut owned = lock(&OWNED);
    owned.retain(|w| w.strong_count() > 0);
    owned.push(Arc::downgrade(job));
}

/// Variable names that look like credentials, removed from the sidecar's environment on top of
/// the caller's list.
pub(crate) fn is_secret_name(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    n.ends_with("_API_KEY") || n.ends_with("_ACCESS_KEY") || n.ends_with("_TOKEN") || n.contains("SECRET") || n.contains("PASSWORD")
}

/// Whether a load of this many MiB fits in GPU memory now (free less headroom): `Some(false)`
/// keeps a voice model off the GPU; `None` (no reading) does not.
#[derive(Clone)]
pub struct VramCheck(pub Arc<dyn Fn(f64) -> Option<bool> + Send + Sync>);

impl std::fmt::Debug for VramCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VramCheck")
    }
}

/// How to run the sidecar.
#[derive(Debug, Clone)]
pub(crate) struct SidecarConfig {
    pub python: Option<PathBuf>,
    /// `None`: the built-in script, written to `cache_dir`.
    pub script: Option<PathBuf>,
    pub reference: Option<PathBuf>,
    /// Working directory, script copy and WAV hand-over.
    pub cache_dir: PathBuf,
    /// Generation and lifetime flags (`--ex 0.5 …`).
    pub args: Vec<String>,
    /// Environment variables removed from the child.
    pub scrub: Vec<String>,
    /// Environment variables set on the child (tests).
    pub env: Vec<(String, String)>,
    pub load_timeout: Duration,
    /// How long a line waits for a model still loading before another voice says it.
    pub ready_grace: Duration,
    /// Base time per part.
    pub request_timeout: Duration,
    /// Extra time per character of a part.
    pub per_char: Duration,
    /// GPU memory the loaded model holds, MiB, checked against `vram` before a GPU start.
    pub vram_mib: f64,
    /// The GPU memory check; `None` starts unchecked (tests, no GPU reading).
    pub vram: Option<VramCheck>,
}

/// A finite value clamped to `lo..=hi`, else `default`.
fn clamp(v: f32, lo: f32, hi: f32, default: f32) -> f32 {
    if v.is_finite() {
        v.clamp(lo, hi)
    } else {
        default
    }
}

impl SidecarConfig {
    pub fn from_settings(home: &Path, cb: &ChatterboxSettings, scrub: &[&str]) -> SidecarConfig {
        let d = ChatterboxSettings::default();
        let device = match cb.device.trim().to_ascii_lowercase().as_str() {
            dev @ ("cuda" | "cpu") => dev.to_string(),
            _ => "auto".to_string(),
        };
        let args = vec![
            "--device".into(),
            device,
            "--ex".into(),
            clamp(cb.exaggeration, 0.0, 2.0, d.exaggeration).to_string(),
            "--cfg".into(),
            clamp(cb.cfg_weight, 0.0, 3.0, d.cfg_weight).to_string(),
            "--temp".into(),
            clamp(cb.temperature, 0.05, 2.0, d.temperature).to_string(),
            "--pause".into(),
            clamp(cb.pause_scale, 0.0, 5.0, d.pause_scale).to_string(),
            "--idle".into(),
            cb.idle_timeout_s.min(86_400).to_string(),
        ];
        SidecarConfig {
            python: cb.python.clone(),
            script: cb.script.clone(),
            reference: cb.reference.clone(),
            cache_dir: home.join("cache").join("voice"),
            args,
            scrub: scrub.iter().map(|s| s.to_string()).collect(),
            env: Vec::new(),
            load_timeout: Duration::from_secs(cb.load_timeout_s.clamp(5, 900)),
            ready_grace: Duration::from_secs(cb.ready_grace_s.min(900)),
            request_timeout: Duration::from_secs(cb.request_timeout_s.clamp(5, 900)),
            per_char: Duration::from_millis(200),
            vram_mib: cb.vram_mib as f64,
            vram: None,
        }
    }

    /// The `--device` the sidecar is started with.
    fn device(&self) -> &str {
        self.args.iter().position(|a| a == "--device").and_then(|i| self.args.get(i + 1)).map_or("auto", String::as_str)
    }

    /// Refuses a GPU start the check says does not fit. A CPU start is never checked.
    pub(crate) fn vram_gate(&self) -> Result<(), SidecarError> {
        if self.device() == "cpu" {
            return Ok(());
        }
        match self.vram.as_ref().and_then(|c| (c.0)(self.vram_mib)) {
            Some(false) => Err(SidecarError::Start(format!(
                "not enough free GPU memory for the voice model (about {:.1} GiB) beside what is loaded",
                self.vram_mib / 1024.0
            ))),
            _ => Ok(()),
        }
    }

    /// The command line, environment and pipes, without spawning.
    pub(crate) fn command(&self, python: &Path, script: &Path, reference: &Path) -> Command {
        let mut cmd = Command::new(python);
        // -I: no user site, no PYTHON* variables, neither the script's dir nor the cwd on sys.path.
        cmd.arg("-I")
            .arg(script)
            .arg("--cache")
            .arg(&self.cache_dir)
            .arg("--ref")
            .arg(reference)
            .args(&self.args)
            .current_dir(&self.cache_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for var in &self.scrub {
            cmd.env_remove(var);
        }
        for (k, _) in std::env::vars_os() {
            if k.to_str().is_some_and(is_secret_name) {
                cmd.env_remove(&k);
            }
        }
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd
    }
}

/// Why the sidecar did not produce audio.
#[derive(Debug)]
pub(crate) enum SidecarError {
    /// It never became ready (not configured, would not spawn, exited or timed out loading).
    Start(String),
    /// It exited while a request was outstanding.
    Died(String),
    /// A part took longer than its deadline; the sidecar was killed.
    Timeout(String),
    /// The caller gave up (barge-in or shutdown).
    Cancelled,
    /// It answered this request with an error (`OutOfMemoryError: …`).
    Synth(String),
    /// It answered with something unusable.
    Protocol(String),
}

impl SidecarError {
    /// The kind of failure, for reporting one cause once: the variant, and for a refused line
    /// the Python exception type (an OOM message carries different numbers every time).
    pub(crate) fn cause(&self) -> String {
        match self {
            SidecarError::Start(_) => "start".into(),
            SidecarError::Died(_) => "died".into(),
            SidecarError::Timeout(_) => "timeout".into(),
            SidecarError::Cancelled => "cancelled".into(),
            SidecarError::Protocol(_) => "protocol".into(),
            SidecarError::Synth(m) => {
                let ty = m.split(':').next().unwrap_or("").trim();
                let ty = if !ty.is_empty() && ty.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.') { ty } else { "" };
                format!("synth:{ty}")
            }
        }
    }
}

impl std::fmt::Display for SidecarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SidecarError::Start(m) => write!(f, "it did not start: {m}"),
            SidecarError::Died(m) => write!(f, "it exited mid-utterance: {m}"),
            SidecarError::Synth(m) => write!(f, "it could not say that: {m}"),
            SidecarError::Timeout(m) | SidecarError::Protocol(m) => f.write_str(m),
            SidecarError::Cancelled => f.write_str("cancelled"),
        }
    }
}

struct Running {
    job: Arc<JobObject>,
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    /// The sidecar's ready line, once it has loaded.
    ready: Option<Value>,
    spawned: Instant,
    tail: Arc<Mutex<VecDeque<String>>>,
}

impl Running {
    fn exited(&mut self) -> bool {
        !matches!(self.child.try_wait(), Ok(None))
    }

    /// The exit status (when known) and the last stderr lines, for an error message.
    fn epitaph(&mut self) -> String {
        let deadline = Instant::now() + Duration::from_millis(500);
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(s)) => break Some(s),
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                _ => break None,
            }
        };
        // Let the stderr thread catch up with what the process wrote last.
        std::thread::sleep(Duration::from_millis(100));
        let tail: Vec<String> = lock(&self.tail).iter().rev().take(3).rev().cloned().collect();
        let status = status.map(|s| format!("exit {}", s.code().map_or("?".into(), |c| c.to_string())));
        match (status, tail.is_empty()) {
            (Some(s), false) => format!("{s}; {}", tail.join(" | ")),
            (Some(s), true) => s,
            (None, false) => tail.join(" | "),
            (None, true) => "no output".into(),
        }
    }

    /// For a process that has exited without anyone reading its ready line: why it never loaded
    /// (its `{"fatal"}` line, else its exit), or `None` when it had loaded (an idle exit).
    fn load_failure(&mut self) -> Option<String> {
        let deadline = Instant::now() + Duration::from_millis(500);
        while let Ok(line) = self.lines.recv_deadline(deadline) {
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            if let Some(msg) = v.get("fatal") {
                return Some(msg.as_str().map(str::to_string).unwrap_or_else(|| msg.to_string()));
            }
            if v.get("ready").and_then(Value::as_bool) == Some(true) {
                return None;
            }
        }
        Some(format!("it exited while loading ({})", self.epitaph()))
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // EOF first: the sidecar exits cleanly when it can; the job takes the rest.
        drop(self.stdin.take());
        let start = Instant::now();
        while start.elapsed() < EXIT_GRACE && !self.exited() {
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.job.terminate(1);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The request the sidecar is working on.
struct Active {
    id: u64,
    /// The clauses the sidecar split the text into (its first reply).
    plan: Option<Vec<String>>,
    /// Parts received so far.
    received: usize,
    whole: bool,
    /// Characters of the whole text.
    chars: usize,
    budget: Duration,
    deadline: Instant,
}

impl Active {
    /// Time allowed for the next part: the base plus time per character of that part (of the
    /// whole text before the plan is known, or when it comes as one part).
    fn next_budget(&self, cfg: &SidecarConfig) -> Duration {
        let chars = match &self.plan {
            Some(plan) if !self.whole => plan.get(self.received).map_or(0, |c| c.chars().count()),
            _ => self.chars,
        };
        cfg.request_timeout + cfg.per_char.saturating_mul(u32::try_from(chars).unwrap_or(u32::MAX))
    }

    fn rearm(&mut self, cfg: &SidecarConfig) {
        self.budget = self.next_budget(cfg);
        self.deadline = Instant::now() + self.budget;
    }
}

/// A lazily started sidecar.
pub(crate) struct Sidecar {
    cfg: SidecarConfig,
    proc: Option<Running>,
    active: Option<Active>,
    next_id: u64,
}

fn materialize_script(dir: &Path) -> std::io::Result<PathBuf> {
    let path = dir.join(SCRIPT_NAME);
    if std::fs::read_to_string(&path).ok().as_deref() != Some(SCRIPT) {
        let tmp = dir.join(format!("{SCRIPT_NAME}.tmp"));
        std::fs::write(&tmp, SCRIPT)?;
        std::fs::rename(&tmp, &path)?;
    }
    Ok(path)
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// stderr -> the daemon log (bounded) and a short tail for error messages.
fn drain_stderr(stderr: impl Read, tail: Arc<Mutex<VecDeque<String>>>, pid: u32) {
    let mut logged = 0usize;
    for chunk in BufReader::new(stderr).split(b'\n') {
        let Ok(bytes) = chunk else { break };
        let text = String::from_utf8_lossy(&bytes);
        // A progress bar redraws with \r: its last state is the line.
        let line = text.rsplit('\r').find(|s| !s.trim().is_empty()).unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let line = clip(line, LOG_LINE);
        if logged < LOG_LINES {
            eprintln!("marlowe: voice sidecar {pid}: {line}");
        } else if logged == LOG_LINES {
            eprintln!("marlowe: voice sidecar {pid}: (further output is not logged)");
        }
        logged = logged.saturating_add(1);
        let mut t = lock(&tail);
        if t.len() == TAIL {
            t.pop_front();
        }
        t.push_back(line);
    }
}

/// Clauses joined back into text for another voice to say.
fn join_clauses(clauses: &[String]) -> String {
    let mut out = String::new();
    for c in clauses.iter().map(|c| c.trim()).filter(|c| !c.is_empty()) {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(c);
        if !c.ends_with(['.', '!', '?', ',', ';', ':']) {
            out.push('.');
        }
    }
    out
}

impl Sidecar {
    pub fn new(cfg: SidecarConfig) -> Sidecar {
        Sidecar { cfg, proc: None, active: None, next_id: 1 }
    }

    /// How long a line waits for a model that is still loading.
    pub fn grace(&self) -> Duration {
        self.cfg.ready_grace
    }

    /// Spawns the sidecar unless one is running. Returns at once: the model loads in the
    /// background. A sidecar that ended while loading (a prewarm nobody waited on) is reported,
    /// not silently loaded again; one that ended after loading (its idle exit) is replaced.
    pub fn start(&mut self) -> Result<(), SidecarError> {
        if let Some(p) = &mut self.proc {
            if !p.exited() {
                if p.ready.is_none() && p.spawned.elapsed() >= self.cfg.load_timeout {
                    self.reset();
                    return Err(SidecarError::Start(format!("the model did not load within {} s", self.cfg.load_timeout.as_secs())));
                }
                return Ok(());
            }
            let failure = if p.ready.is_none() { p.load_failure() } else { None };
            self.reset();
            if let Some(why) = failure {
                return Err(SidecarError::Start(why));
            }
        }
        self.cfg.vram_gate()?;
        self.proc = Some(self.spawn()?);
        Ok(())
    }

    /// Whether a sidecar is running with its model loaded: it can speak with nothing to load.
    pub fn is_loaded(&mut self) -> bool {
        let running = self.proc.as_mut().is_some_and(|p| !p.exited());
        if !running {
            return false;
        }
        matches!(self.ready(Duration::ZERO, &|| false), Ok(true))
    }

    /// Drops the process (its job kills the tree, freeing its VRAM) and any request.
    pub fn reset(&mut self) {
        self.active = None;
        self.proc = None;
    }

    fn spawn(&self) -> Result<Running, SidecarError> {
        let cfg = &self.cfg;
        let python = cfg.python.as_deref().ok_or_else(|| {
            SidecarError::Start("no Python is configured (voice.json chatterbox.python or MARLOWE_VOICE_PYTHON)".into())
        })?;
        let reference = cfg.reference.as_deref().ok_or_else(|| {
            SidecarError::Start("no voice sample is configured (voice.json chatterbox.reference or MARLOWE_VOICE_REF)".into())
        })?;
        if !reference.is_file() {
            return Err(SidecarError::Start(format!("the voice sample {} does not exist", reference.display())));
        }
        std::fs::create_dir_all(&cfg.cache_dir)
            .map_err(|e| SidecarError::Start(format!("cache dir {}: {e}", cfg.cache_dir.display())))?;
        let script = match &cfg.script {
            Some(s) => s.clone(),
            None => materialize_script(&cfg.cache_dir).map_err(|e| SidecarError::Start(format!("writing the sidecar script: {e}")))?,
        };
        let mut cmd = cfg.command(python, &script, reference);
        let jc = marlowe_os::spawn_in_job_with_flags(&mut cmd, CREATE_NO_WINDOW)
            .map_err(|e| SidecarError::Start(format!("could not run {}: {e}", python.display())))?;
        let pid = jc.id();
        let (mut child, job) = jc.into_parts();
        let job = Arc::new(job);
        // Dropping `job` on any error below kills the process.
        let (stdin, stdout, stderr) = match (child.stdin.take(), child.stdout.take(), child.stderr.take()) {
            (Some(i), Some(o), Some(e)) => (i, o, e),
            _ => return Err(SidecarError::Start("the sidecar's pipes are missing".into())),
        };
        let tail = Arc::new(Mutex::new(VecDeque::with_capacity(TAIL)));
        let t = tail.clone();
        std::thread::Builder::new()
            .name("marlowe-voice-sidecar-err".into())
            .spawn(move || drain_stderr(stderr, t, pid))
            .map_err(|e| SidecarError::Start(format!("stderr reader: {e}")))?;
        let (tx, lines) = crossbeam_channel::bounded::<String>(64);
        std::thread::Builder::new()
            .name("marlowe-voice-sidecar-out".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| SidecarError::Start(format!("stdout reader: {e}")))?;
        register(&job);
        eprintln!("marlowe: voice: started the Chatterbox sidecar (pid {pid})");
        Ok(Running { job, child, stdin: Some(stdin), lines, ready: None, spawned: Instant::now(), tail })
    }

    /// Waits up to `wait` for the running sidecar to load its model. `Ok(false)`: still loading
    /// (it carries on, and the next call finds it further along). Past the load timeout it is
    /// killed.
    pub fn ready(&mut self, wait: Duration, cancelled: &dyn Fn() -> bool) -> Result<bool, SidecarError> {
        let load_timeout = self.cfg.load_timeout;
        let Some(p) = &mut self.proc else { return Err(SidecarError::Start("not running".into())) };
        if p.ready.is_some() {
            return Ok(true);
        }
        let until = Instant::now() + wait;
        let deadline = p.spawned + load_timeout;
        loop {
            match p.lines.recv_timeout(POLL.min(until.saturating_duration_since(Instant::now()))) {
                Ok(line) => {
                    let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                    if let Some(msg) = v.get("fatal") {
                        let msg = msg.as_str().map(str::to_string).unwrap_or_else(|| msg.to_string());
                        self.reset();
                        return Err(SidecarError::Start(msg));
                    }
                    if v.get("ready").and_then(Value::as_bool) == Some(true) {
                        let device = v.get("device").and_then(Value::as_str).unwrap_or("?");
                        eprintln!("marlowe: voice: the Chatterbox sidecar is ready on {device} after {:.1} s", p.spawned.elapsed().as_secs_f32());
                        p.ready = Some(v);
                        return Ok(true);
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    let why = p.epitaph();
                    self.reset();
                    return Err(SidecarError::Start(format!("it exited while loading ({why})")));
                }
                Err(RecvTimeoutError::Timeout) => {
                    if Instant::now() >= deadline {
                        self.reset();
                        return Err(SidecarError::Start(format!("the model did not load within {} s", load_timeout.as_secs())));
                    }
                    if cancelled() {
                        return Err(SidecarError::Cancelled);
                    }
                    if Instant::now() >= until {
                        return Ok(false);
                    }
                }
            }
        }
    }

    /// Sends `text` to the ready sidecar; its parts then come from [`Sidecar::next_part`]
    /// (`whole`: all of it as one part). A request still outstanding is cancelled first.
    pub fn begin(&mut self, text: &str, whole: bool) -> Result<(), SidecarError> {
        self.abandon();
        let id = self.next_id;
        self.next_id += 1;
        let Some(p) = &mut self.proc else { return Err(SidecarError::Died("not running".into())) };
        let line = serde_json::json!({ "id": id, "text": text, "whole": whole }).to_string();
        let sent = match p.stdin.as_mut() {
            Some(w) => writeln!(w, "{line}").and_then(|()| w.flush()),
            None => Err(std::io::Error::other("stdin closed")),
        };
        if let Err(e) = sent {
            let why = p.epitaph();
            self.reset();
            return Err(SidecarError::Died(format!("{e}; {why}")));
        }
        let mut a = Active {
            id,
            plan: None,
            received: 0,
            whole,
            chars: text.chars().count(),
            budget: Duration::ZERO,
            deadline: Instant::now(),
        };
        a.rearm(&self.cfg);
        self.active = Some(a);
        Ok(())
    }

    /// The outstanding request's next part, if it arrives within `wait` (`Ok(None)`: not yet).
    /// A cancel tells the sidecar to skip the rest; a part past its deadline kills the sidecar.
    /// After an error the request stays outstanding for [`Sidecar::take_rest`].
    pub fn next_part(&mut self, wait: Duration, cancelled: &dyn Fn() -> bool) -> Result<Option<Part>, SidecarError> {
        let until = Instant::now() + wait;
        loop {
            let (Some(p), Some(a)) = (self.proc.as_mut(), self.active.as_mut()) else {
                return Err(SidecarError::Died("no request is outstanding".into()));
            };
            match p.lines.recv_timeout(POLL.min(until.saturating_duration_since(Instant::now()))) {
                Ok(line) => {
                    let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                    if v.get("id").and_then(Value::as_u64) != Some(a.id) {
                        // A reply to a request given up earlier: its audio is not wanted.
                        if let Some(path) = v.get("wav").and_then(Value::as_str) {
                            let _ = read_wav(&self.cfg.cache_dir, Path::new(path));
                        }
                        continue;
                    }
                    if let Some(err) = v.get("error") {
                        let err = err.as_str().map(str::to_string).unwrap_or_else(|| err.to_string());
                        return Err(SidecarError::Synth(err));
                    }
                    if let Some(plan) = v.get("plan").and_then(Value::as_array) {
                        a.plan = Some(plan.iter().map(|c| c.as_str().unwrap_or_default().to_string()).collect());
                        a.rearm(&self.cfg);
                        continue;
                    }
                    let Some(path) = v.get("wav").and_then(Value::as_str) else { continue };
                    let last = v.get("last").and_then(Value::as_bool).unwrap_or(true);
                    let wav = read_wav(&self.cfg.cache_dir, Path::new(path))?;
                    a.received += 1;
                    if last {
                        self.active = None;
                    } else {
                        a.rearm(&self.cfg);
                    }
                    return Ok(Some(Part { wav, last }));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    let why = p.epitaph();
                    self.proc = None;
                    return Err(SidecarError::Died(why));
                }
                Err(RecvTimeoutError::Timeout) => {
                    if cancelled() {
                        self.abandon();
                        return Err(SidecarError::Cancelled);
                    }
                    if Instant::now() >= a.deadline {
                        let secs = a.budget.as_secs();
                        self.proc = None;
                        return Err(SidecarError::Timeout(format!("it did not answer within {secs} s")));
                    }
                    if Instant::now() >= until {
                        return Ok(None);
                    }
                }
            }
        }
    }

    /// Ends the outstanding request and returns what it had not yet delivered, when the
    /// sidecar had said how it split the text (`None`: nothing was delivered).
    pub fn take_rest(&mut self) -> Option<String> {
        let a = self.active.take()?;
        let plan = a.plan?;
        Some(join_clauses(plan.get(a.received..).unwrap_or_default()))
    }

    /// Gives up the outstanding request: the sidecar skips the clauses it has not started, and
    /// the reply it may still send is dropped as stale.
    pub fn abandon(&mut self) {
        let Some(a) = self.active.take() else { return };
        if let Some(w) = self.proc.as_mut().and_then(|p| p.stdin.as_mut()) {
            let _ = writeln!(w, "{}", serde_json::json!({ "cancel": a.id })).and_then(|()| w.flush());
        }
    }

    /// Starts the sidecar as needed, waits up to `grace` for its model, sends `text` and waits for
    /// the first part. `Ok(None)`: still loading after `grace` (it carries on). A sidecar that
    /// dies before the first part is replaced once.
    pub fn first_part(&mut self, text: &str, whole: bool, grace: Duration, cancelled: &dyn Fn() -> bool) -> Result<Option<Part>, SidecarError> {
        let mut respawned = false;
        loop {
            self.start()?;
            if !self.ready(grace, cancelled)? {
                return Ok(None);
            }
            let got = self.begin(text, whole).and_then(|()| self.wait_part(cancelled));
            match got {
                Err(SidecarError::Died(why)) if !respawned => {
                    eprintln!("marlowe: voice: the Chatterbox sidecar died ({why}); starting a fresh one");
                    respawned = true;
                    self.reset();
                }
                other => return other.map(Some),
            }
        }
    }

    fn wait_part(&mut self, cancelled: &dyn Fn() -> bool) -> Result<Part, SidecarError> {
        loop {
            // `next_part` returns early on a part, an error, a cancel or the part's deadline.
            if let Some(part) = self.next_part(Duration::from_secs(3600), cancelled)? {
                return Ok(part);
            }
        }
    }

    /// Synthesizes all of `text` to one WAV, waiting for a cold model up to the load timeout.
    pub fn synthesize(&mut self, text: &str, cancelled: &dyn Fn() -> bool) -> Result<Vec<u8>, SidecarError> {
        let load = self.cfg.load_timeout;
        match self.first_part(text, true, load, cancelled)? {
            Some(part) => Ok(part.wav),
            None => Err(SidecarError::Start(format!("the model did not load within {} s", load.as_secs()))),
        }
    }
}

/// Reads and deletes a WAV the sidecar wrote. Only `utt-*.wav` directly inside `cache_dir`.
fn read_wav(cache_dir: &Path, path: &Path) -> Result<Vec<u8>, SidecarError> {
    let bad = |why: &str| SidecarError::Protocol(format!("refused the sidecar's file {}: {why}", path.display()));
    let name = path.file_name().and_then(|n| n.to_str()).ok_or_else(|| bad("no file name"))?;
    if !(name.starts_with("utt-") && name.ends_with(".wav")) {
        return Err(bad("not an utterance file"));
    }
    let dir = path.parent().and_then(|d| d.canonicalize().ok()).ok_or_else(|| bad("no such directory"))?;
    if Some(dir) != cache_dir.canonicalize().ok() {
        return Err(bad("outside the voice cache"));
    }
    let len = std::fs::metadata(path).map_err(|e| bad(&e.to_string()))?.len();
    if len > MAX_WAV {
        let _ = std::fs::remove_file(path);
        return Err(bad("too large"));
    }
    let bytes = std::fs::read(path).map_err(|e| bad(&e.to_string()));
    let _ = std::fs::remove_file(path);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real Chatterbox sidecar through this client: loads the model (GPU when free, else
    /// CPU; ~3 GB), synthesizes one line to a buffer and checks it. Plays nothing. Needs
    /// `MARLOWE_VOICE_PYTHON` and `MARLOWE_VOICE_REF` (and optionally `MARLOWE_VOICE_DEVICE`).
    #[test]
    #[ignore]
    fn live_chatterbox_synthesizes_a_line() {
        let s = crate::VoiceSettings::default().with_env(&|k| std::env::var(k).ok());
        let cb = s.chatterbox.expect("set MARLOWE_VOICE_PYTHON and MARLOWE_VOICE_REF");
        let home = std::env::temp_dir().join(format!("marlowe-voice-live-{}", std::process::id()));
        let mut sidecar = Sidecar::new(SidecarConfig::from_settings(&home, &cb, SCRUB));
        let t = Instant::now();
        let bytes = sidecar.synthesize("Good morning, sir. Systems are online.", &|| false).unwrap();
        let wav = crate::parse_wav(&bytes).unwrap();
        let peak = wav.samples.iter().fold(0f32, |m, s| m.max(s.abs()));
        println!(
            "{} bytes, {} Hz, {} ms of audio, peak {peak:.2}, {:.1} s including the load; ready: {:?}",
            bytes.len(),
            wav.sample_rate,
            wav.duration_ms(),
            t.elapsed().as_secs_f32(),
            sidecar.proc.as_ref().and_then(|p| p.ready.clone()),
        );
        assert!((1_000..15_000).contains(&wav.duration_ms()), "{} ms", wav.duration_ms());
        assert!(peak > 0.05, "silent");
        drop(sidecar);
        let _ = std::fs::remove_dir_all(home);
    }

    const SCRUB: &[&str] = &["OPENROUTER_API_KEY", "ANTHROPIC_API_KEY", "OPENAI_API_KEY", "HF_TOKEN"];

    #[test]
    fn credential_looking_names_are_secret() {
        for n in ["OPENROUTER_API_KEY", "hf_token", "GITHUB_TOKEN", "AWS_SECRET_ACCESS_KEY", "DB_PASSWORD", "MY_SECRET"] {
            assert!(is_secret_name(n), "{n}");
        }
        for n in ["PATH", "SystemRoot", "HF_HOME", "CUDA_VISIBLE_DEVICES", "TEMP", "TOKENIZERS_PARALLELISM"] {
            assert!(!is_secret_name(n), "{n}");
        }
    }

    #[test]
    fn the_command_scrubs_isolates_and_runs_in_the_cache_dir() {
        let cb = ChatterboxSettings { temperature: f32::NAN, exaggeration: 9.0, ..Default::default() };
        let cfg = SidecarConfig::from_settings(Path::new(r"C:\home"), &cb, &["OPENROUTER_API_KEY", "HF_TOKEN"]);
        assert_eq!(cfg.cache_dir, Path::new(r"C:\home\cache\voice"));
        let joined = cfg.args.join(" ");
        assert!(joined.contains("--temp 0.6") && joined.contains("--ex 2") && joined.contains("--device auto"), "{joined}");
        let cmd = cfg.command(Path::new("python.exe"), Path::new("s.py"), Path::new("me.wav"));
        let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(&args[..2], &["-I", "s.py"]);
        assert_eq!(cmd.get_current_dir(), Some(Path::new(r"C:\home\cache\voice")));
        let removed: Vec<String> =
            cmd.get_envs().filter(|(_, v)| v.is_none()).map(|(k, _)| k.to_string_lossy().into_owned()).collect();
        for k in ["OPENROUTER_API_KEY", "HF_TOKEN"] {
            assert!(removed.iter().any(|r| r.eq_ignore_ascii_case(k)), "{k} not scrubbed: {removed:?}");
        }
    }

    #[test]
    fn one_kind_of_failure_is_one_cause_whatever_its_numbers() {
        let a = SidecarError::Synth("OutOfMemoryError: CUDA out of memory. Tried to allocate 20.00 MiB".into());
        let b = SidecarError::Synth("OutOfMemoryError: CUDA out of memory. Tried to allocate 40.00 MiB".into());
        assert_eq!(a.cause(), b.cause());
        assert_eq!(a.cause(), "synth:OutOfMemoryError");
        assert_ne!(a.cause(), SidecarError::Synth("RuntimeError: text too strange".into()).cause());
        assert_eq!(SidecarError::Synth("no text".into()).cause(), "synth:");
        assert_eq!(SidecarError::Timeout("it did not answer within 61 s".into()).cause(), SidecarError::Timeout("within 75 s".into()).cause());
        assert!(a.to_string().starts_with("it could not say that: OutOfMemoryError"));
    }

    #[test]
    fn clauses_join_back_into_sentences() {
        let c = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(join_clauses(&c(&["Systems are online"])), "Systems are online.");
        assert_eq!(join_clauses(&c(&["Wait, what?", " ", "All good"])), "Wait, what? All good.");
        assert_eq!(join_clauses(&[]), "");
    }

    #[test]
    fn only_utterance_files_in_the_cache_are_read_and_they_are_deleted() {
        let dir = std::env::temp_dir().join(format!("marlowe-voice-readwav-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ok = dir.join("utt-1-1-1.wav");
        std::fs::write(&ok, b"RIFF").unwrap();
        assert_eq!(read_wav(&dir, &ok).unwrap(), b"RIFF");
        assert!(!ok.exists(), "read files are deleted");
        let other = dir.join("notes.wav");
        std::fs::write(&other, b"x").unwrap();
        assert!(matches!(read_wav(&dir, &other), Err(SidecarError::Protocol(_))));
        assert!(other.exists(), "a refused file is left alone");
        let outside = std::env::temp_dir().join(format!("utt-outside-{}.wav", std::process::id()));
        std::fs::write(&outside, b"x").unwrap();
        assert!(matches!(read_wav(&dir, &outside), Err(SidecarError::Protocol(_))));
        assert!(outside.exists());
        let _ = std::fs::remove_file(outside);
        let _ = std::fs::remove_dir_all(dir);
    }
}
