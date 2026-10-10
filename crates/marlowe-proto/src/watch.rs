//! Sentinel values on the wire: watch roots, runs, notices, discovery candidates, the brief, and
//! voice state. Shared by the daemon, `marlowe-sentinel`, `marlowe-voice` and the clients.

use serde::{Deserialize, Serialize};

/// How much a notice matters. Ordered: `Info < Good < Attention < Alarm`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    #[default]
    Info,
    /// Good news: finished cleanly, best so far.
    Good,
    /// Needs a look: stalled, plateau, a failed run.
    Attention,
    /// Act now: NaN/inf, OOM, divergence, killed.
    Alarm,
}

impl Severity {
    /// Attention and Alarm are spoken (outside quiet hours) and toasted.
    pub fn is_urgent(self) -> bool {
        self >= Severity::Attention
    }
}

/// Which watcher raised a notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeSource {
    /// A run directory's heartbeat / pid.
    Run,
    /// A tailed `*.log` / `*.txt`.
    Log,
    /// A metric or result JSON file.
    Metric,
    /// `trainer_state.json`, `*.jsonl`, `*.csv` metric logs.
    Trainer,
    Git,
    Ollama,
    Gpu,
    /// A discovery candidate (offered, never auto-added).
    Discovery,
    /// CI on a pushed branch (track J).
    Ci,
    /// The experiments bridge: metrics, plateaus, deadlines (track J2).
    Experiment,
}

/// One observation worth telling the user. Deduplicated by `(source, subject, kind)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notice {
    pub id: String,
    /// Unix epoch ms when it was raised (or last refreshed by a duplicate).
    pub at_ms: i64,
    pub source: NoticeSource,
    pub severity: Severity,
    /// Machine kind within the source, e.g. `finished`, `stalled`, `nan`, `best`, `plateau`,
    /// `new_commits`, `new_model`. Part of the dedup key.
    pub kind: String,
    /// One line, display-ready: `wnr5 finished`.
    pub title: String,
    /// Detail line: numbers flat and exact.
    #[serde(default)]
    pub detail: String,
    /// What it is about: a run dir, a file, a repo, a model tag.
    pub subject: String,
    /// Seen by the requesting client (per client; false in broadcasts).
    #[serde(default)]
    pub seen: bool,
}

/// Quiet hours as minutes after local midnight; `start > end` wraps midnight.
/// Quiet hours suppress speaking, never recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuietHours {
    pub start_min: u16,
    pub end_min: u16,
}

impl Default for QuietHours {
    /// 23:00–07:00.
    fn default() -> Self {
        Self { start_min: 23 * 60, end_min: 7 * 60 }
    }
}

impl QuietHours {
    /// True when `minute` (0..1440, local) falls inside the quiet window.
    pub fn contains(&self, minute: u16) -> bool {
        let m = minute % 1440;
        match self.start_min.cmp(&self.end_min) {
            std::cmp::Ordering::Equal => false,
            std::cmp::Ordering::Less => m >= self.start_min && m < self.end_min,
            std::cmp::Ordering::Greater => m >= self.start_min || m < self.end_min,
        }
    }

    /// `23:00–07:00`.
    pub fn label(&self) -> String {
        let f = |m: u16| format!("{:02}:{:02}", m / 60, m % 60);
        format!("{}–{}", f(self.start_min), f(self.end_min))
    }
}

/// A run directory's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Pid alive and heartbeat fresh (or a log still growing).
    Alive,
    /// Pid alive, heartbeat older than max(10 min, 5x its usual interval).
    Stalled,
    /// Pid gone; the newest log's last line says it completed.
    Finished,
    /// Pid gone; the newest log ends in an error, traceback, OOM or `Killed`.
    Failed,
    /// Pid gone and the logs do not say how.
    Ended,
}

impl RunStatus {
    pub fn is_live(self) -> bool {
        matches!(self, RunStatus::Alive | RunStatus::Stalled)
    }
}

/// One watched run directory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunInfo {
    /// The run directory.
    pub dir: String,
    /// Heartbeat `name`, else the directory name.
    pub name: String,
    pub status: RunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Heartbeat `time`, Unix epoch ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_ms: Option<i64>,
    /// Last meaningful line of the newest log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_line: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loss: Option<f64>,
    /// When the status last changed, Unix epoch ms.
    pub since_ms: i64,
}

/// A git repository's state, from read-only `git status --porcelain` / `git log -n`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitInfo {
    pub branch: String,
    /// Lines of `git status --porcelain`.
    pub dirty: u32,
    /// Short hash of HEAD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// HEAD's commit time, Unix epoch ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_ms: Option<i64>,
    /// Commits since the head last seen by the Sentinel.
    #[serde(default)]
    pub new_commits: u32,
}

/// One watch root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchRoot {
    pub path: String,
    pub added_ms: i64,
    /// Run directories found under it.
    #[serde(default)]
    pub runs: u32,
    /// Set when the root is (inside) a git repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitInfo>,
    /// Why the root is not being watched right now (missing, unreadable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// One GPU, from NVML.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuDevice {
    pub index: u32,
    pub name: String,
    pub util_pct: u32,
    pub mem_used_mb: u64,
    pub mem_total_mb: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temp_c: Option<u32>,
    /// Top process names using it, when NVML allows reading them.
    #[serde(default)]
    pub processes: Vec<String>,
}

/// GPU state. `unavailable` set (and no devices) when nvml.dll does not load.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuReport {
    #[serde(default)]
    pub devices: Vec<GpuDevice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
}

/// The Ollama model store, from `GET /api/tags` only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OllamaReport {
    pub reachable: bool,
    pub models: u32,
    /// Tags new or updated since last seen.
    #[serde(default)]
    pub new_models: Vec<String>,
}

/// Everything the Sentinel is watching, for the rail, the Watch view and the boot lines.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WatchState {
    pub roots: Vec<WatchRoot>,
    pub runs: Vec<RunInfo>,
    pub gpu: GpuReport,
    pub ollama: OllamaReport,
    #[serde(default)]
    pub quiet: QuietHours,
}

/// A directory discovery thinks is worth watching. Offered, never auto-added.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchCandidate {
    pub path: String,
    /// Why: `heartbeat.json, 3 logs, 2 metric files`.
    pub reason: String,
    /// Newest modification inside, Unix epoch ms.
    pub modified_ms: i64,
    /// Runs that look alive right now.
    #[serde(default)]
    pub live_runs: u32,
}

/// What a brief line is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BriefItemKind {
    Finished,
    Attention,
    Running,
    Model,
    Git,
    Memory,
}

/// One structured line of a brief.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BriefItem {
    pub kind: BriefItemKind,
    pub severity: Severity,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

/// Voice state for the presence meter. `envelope` is RMS amplitude, 0..=255, at
/// [`ENVELOPE_HZ`] frames per second, for the chunk being spoken.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceStatus {
    /// A speech backend is present on this machine.
    pub available: bool,
    pub enabled: bool,
    /// Display name of the chosen voice: `George (en-GB)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    pub speaking: bool,
    #[serde(default)]
    pub envelope: Vec<u8>,
}

/// Envelope frames per second.
pub const ENVELOPE_HZ: u32 = 30;
