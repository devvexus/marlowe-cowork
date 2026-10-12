//! The engine thread: owns every watched root's in-memory state (file marks, tails, runs),
//! turns file events and poll ticks into notices, and keeps the cached [`WatchState`].
//!
//! Strictly read-only towards watched roots: directory listings, metadata, and short reads of
//! files opened read-only (std's default share mode, read|write|delete) that are closed at once.
//! All state is written to `cowork.db` under `home`, never inside a root.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use marlowe_proto::{
    GitInfo, GpuReport, NoticeSource, OllamaReport, QuietHours, RunInfo, RunStatus, Severity, WatchRoot, WatchState,
};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};

mod results;

use crate::discover::skip_dir;
use crate::logs::{self, LineKind};
use crate::notice::NoticeDraft;
use crate::run::{self, Heartbeat, RunObservation};
use crate::store::{FileMark, SentinelStore};
use crate::trainer::{Record, TrainingSummary};
use crate::{now_ms, process, SentinelConfig, SentinelEvent, Sink};

/// Directory depth walked below a root.
const MAX_SCAN_DEPTH: usize = 8;
/// Most watched files per root; beyond it the root reports a problem.
const MAX_SCAN_FILES: usize = 50_000;
/// A full rescan runs every this many poll ticks while file events work (every tick otherwise).
const FULL_SCAN_EVERY: u64 = 4;
/// A log first seen (after the baseline) larger than this is tailed from its last 64 KiB.
const NEW_LOG_START: u64 = 1 << 20;
const NEW_LOG_TAIL: u64 = 64 * 1024;
/// Bytes read from a log's end to find its last line.
const LAST_LINE_TAIL: u64 = 8 * 1024;
const MAX_HEARTBEAT: u64 = 64 * 1024;
const MAX_STATE_JSON: u64 = 1 << 20;
const MAX_TRAINER_STATE: u64 = 16 << 20;
/// `*.jsonl`/`*.csv` larger than this when first seen are not metric logs.
const MAX_NEW_METRIC_LOG: u64 = 64 << 20;
/// History re-read for a metric log after a restart.
const HISTORY_BYTES: u64 = 256 * 1024;
const MAX_RECORDS: usize = 5_000;
const HEARTBEAT_HISTORY: usize = 16;
/// Finished runs stay in the state this long.
const RECENT_RUN_MS: i64 = 7 * 24 * 3600 * 1000;
/// Minor state changes (heartbeats, last lines, GPU) are broadcast at most this often.
const MINOR_STATE_EVERY: Duration = Duration::from_secs(60);
/// Most sibling files read to seed a metric family's best values.
const FAMILY_SEED_MAX: usize = 32;
const DETAIL_MAX: usize = 240;

/// Messages to the engine thread.
pub(crate) enum Cmd {
    Fs(notify::Result<notify::Event>),
    AddRoot { path: PathBuf, persisted: bool, added_ms: i64, done: Option<Sender<()>> },
    RemoveRoot { path: PathBuf, done: Sender<()> },
    Git(PathBuf, Option<GitInfo>),
    Gpu(GpuReport),
    Ollama(OllamaReport),
    Quiet(QuietHours),
    Shutdown,
}

/// Records a draft and, when it is new or escalated, sends it. Never blocks.
pub(crate) fn raise(store: &SentinelStore, sink: &Sink, draft: NoticeDraft) {
    if let Ok(Some(n)) = store.record(&draft, now_ms()) {
        let _ = sink.try_send(SentinelEvent::Notice(n));
    }
}

/// The database key for a path.
pub(crate) fn path_key(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// `root` with a trailing separator, for prefix queries.
fn prefix_of(root: &Path) -> String {
    let mut s = path_key(root);
    if !s.ends_with(std::path::MAIN_SEPARATOR) {
        s.push(std::path::MAIN_SEPARATOR);
    }
    s
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path_key(p))
}

fn modified_ms(m: &std::fs::Metadata) -> i64 {
    m.modified().map(crate::discover::ms).unwrap_or(0)
}

fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// `45 s`, `12 min`, `2 h 5 min`.
pub(crate) fn fmt_age(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    match s {
        0..=89 => format!("{s} s"),
        90..=5399 => format!("{} min", (s + 30) / 60),
        _ => format!("{} h {} min", s / 3600, (s % 3600) / 60),
    }
}

/// `HH:MM` local time for an epoch-ms instant.
fn local_clock(ms: i64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_millis_opt(ms).single() {
        Some(t) => t.format("%H:%M").to_string(),
        None => "?".into(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Heartbeat,
    RunState,
    Log,
    Trainer,
    Jsonl,
    Csv,
    Metric,
}

impl Kind {
    /// Read by appending tails rather than whole.
    fn tails(self) -> bool {
        matches!(self, Kind::Log | Kind::Jsonl | Kind::Csv)
    }
}

/// The kind of a watched file from its lowercased name; `.txt` counts only inside run dirs.
fn kind_of(name: &str, in_run_dir: bool) -> Option<Kind> {
    Some(match name {
        "heartbeat.json" => Kind::Heartbeat,
        "state.json" => Kind::RunState,
        "trainer_state.json" => Kind::Trainer,
        _ if name.ends_with(".log") => Kind::Log,
        _ if name.ends_with(".txt") && in_run_dir => Kind::Log,
        _ if name.ends_with(".jsonl") => Kind::Jsonl,
        _ if name.ends_with(".csv") => Kind::Csv,
        _ if name.ends_with(".json") => Kind::Metric,
        _ => return None,
    })
}

/// The run a `trainer_state.json` belongs to: its directory, or the output directory above a
/// `checkpoint-*` folder.
fn trainer_dir(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or(path);
    let is_ckpt = parent.file_name().is_some_and(|n| n.to_string_lossy().starts_with("checkpoint-"));
    match parent.parent() {
        Some(gp) if is_ckpt => gp.to_path_buf(),
        _ => parent.to_path_buf(),
    }
}

fn status_str(s: RunStatus) -> &'static str {
    match s {
        RunStatus::Alive => "alive",
        RunStatus::Stalled => "stalled",
        RunStatus::Finished => "finished",
        RunStatus::Failed => "failed",
        RunStatus::Ended => "ended",
    }
}

fn status_of(s: &str) -> Option<RunStatus> {
    Some(match s {
        "alive" => RunStatus::Alive,
        "stalled" => RunStatus::Stalled,
        "finished" => RunStatus::Finished,
        "failed" => RunStatus::Failed,
        "ended" => RunStatus::Ended,
        _ => return None,
    })
}

struct FileState {
    kind: Kind,
    mark: FileMark,
    /// Last meaningful line (logs).
    last: Option<(String, LineKind)>,
    /// The log's end was read once to find its last line.
    tail_read: bool,
    /// Parsed records (metric logs), newest last.
    records: Vec<Record>,
    header: Option<String>,
    history_loaded: bool,
    /// Not a metric log (a dataset): never read again.
    ignored: bool,
    summary: TrainingSummary,
}

impl FileState {
    fn new(kind: Kind, mark: FileMark) -> Self {
        Self {
            kind,
            mark,
            last: None,
            tail_read: false,
            records: Vec::new(),
            header: None,
            history_loaded: false,
            ignored: false,
            summary: TrainingSummary::default(),
        }
    }

    fn changed(&self, len: u64, mtime: i64) -> bool {
        self.mark.len != len || self.mark.modified_ms != mtime
    }
}

struct Run {
    heartbeat: Option<Heartbeat>,
    /// Distinct activity times (heartbeat, or log/trainer writes for pid-less runs), oldest first.
    beats: VecDeque<i64>,
    state_pid: Option<u32>,
    newest_log: Option<(PathBuf, i64)>,
    trainer_ms: Option<i64>,
    trainer_finished: bool,
    step: Option<u64>,
    loss: Option<f64>,
    last_line: Option<String>,
    activity_ms: i64,
    status: Option<RunStatus>,
    since_ms: i64,
    /// When a live run was first seen without its process (the end grace).
    ending: Option<i64>,
    /// Appeared after its root's baseline: a first sighting is news.
    fresh: bool,
}

impl Run {
    fn new(fresh: bool) -> Self {
        Self {
            heartbeat: None,
            beats: VecDeque::new(),
            state_pid: None,
            newest_log: None,
            trainer_ms: None,
            trainer_finished: false,
            step: None,
            loss: None,
            last_line: None,
            activity_ms: 0,
            status: None,
            since_ms: 0,
            ending: None,
            fresh,
        }
    }

    fn name(&self, dir: &Path) -> String {
        self.heartbeat.as_ref().and_then(|h| h.name.clone()).unwrap_or_else(|| file_name(dir))
    }

    fn note_beat(&mut self, t: i64) {
        if self.beats.back().is_none_or(|b| t > *b) {
            self.beats.push_back(t);
            if self.beats.len() > HEARTBEAT_HISTORY {
                self.beats.pop_front();
            }
        }
    }
}

struct Root {
    path: PathBuf,
    persisted: bool,
    added_ms: i64,
    watcher: Option<RecommendedWatcher>,
    problem: Option<String>,
    git: Option<GitInfo>,
    scanned: bool,
}

pub(crate) struct Engine {
    cfg: SentinelConfig,
    store: Arc<SentinelStore>,
    sink: Sink,
    home: PathBuf,
    cmd_tx: Sender<Cmd>,
    shared_roots: Arc<Mutex<Vec<PathBuf>>>,
    snapshot: Arc<Mutex<WatchState>>,
    roots: Vec<Root>,
    files: HashMap<PathBuf, FileState>,
    runs: BTreeMap<PathBuf, Run>,
    trainer_prev: HashMap<PathBuf, TrainingSummary>,
    pending: BTreeSet<PathBuf>,
    /// Processed without notices (a root's baseline).
    silent: HashSet<PathBuf>,
    debounce_until: Option<Instant>,
    rescan_soon: bool,
    budget: u64,
    gpu: GpuReport,
    ollama: OllamaReport,
    quiet: QuietHours,
    published: Option<WatchState>,
    minor_at: Instant,
    /// The registered probes ([`crate::probe::PROBES`]), ticked on every poll.
    probes: Vec<Box<dyn crate::probe::Probe>>,
}

impl Engine {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        cfg: SentinelConfig,
        store: Arc<SentinelStore>,
        sink: Sink,
        home: PathBuf,
        cmd_tx: Sender<Cmd>,
        shared_roots: Arc<Mutex<Vec<PathBuf>>>,
        snapshot: Arc<Mutex<WatchState>>,
        quiet: QuietHours,
    ) -> Self {
        let gpu = GpuReport { devices: Vec::new(), unavailable: Some("not sampled yet".into()) };
        let probes = crate::probe::start_all(&crate::probe::ProbeContext { home: &home });
        Self {
            cfg,
            store,
            sink,
            home,
            cmd_tx,
            shared_roots,
            snapshot,
            roots: Vec::new(),
            files: HashMap::new(),
            runs: BTreeMap::new(),
            trainer_prev: HashMap::new(),
            pending: BTreeSet::new(),
            silent: HashSet::new(),
            debounce_until: None,
            rescan_soon: false,
            budget: 0,
            gpu,
            ollama: OllamaReport::default(),
            quiet,
            published: None,
            minor_at: Instant::now(),
            probes,
        }
    }

    /// The thread body: initial roots, then events, debounce and poll ticks until shutdown.
    pub(crate) fn run(mut self, rx: Receiver<Cmd>, initial: Vec<(PathBuf, bool, i64)>) {
        for (path, persisted, added_ms) in initial {
            self.add_root(path, persisted, added_ms);
        }
        self.pass(false);
        let mut next_poll = Instant::now() + self.cfg.poll_interval;
        let mut ticks: u64 = 0;
        loop {
            let wake = self.debounce_until.map_or(next_poll, |d| d.min(next_poll));
            match rx.recv_timeout(wake.saturating_duration_since(Instant::now())) {
                Ok(Cmd::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(cmd) => self.handle(cmd),
                Err(RecvTimeoutError::Timeout) => {}
            }
            let now = Instant::now();
            if self.debounce_until.is_some_and(|d| d <= now) {
                self.debounce_until = None;
                let rescan = std::mem::take(&mut self.rescan_soon);
                self.pass(rescan);
            }
            if now >= next_poll {
                ticks += 1;
                let full = ticks.is_multiple_of(FULL_SCAN_EVERY) || self.roots.iter().any(|r| r.watcher.is_none());
                self.restat_runs();
                self.pass(full);
                self.tick_probes();
                next_poll = Instant::now() + self.cfg.poll_interval;
            }
        }
    }

    /// One processing pass: optional full scan, pending files within the byte budget, run
    /// liveness, then the cached state.
    fn pass(&mut self, full_scan: bool) {
        if full_scan {
            let roots: Vec<PathBuf> = self.roots.iter().map(|r| r.path.clone()).collect();
            for r in roots {
                self.scan_root(&r);
            }
        }
        self.drain();
        self.refresh_runs();
        self.publish();
    }

    /// Ticks every registered probe and records what it raised.
    fn tick_probes(&mut self) {
        let roots: Vec<PathBuf> = self.roots.iter().map(|r| r.path.clone()).collect();
        let now = now_ms();
        for p in &mut self.probes {
            for draft in p.tick(now, &roots) {
                raise(&self.store, &self.sink, draft);
            }
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Fs(Ok(ev)) => {
                if ev.need_rescan() {
                    // The OS event buffer overflowed: events were lost.
                    self.rescan_soon = true;
                    self.arm_debounce();
                }
                if matches!(ev.kind, notify::EventKind::Access(_)) {
                    return;
                }
                for p in ev.paths {
                    self.on_path(p);
                }
            }
            Cmd::Fs(Err(_)) => {
                self.rescan_soon = true;
                self.arm_debounce();
            }
            Cmd::AddRoot { path, persisted, added_ms, done } => {
                self.add_root(path, persisted, added_ms);
                self.pass(false);
                if let Some(done) = done {
                    let _ = done.send(());
                }
            }
            Cmd::RemoveRoot { path, done } => {
                self.remove_root(&path);
                self.publish();
                let _ = done.send(());
            }
            Cmd::Git(root, info) => {
                if let Some(r) = self.roots.iter_mut().find(|r| r.path == root) {
                    r.git = info;
                }
                self.publish();
            }
            Cmd::Gpu(report) => {
                self.gpu = report;
                self.publish();
            }
            Cmd::Ollama(report) => {
                self.ollama = report;
                self.publish();
            }
            Cmd::Quiet(q) => {
                self.quiet = q;
                self.publish();
            }
            Cmd::Shutdown => {}
        }
    }

    fn arm_debounce(&mut self) {
        if self.debounce_until.is_none() {
            self.debounce_until = Some(Instant::now() + self.cfg.fs_debounce);
        }
    }

    /// A file event: queue watched files, rescan on new directories, ignore everything else.
    fn on_path(&mut self, p: PathBuf) {
        if p.starts_with(&self.home) {
            return;
        }
        let Some(root) = self.roots.iter().find(|r| p.starts_with(&r.path)) else { return };
        let skipped = p
            .strip_prefix(&root.path)
            .map(|rel| rel.components().any(|c| skip_dir(&c.as_os_str().to_string_lossy())))
            .unwrap_or(true);
        if skipped {
            return;
        }
        let name = p.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        if kind_of(&name, true).is_some() {
            self.pending.insert(p);
        } else if p.extension().is_none() && p.is_dir() {
            self.rescan_soon = true;
        } else {
            return;
        }
        self.arm_debounce();
    }

    fn add_root(&mut self, path: PathBuf, persisted: bool, added_ms: i64) {
        if let Some(r) = self.roots.iter_mut().find(|r| r.path == path) {
            r.persisted |= persisted;
            return;
        }
        self.roots.push(Root { path: path.clone(), persisted, added_ms, watcher: None, problem: None, git: None, scanned: false });
        self.shared_roots.lock().unwrap_or_else(|p| p.into_inner()).push(path.clone());
        self.scan_root(&path);
    }

    fn remove_root(&mut self, path: &Path) {
        self.roots.retain(|r| r.path != path);
        self.shared_roots.lock().unwrap_or_else(|p| p.into_inner()).retain(|r| r != path);
        // Another root may still cover part of this one.
        let covered = |p: &Path, roots: &[Root]| roots.iter().any(|r| p.starts_with(&r.path));
        let gone: Vec<PathBuf> = self.files.keys().filter(|p| p.starts_with(path) && !covered(p, &self.roots)).cloned().collect();
        for p in gone {
            self.files.remove(&p);
            self.pending.remove(&p);
        }
        self.runs.retain(|d, _| !d.starts_with(path) || covered(d, &self.roots));
        if !covered(path, &self.roots) {
            let _ = self.store.forget_under(&prefix_of(path));
            let _ = self.store.delete_kv(&format!("baselined:{}", path_key(path)));
        }
    }

    fn watch(&mut self, idx: usize) {
        let tx = self.cmd_tx.clone();
        let root = self.roots[idx].path.clone();
        let made = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let _ = tx.send(Cmd::Fs(res));
        })
        .and_then(|mut w| w.watch(&root, RecursiveMode::Recursive).map(|_| w));
        // Without events the root is still polled every tick.
        self.roots[idx].watcher = made.ok();
    }

    /// Walks a root (listings and metadata only). The first scan ever is the baseline: files are
    /// marked at their current size and nothing historical raises a notice.
    fn scan_root(&mut self, root: &Path) {
        let Some(idx) = self.roots.iter().position(|r| r.path == root) else { return };
        if !root.is_dir() {
            self.roots[idx].problem = Some("missing or not a directory".into());
            self.roots[idx].watcher = None;
            return;
        }
        self.roots[idx].problem = None;
        if self.roots[idx].watcher.is_none() {
            self.watch(idx);
        }
        let bkey = format!("baselined:{}", path_key(root));
        let baseline = matches!(self.store.kv(&bkey), Ok(None));
        let first_in_process = !self.roots[idx].scanned;
        let persisted = if first_in_process && !baseline { self.store.file_marks_under(&prefix_of(root)).unwrap_or_default() } else { HashMap::new() };
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let mut run_dirs: HashSet<PathBuf> = HashSet::new();
        let mut batch: Vec<(String, FileMark)> = Vec::new();
        let mut truncated = false;
        let mut stack = vec![(root.to_path_buf(), 0usize)];
        'walk: while let Some((dir, depth)) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            let mut files = Vec::new();
            for e in rd.flatten() {
                let Ok(ft) = e.file_type() else { continue };
                let name = e.file_name().to_string_lossy().into_owned();
                if ft.is_dir() {
                    let p = e.path();
                    if depth < MAX_SCAN_DEPTH && !skip_dir(&name) && !p.starts_with(&self.home) {
                        stack.push((p, depth + 1));
                    }
                } else if ft.is_file() {
                    files.push((e.path(), name.to_ascii_lowercase(), e.metadata().ok()));
                }
            }
            let is_run = files.iter().any(|(_, n, _)| n == "heartbeat.json" || n == "state.json" || n.ends_with(".log"));
            if is_run {
                run_dirs.insert(dir.clone());
                self.runs.entry(dir.clone()).or_insert_with(|| Run::new(!baseline));
            }
            for (path, name, meta) in files {
                let (Some(meta), Some(kind)) = (meta, kind_of(&name, is_run)) else { continue };
                if seen.len() >= MAX_SCAN_FILES {
                    truncated = true;
                    break 'walk;
                }
                seen.insert(path.clone());
                if kind == Kind::Trainer {
                    let d = trainer_dir(&path);
                    run_dirs.insert(d.clone());
                    self.runs.entry(d).or_insert_with(|| Run::new(!baseline));
                }
                // A listing's size/mtime come from the directory index, which NTFS updates
                // lazily for files a writer holds open: ask the file itself for tailed kinds.
                let meta = if kind.tails() { std::fs::metadata(&path).unwrap_or(meta) } else { meta };
                let (len, mtime) = (meta.len(), modified_ms(&meta));
                if kind == Kind::Log {
                    self.note_log(&path, mtime);
                }
                if let Some(st) = self.files.get(&path) {
                    if st.changed(len, mtime) {
                        self.pending.insert(path);
                    }
                    continue;
                }
                let key = path_key(&path);
                let known = if first_in_process { persisted.get(&key).copied() } else { self.store.file_mark(&key).ok().flatten() };
                let st = match known {
                    Some(mark) => {
                        // Known from an earlier process. Run files and unchanged trainer states
                        // are re-read silently to rebuild in-memory state; changes made while
                        // the daemon was down (or bytes left unread) are processed normally.
                        let st = FileState::new(kind, mark);
                        let changed = st.changed(len, mtime);
                        let rebuild = matches!(kind, Kind::Heartbeat | Kind::RunState) || (kind == Kind::Trainer && !changed);
                        if rebuild && !changed {
                            self.silent.insert(path.clone());
                        }
                        if rebuild || changed || (kind.tails() && mark.offset < mark.len) {
                            self.pending.insert(path.clone());
                        }
                        st
                    }
                    None if baseline => {
                        let mark = FileMark { offset: len, len, modified_ms: mtime };
                        batch.push((key, mark));
                        if matches!(kind, Kind::Heartbeat | Kind::RunState | Kind::Trainer) {
                            self.silent.insert(path.clone());
                            self.pending.insert(path.clone());
                            FileState::new(kind, FileMark { offset: 0, len: 0, modified_ms: 0 })
                        } else {
                            FileState::new(kind, mark)
                        }
                    }
                    None => {
                        self.pending.insert(path.clone());
                        FileState::new(kind, first_mark(kind, len))
                    }
                };
                self.files.insert(path, st);
            }
        }
        if truncated {
            self.roots[idx].problem = Some(format!("more than {MAX_SCAN_FILES} watched files; the rest are not watched"));
        } else {
            let gone: Vec<PathBuf> = self.files.keys().filter(|p| p.starts_with(root) && !seen.contains(*p)).cloned().collect();
            for p in gone {
                self.forget(&p);
            }
            let stale: Vec<PathBuf> = self.runs.keys().filter(|d| d.starts_with(root) && !run_dirs.contains(*d)).cloned().collect();
            for d in stale {
                self.runs.remove(&d);
                let _ = self.store.delete_kv(&format!("run:{}", path_key(&d)));
            }
        }
        if !batch.is_empty() {
            let _ = self.store.set_file_marks(&batch);
        }
        if baseline {
            let _ = self.store.set_kv(&bkey, &now_ms().to_string());
        }
        self.roots[idx].scanned = true;
    }

    /// Re-checks every run's heartbeat and newest log (metadata only) on each poll tick, so
    /// liveness stays current even when file events are missed.
    fn restat_runs(&mut self) {
        let mut check: Vec<PathBuf> = Vec::new();
        for (dir, run) in &self.runs {
            check.push(dir.join("heartbeat.json"));
            if let Some((log, _)) = &run.newest_log {
                check.push(log.clone());
            }
        }
        for p in check {
            match (self.files.get(&p), std::fs::metadata(&p)) {
                (Some(st), Ok(m)) if st.changed(m.len(), modified_ms(&m)) => {
                    self.pending.insert(p);
                }
                (None, Ok(m)) if m.is_file() => {
                    self.pending.insert(p);
                }
                (Some(_), Err(_)) => {
                    self.pending.insert(p);
                }
                _ => {}
            }
        }
    }

    fn forget(&mut self, p: &Path) {
        if let Some(st) = self.files.remove(p) {
            if st.kind == Kind::Log {
                let parent = p.parent().map(Path::to_path_buf);
                if let Some(run) = parent.as_ref().and_then(|d| self.runs.get_mut(d)) {
                    if run.newest_log.as_ref().is_some_and(|(l, _)| l == p) {
                        run.newest_log = None;
                    }
                }
                if let Some(d) = parent {
                    self.recompute_newest_log(&d);
                }
            }
        }
        self.pending.remove(p);
        let _ = self.store.forget_file(&path_key(p));
    }

    fn recompute_newest_log(&mut self, dir: &Path) {
        let newest = self
            .files
            .iter()
            .filter(|(p, st)| st.kind == Kind::Log && p.parent() == Some(dir))
            .map(|(p, st)| (p.clone(), st.mark.modified_ms))
            .max_by_key(|(_, t)| *t);
        if let Some(run) = self.runs.get_mut(dir) {
            run.newest_log = newest;
        }
    }

    fn note_log(&mut self, path: &Path, mtime: i64) {
        let Some(run) = path.parent().and_then(|d| self.runs.get_mut(d)) else { return };
        let newer = run.newest_log.as_ref().is_none_or(|(p, t)| p == path || mtime >= *t);
        if newer {
            run.newest_log = Some((path.to_path_buf(), mtime));
        }
    }

    /// Processes pending files until the per-tick byte budget runs out; the rest wait.
    fn drain(&mut self) {
        self.budget = self.cfg.max_read_per_tick.max(1);
        let pending: Vec<PathBuf> = std::mem::take(&mut self.pending).into_iter().collect();
        for (i, p) in pending.iter().enumerate() {
            if self.budget == 0 {
                self.pending.extend(pending[i..].iter().cloned());
                break;
            }
            if !self.process(p) {
                self.pending.insert(p.clone());
            }
        }
    }

    /// Reads one changed file. False when it has more to read (budget).
    fn process(&mut self, path: &Path) -> bool {
        let meta = match std::fs::metadata(path) {
            Ok(m) if m.is_file() => m,
            _ => {
                self.forget(path);
                return true;
            }
        };
        let (len, mtime) = (meta.len(), modified_ms(&meta));
        if !self.files.contains_key(path) {
            let Some(st) = self.new_file(path, len) else { return true };
            self.files.insert(path.to_path_buf(), st);
        }
        let silent = self.silent.remove(path);
        let kind = self.files[path].kind;
        if kind == Kind::Log {
            self.note_log(path, mtime);
        }
        let done = match kind {
            Kind::Heartbeat => self.read_heartbeat(path, len),
            Kind::RunState => self.read_run_state(path, len),
            Kind::Log => self.tail_log(path, silent),
            Kind::Jsonl | Kind::Csv => self.tail_metric_log(path, len, silent),
            Kind::Trainer => self.read_trainer(path, len, mtime, silent),
            Kind::Metric => self.read_metric(path, len, mtime, silent),
        };
        if let Some(st) = self.files.get_mut(path) {
            st.mark.len = len;
            st.mark.modified_ms = mtime;
            if !kind.tails() {
                st.mark.offset = len;
            }
            // The offset (bytes consumed) persists, so unread bytes survive a restart.
            let _ = self.store.set_file_mark(&path_key(path), st.mark);
        }
        done
    }

    /// State for a file first met through an event (after its root's baseline).
    fn new_file(&mut self, path: &Path, len: u64) -> Option<FileState> {
        let name = path.file_name()?.to_string_lossy().to_ascii_lowercase();
        let parent = path.parent()?.to_path_buf();
        let in_run = self.runs.contains_key(&parent) || run::is_run_dir(&parent);
        let kind = kind_of(&name, in_run)?;
        if matches!(kind, Kind::Heartbeat | Kind::RunState | Kind::Log) {
            self.runs.entry(parent).or_insert_with(|| Run::new(true));
        }
        if kind == Kind::Trainer {
            self.runs.entry(trainer_dir(path)).or_insert_with(|| Run::new(true));
        }
        let mark = self.store.file_mark(&path_key(path)).ok().flatten().unwrap_or_else(|| first_mark(kind, len));
        Some(FileState::new(kind, mark))
    }

    /// A newline-less read this long is one over-long line (the full per-tick cap, never the
    /// leftover budget).
    fn long_line(&self) -> u64 {
        self.cfg.max_read_per_tick.max(1)
    }

    fn spend(&mut self, bytes: u64) {
        self.budget = self.budget.saturating_sub(bytes);
    }

    fn read_heartbeat(&mut self, path: &Path, len: u64) -> bool {
        if len > MAX_HEARTBEAT {
            return true;
        }
        let Ok(bytes) = logs::read_capped(path, MAX_HEARTBEAT) else { return true };
        self.spend(bytes.len() as u64);
        // A half-written heartbeat keeps the previous one.
        let Some(hb) = run::parse_heartbeat(&String::from_utf8_lossy(&bytes)) else { return true };
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let run = self.runs.entry(dir).or_insert_with(|| Run::new(true));
        if let Some(t) = hb.time_ms() {
            run.note_beat(t);
        }
        run.heartbeat = Some(hb);
        true
    }

    fn read_run_state(&mut self, path: &Path, len: u64) -> bool {
        if len > MAX_STATE_JSON {
            return true;
        }
        let Ok(bytes) = logs::read_capped(path, MAX_STATE_JSON) else { return true };
        self.spend(bytes.len() as u64);
        let pid = run::state_pid(&String::from_utf8_lossy(&bytes));
        if let Some(run) = path.parent().and_then(|d| self.runs.get_mut(d)) {
            run.state_pid = pid.or(run.state_pid);
        }
        true
    }

    fn tail_log(&mut self, path: &Path, silent: bool) -> bool {
        let from = self.files[path].mark.offset;
        let Ok(a) = logs::read_appended(path, from, self.budget, self.long_line()) else { return true };
        let read = a.next_offset - if a.restarted { 0 } else { from };
        self.spend(read.max(1));
        let st = self.files.get_mut(path).expect("present");
        st.mark.offset = a.next_offset;
        if let Some(last) = logs::last_meaningful(&a.lines) {
            st.last = Some(last);
            st.tail_read = true;
        }
        if !silent {
            // The most severe line of this read; the latest of equals (an exception line after
            // its traceback).
            let worst = a.lines.iter().filter_map(|l| {
                let k = logs::classify_line(l);
                k.severity().map(|s| (s, k, l))
            });
            if let Some((sev, k, line)) = worst.max_by_key(|(s, _, _)| *s) {
                self.log_notice(path, sev, k, line);
            }
        }
        a.caught_up()
    }

    fn log_notice(&self, path: &Path, sev: Severity, k: LineKind, line: &str) {
        let label = file_name(path);
        let at = logs::parse_timestamp(line).map(|s| format!(" at {:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)).unwrap_or_default();
        let (kind, title) = match k {
            LineKind::Oom => ("oom", format!("{label} ran out of memory{at}")),
            LineKind::Killed => ("killed", format!("{label}: process killed{at}")),
            LineKind::NonFinite => ("nan", format!("{label} logged NaN/inf{at}")),
            LineKind::Error | LineKind::Traceback => ("error", format!("{label} logged an error{at}")),
            LineKind::Completion => ("completion", format!("{label}: completed{at}")),
            LineKind::Progress | LineKind::Other => return,
        };
        let draft = NoticeDraft::new(NoticeSource::Log, sev, kind, title, clip(line, DETAIL_MAX), path_key(path));
        raise(&self.store, &self.sink, draft);
    }

    /// The last meaningful line of a log, reading its last 8 KiB once if no tail has shown one.
    fn ensure_last_line(&mut self, log: &Path) -> Option<(String, LineKind)> {
        let st = self.files.get_mut(log)?;
        if st.last.is_none() && !st.tail_read {
            st.tail_read = true;
            if let Ok((lines, _)) = logs::read_tail(log, LAST_LINE_TAIL) {
                st.last = logs::last_meaningful(&lines);
            }
        }
        st.last.clone()
    }

    /// Classifies every run and raises notices for status changes (including ones that
    /// happened while the daemon was down, from the persisted status).
    fn refresh_runs(&mut self) {
        let now = now_ms();
        let dirs: Vec<PathBuf> = self.runs.keys().cloned().collect();
        for dir in dirs {
            let newest = self.runs[&dir].newest_log.clone();
            let last = newest.as_ref().and_then(|(p, _)| self.ensure_last_line(p));
            let Some(run) = self.runs.get_mut(&dir) else { continue };
            let hb_ms = run.heartbeat.as_ref().and_then(Heartbeat::time_ms);
            let pid = run.heartbeat.as_ref().and_then(|h| h.pid).or(run.state_pid);
            let alive = pid.map(|p| process::alive_for(p, hb_ms));
            let activity = [hb_ms, newest.as_ref().map(|(_, t)| *t), run.trainer_ms].into_iter().flatten().max();
            run.activity_ms = activity.unwrap_or(0);
            if pid.is_none() {
                if let Some(t) = activity {
                    run.note_beat(t);
                }
            }
            let mut last_kind = last.as_ref().map(|(_, k)| *k);
            // `X.gguf written, check OK` ends a run as surely as `done` (only read once it is gone).
            if let Some((line, LineKind::Progress)) = &last {
                match logs::ending(line) {
                    logs::Ending::Success => last_kind = Some(LineKind::Completion),
                    logs::Ending::Failure(_) => last_kind = Some(LineKind::Error),
                    logs::Ending::Unknown => {}
                }
            }
            if run.trainer_finished && last_kind.is_none_or(|k| !k.is_failure()) {
                last_kind = Some(LineKind::Completion);
            }
            let obs = RunObservation {
                heartbeat_ms: if pid.is_some() { hb_ms } else { activity },
                pid_alive: alive,
                now_ms: now,
                usual_interval: run::usual_interval(run.beats.make_contiguous()),
                stall_floor: self.cfg.stall_floor,
                last_line: last_kind,
            };
            let mut status = run::classify(&obs);
            // A live run must stay ended for `end_grace` before it counts as ended.
            match run.status {
                Some(live) if live.is_live() && !status.is_live() => {
                    let since = *run.ending.get_or_insert(now);
                    if now - since < self.cfg.end_grace.as_millis() as i64 {
                        status = live;
                    } else {
                        run.ending = None;
                    }
                }
                _ => run.ending = None,
            }
            run.last_line = last.map(|(l, _)| clip(&l, DETAIL_MAX));
            let prev = run.status;
            run.status = Some(status);
            if prev == Some(status) {
                continue;
            }
            let kv_key = format!("run:{}", path_key(&dir));
            let saved = if prev.is_none() {
                self.store.kv(&kv_key).ok().flatten().and_then(|v| {
                    let (s, since) = v.split_once('|')?;
                    Some((status_of(s)?, since.parse::<i64>().ok()?))
                })
            } else {
                None
            };
            let run = self.runs.get_mut(&dir).expect("present");
            let from = match (prev, saved) {
                (Some(p), _) => Some(Some(p)),
                (None, Some((s, since))) if s == status => {
                    run.since_ms = since;
                    None
                }
                (None, Some((s, _))) => Some(Some(s)),
                (None, None) if run.fresh => Some(None),
                // Baseline: only a stalled run is worth saying now.
                (None, None) => (status == RunStatus::Stalled).then_some(None),
            };
            // Not seen happening in this process (a baseline, or an end while the daemon was
            // down): an ended run's time is its last observed activity, not now.
            let late = prev.is_none() && !status.is_live() && run.activity_ms > 0 && run.activity_ms < now;
            if prev.is_some() || saved.is_none_or(|(s, _)| s != status) {
                let since = if late { run.activity_ms } else { now };
                run.since_ms = since;
                let _ = self.store.set_kv(&kv_key, &format!("{}|{since}", status_str(status)));
            }
            if let Some(from) = from {
                if let Some(mut d) = self.run_notice(&dir, from, status, last_kind, obs.heartbeat_ms, now) {
                    if late {
                        // The notice is raised now; its title carries when it happened.
                        d.title.push_str(&format!(" at {}", local_clock(self.runs[&dir].activity_ms)));
                    }
                    raise(&self.store, &self.sink, d);
                }
            }
        }
    }

    fn run_notice(&self, dir: &Path, from: Option<RunStatus>, to: RunStatus, last: Option<LineKind>, hb_ms: Option<i64>, now: i64) -> Option<NoticeDraft> {
        let run = self.runs.get(dir)?;
        let name = run.name(dir);
        let hb = run.heartbeat.as_ref();
        let mut parts: Vec<String> = Vec::new();
        if let Some(stage) = hb.and_then(|h| h.stage.as_deref()) {
            parts.push(format!("stage {stage}"));
        }
        if let Some(layer) = hb.and_then(|h| h.layer) {
            parts.push(format!("layer {layer}"));
        }
        if let Some(step) = run.step {
            parts.push(format!("step {step}"));
        }
        let with_line = |mut parts: Vec<String>| {
            if let Some(l) = &run.last_line {
                parts.push(l.clone());
            }
            parts.join(" · ")
        };
        let (sev, kind, title, detail) = match to {
            RunStatus::Stalled => {
                let quiet = hb_ms.map(|t| format!("no heartbeat for {}", fmt_age(now - t))).unwrap_or_else(|| "no heartbeat".into());
                parts.insert(0, quiet);
                (Severity::Attention, "stalled", format!("{name} stalled"), parts.join(" · "))
            }
            RunStatus::Finished => (Severity::Good, "finished", format!("{name} finished"), with_line(parts)),
            RunStatus::Failed => {
                let sev = if matches!(last, Some(LineKind::Oom | LineKind::Killed | LineKind::NonFinite)) { Severity::Alarm } else { Severity::Attention };
                (sev, "failed", format!("{name} failed"), with_line(parts))
            }
            RunStatus::Ended => (Severity::Info, "ended", format!("{name} ended"), with_line(parts)),
            RunStatus::Alive => match from {
                Some(RunStatus::Stalled) => (Severity::Info, "resumed", format!("{name} resumed"), parts.join(" · ")),
                Some(RunStatus::Alive) => return None,
                _ => (Severity::Info, "started", format!("{name} started"), parts.join(" · ")),
            },
        };
        Some(NoticeDraft::new(NoticeSource::Run, sev, kind, title, detail, path_key(dir)))
    }

    fn build_state(&self) -> WatchState {
        let now = now_ms();
        let roots = self
            .roots
            .iter()
            .map(|r| WatchRoot {
                path: path_key(&r.path),
                added_ms: r.added_ms,
                runs: self.runs.keys().filter(|d| d.starts_with(&r.path)).count() as u32,
                git: r.git.clone(),
                problem: r.problem.clone(),
            })
            .collect();
        let mut runs: Vec<RunInfo> = self
            .runs
            .iter()
            .filter_map(|(dir, r)| {
                let status = r.status?;
                (status.is_live() || now - r.activity_ms <= RECENT_RUN_MS).then(|| {
                    let hb = r.heartbeat.as_ref();
                    RunInfo {
                        dir: path_key(dir),
                        name: r.name(dir),
                        status,
                        stage: hb.and_then(|h| h.stage.clone()),
                        layer: hb.and_then(|h| h.layer),
                        pid: hb.and_then(|h| h.pid).or(r.state_pid),
                        heartbeat_ms: hb.and_then(Heartbeat::time_ms),
                        last_line: r.last_line.clone(),
                        step: r.step,
                        loss: r.loss.filter(|l| l.is_finite()),
                        since_ms: r.since_ms,
                    }
                })
            })
            .collect();
        runs.sort_by(|a, b| b.status.is_live().cmp(&a.status.is_live()).then(b.since_ms.cmp(&a.since_ms)).then(a.dir.cmp(&b.dir)));
        WatchState { roots, runs, gpu: self.gpu.clone(), ollama: self.ollama.clone(), quiet: self.quiet }
    }

    /// Updates the cached state and broadcasts it: at once for structural changes (roots, run
    /// statuses, models, quiet hours), at most once a minute for the rest.
    fn publish(&mut self) {
        let state = self.build_state();
        *self.snapshot.lock().unwrap_or_else(|p| p.into_inner()) = state.clone();
        let major = |s: &WatchState| {
            (
                s.roots.iter().map(|r| (r.path.clone(), r.runs, r.problem.clone(), r.git.as_ref().map(|g| (g.head.clone(), g.dirty)))).collect::<Vec<_>>(),
                s.runs.iter().map(|r| (r.dir.clone(), r.status)).collect::<Vec<_>>(),
                (s.ollama.reachable, s.ollama.models, s.ollama.new_models.clone()),
                s.quiet,
            )
        };
        let send = match &self.published {
            None => true,
            Some(p) if major(p) != major(&state) => true,
            Some(p) => *p != state && self.minor_at.elapsed() >= MINOR_STATE_EVERY,
        };
        if send {
            self.minor_at = Instant::now();
            let _ = self.sink.try_send(SentinelEvent::State(state.clone()));
            self.published = Some(state);
        }
    }
}

/// The resume mark for a file met after its root's baseline: logs larger than 1 MiB start near
/// their end; everything else is read from the start.
fn first_mark(kind: Kind, len: u64) -> FileMark {
    let offset = if kind == Kind::Log && len > NEW_LOG_START { len - NEW_LOG_TAIL } else { 0 };
    FileMark { offset, len: 0, modified_ms: 0 }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_and_helpers() {
        assert_eq!(kind_of("heartbeat.json", false), Some(Kind::Heartbeat));
        assert_eq!(kind_of("stage_27b.log", false), Some(Kind::Log));
        assert_eq!(kind_of("notes.txt", false), None);
        assert_eq!(kind_of("notes.txt", true), Some(Kind::Log));
        assert_eq!(kind_of("metrics.jsonl", false), Some(Kind::Jsonl));
        assert_eq!(kind_of("model.safetensors", true), None);
        assert_eq!(trainer_dir(Path::new("out/checkpoint-500/trainer_state.json")), PathBuf::from("out"));
        assert_eq!(trainer_dir(Path::new("out/trainer_state.json")), PathBuf::from("out"));
        assert_eq!(fmt_age(45_000), "45 s");
        assert_eq!(fmt_age(12 * 60_000), "12 min");
        assert_eq!(fmt_age(125 * 60_000), "2 h 5 min");
        assert_eq!(clip("abcdef", 4), "abc…");
        assert_eq!(first_mark(Kind::Log, 2 << 20).offset, (2 << 20) - NEW_LOG_TAIL);
        assert_eq!(status_of(status_str(RunStatus::Stalled)), Some(RunStatus::Stalled));
    }
}
