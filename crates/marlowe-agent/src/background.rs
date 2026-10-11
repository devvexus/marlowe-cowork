//! What the background tools (track N: `cron_*`, `schedule_wakeup`, `list_agents`,
//! `send_message` to a run or session, `notify`, `send_file`, `notices`, `watch`) reach in the
//! daemon: the [`Background`] service. The tools check and shape their arguments (they live in
//! `marlowe-exec`); the daemon's [`BackgroundOps`] does the effects, so a test can run a tool
//! against a fake and the daemon against the real thing.
//!
//! The task registry itself (`task_list`, `task_output`, `task_stop`, `monitor`) is the separate
//! [`crate::Tasks`] service.

use std::path::Path;
use std::sync::Arc;

use marlowe_model::CancelToken;
use marlowe_proto::{Notice, ScheduleInfo, SentFile, SessionId, Urgency, WatchState};

/// What a turn started by background news begins with (a relay: not the user's words, so memory
/// never ingests it at user trust). The sources follow, comma separated, then `]`.
pub const RELAY_BACKGROUND: &str = "[crew · run background · ";

/// The sources of a turn started by background news (`monitor`, `cron, wakeup`), if `text` is
/// one: what a UserPromptSubmit hook sees as `source` (X22).
pub fn relay_source(text: &str) -> Option<String> {
    let rest = text.strip_prefix(RELAY_BACKGROUND)?;
    let end = rest.find(']')?;
    Some(rest[..end].trim().to_string()).filter(|s| !s.is_empty())
}

/// Shortest and longest `schedule_wakeup` delay.
pub const WAKEUP_MIN_SECS: u64 = 60;
pub const WAKEUP_MAX_SECS: u64 = 3_600;

/// A delay clamped to what `schedule_wakeup` allows, and whether it had to be.
pub fn clamp_wakeup(delay_s: i64) -> (u64, bool) {
    let d = delay_s.clamp(WAKEUP_MIN_SECS as i64, WAKEUP_MAX_SECS as i64) as u64;
    (d, d as i64 != delay_s)
}

/// What `cron_create` asks for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CronRequest {
    /// A 5-field expression, local time. Exactly one of `cron` and `once_at_ms`.
    pub cron: Option<String>,
    pub once_at_ms: Option<i64>,
    /// What runs: the prompt the session is woken with.
    pub prompt: String,
    /// Survives a daemon restart (and asks the user, since it outlives the session).
    pub durable: bool,
    /// `schedule_wakeup`: a one-shot that wakes the session as a wake-up (with this reason), not
    /// as a cron fire.
    pub wakeup: Option<String>,
    /// The session had read untrusted content when it made this: what it fires is untrusted too.
    pub tainted: bool,
}

/// One agent-like thing `list_agents` shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    /// What `send_message` takes as `to`: `agent-2`, a run id, `session:<id>`.
    pub id: String,
    /// `subagent`, `crew run`, `workflow`, `session`.
    pub kind: String,
    pub status: String,
    pub title: String,
    pub workspace: Option<String>,
}

/// Which notices `notices` returns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NoticeQuery {
    /// Only those this agent has not been shown (and marks them shown).
    pub unread: bool,
    /// Raised or refreshed at or after this time.
    pub since_ms: Option<i64>,
    /// `run`, `log`, `ci`, `task`, `message`, `notify`, ...
    pub source: Option<String>,
    pub limit: usize,
}

/// `watch`: what to do with a watch root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchAction {
    List,
    Add(String),
    Remove(String),
}

/// The daemon's side of the background tools. Every method is cheap or hands its work to a
/// thread: a tool call must never block on a model, a child process or the user.
pub trait BackgroundOps: Send + Sync {
    /// Creates a schedule that wakes `session` with the prompt. Session-scoped unless durable.
    fn cron_create(&self, session: &SessionId, req: CronRequest) -> Result<ScheduleInfo, String>;
    /// The session's schedules and the durable ones.
    fn cron_list(&self, session: &SessionId) -> Vec<ScheduleInfo>;
    fn cron_delete(&self, session: &SessionId, id: &str) -> Result<(), String>;
    /// This session's agents, crew runs, workflows and the other local sessions.
    fn list_agents(&self, session: &SessionId) -> Vec<AgentRow>;
    /// Delivers `text` to a subagent's continuation, a crew run, a workflow agent or another
    /// session (`session:<id>`). Returns what happened, for the model. Cross-session text arrives
    /// at agent trust with `from` as its lineage, never at user trust.
    fn send_message(&self, from: &SessionId, to: &str, text: &str, tainted: bool, cancel: &CancelToken) -> Result<String, String>;
    /// A toast (when on), a spoken line outside quiet hours for `Normal` and up, and a notice.
    /// Returns what was done, or that it was rate limited.
    fn notify(&self, session: &SessionId, title: &str, body: &str, urgency: Urgency) -> Result<String, String>;
    /// Copies `path` to the outbox and announces a file card.
    fn send_file(&self, session: &SessionId, path: &Path, note: &str) -> Result<SentFile, String>;
    fn notices(&self, session: &SessionId, query: &NoticeQuery) -> Result<Vec<Notice>, String>;
    /// Adds, removes or lists the Sentinel's watched folders.
    fn watch(&self, action: WatchAction) -> Result<WatchState, String>;
}

/// The service in `Services` (one per session).
#[derive(Clone)]
pub struct Background(pub Arc<dyn BackgroundOps>);

impl std::fmt::Debug for Background {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Background")
    }
}

/// Urgency from the word the model gave (`low`, `normal`, `urgent`); anything else is an error
/// the tool reports.
pub fn parse_urgency(word: &str) -> Option<Urgency> {
    match word.trim().to_ascii_lowercase().as_str() {
        "" | "normal" => Some(Urgency::Normal),
        "low" => Some(Urgency::Low),
        "urgent" | "high" => Some(Urgency::Urgent),
        _ => None,
    }
}

/// A [`BackgroundOps`] that keeps everything in memory, for tests of the tools (and anything else
/// that wants a daemon-less `Background`). Schedules get ids and next-fire times the way the
/// daemon's do; messages, notifications and sent files are recorded; notices are seen once.
#[derive(Default)]
pub struct FakeBackground {
    pub schedules: std::sync::Mutex<Vec<(SessionId, ScheduleInfo)>>,
    pub messages: std::sync::Mutex<Vec<(String, String, String)>>,
    pub notified: std::sync::Mutex<Vec<(String, String, Urgency)>>,
    pub sent: std::sync::Mutex<Vec<(std::path::PathBuf, String)>>,
    pub notices: std::sync::Mutex<Vec<Notice>>,
    pub seen: std::sync::Mutex<std::collections::HashSet<String>>,
    pub roots: std::sync::Mutex<Vec<String>>,
    pub agents: std::sync::Mutex<Vec<AgentRow>>,
    next: std::sync::atomic::AtomicU64,
}

fn guard<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

impl BackgroundOps for FakeBackground {
    fn cron_create(&self, session: &SessionId, req: CronRequest) -> Result<ScheduleInfo, String> {
        let n = self.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let next = match (&req.cron, req.once_at_ms) {
            (Some(c), None) => crate::cron::CronExpr::parse(c).ok().and_then(|e| crate::cron::next_fire_ms(&e, now_ms())),
            (None, Some(at)) => Some(at),
            _ => return Err("give exactly one of a cron expression and a time".into()),
        };
        let info = ScheduleInfo {
            id: format!("cron_{n}"),
            session: Some(session.clone()),
            text: req.prompt,
            next_at_ms: next,
            cron: req.cron,
            once_at_ms: req.once_at_ms,
            durable: req.durable,
            ..ScheduleInfo::default()
        };
        guard(&self.schedules).push((session.clone(), info.clone()));
        Ok(info)
    }

    fn cron_list(&self, session: &SessionId) -> Vec<ScheduleInfo> {
        guard(&self.schedules).iter().filter(|(s, i)| s == session || i.durable).map(|(_, i)| i.clone()).collect()
    }

    fn cron_delete(&self, session: &SessionId, id: &str) -> Result<(), String> {
        let mut all = guard(&self.schedules);
        let before = all.len();
        all.retain(|(s, i)| !(i.id == id && (s == session || i.durable)));
        if all.len() < before { Ok(()) } else { Err(format!("no schedule `{id}`")) }
    }

    fn list_agents(&self, _session: &SessionId) -> Vec<AgentRow> {
        guard(&self.agents).clone()
    }

    fn send_message(&self, from: &SessionId, to: &str, text: &str, _tainted: bool, _cancel: &CancelToken) -> Result<String, String> {
        guard(&self.messages).push((from.to_string(), to.to_string(), text.to_string()));
        Ok(format!("Delivered to {to}."))
    }

    fn notify(&self, _session: &SessionId, title: &str, body: &str, urgency: Urgency) -> Result<String, String> {
        guard(&self.notified).push((title.to_string(), body.to_string(), urgency));
        Ok("Notified: toast shown.".into())
    }

    fn send_file(&self, _session: &SessionId, path: &Path, note: &str) -> Result<SentFile, String> {
        guard(&self.sent).push((path.to_path_buf(), note.to_string()));
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(SentFile { name, path: path.display().to_string(), bytes: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0), digest: String::new(), note: note.to_string() })
    }

    fn notices(&self, _session: &SessionId, query: &NoticeQuery) -> Result<Vec<Notice>, String> {
        let all = guard(&self.notices).clone();
        let mut seen = guard(&self.seen);
        let out: Vec<Notice> = all
            .into_iter()
            .filter(|n| !query.unread || !seen.contains(&n.id))
            .filter(|n| query.since_ms.is_none_or(|t| n.at_ms >= t))
            .filter(|n| query.source.as_deref().is_none_or(|s| format!("{:?}", n.source).to_ascii_lowercase() == s))
            .take(query.limit.max(1))
            .collect();
        for n in &out {
            seen.insert(n.id.clone());
        }
        Ok(out)
    }

    fn watch(&self, action: WatchAction) -> Result<WatchState, String> {
        let mut roots = guard(&self.roots);
        match action {
            WatchAction::List => {}
            WatchAction::Add(p) => {
                if !roots.contains(&p) {
                    roots.push(p);
                }
            }
            WatchAction::Remove(p) => roots.retain(|r| *r != p),
        }
        Ok(WatchState {
            roots: roots
                .iter()
                .map(|p| marlowe_proto::WatchRoot { path: p.clone(), added_ms: 0, runs: 0, git: None, problem: None })
                .collect(),
            ..WatchState::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wakeup_delays_clamp_to_a_minute_and_an_hour() {
        let table = [
            (-5, (60, true)),
            (0, (60, true)),
            (59, (60, true)),
            (60, (60, false)),
            (300, (300, false)),
            (3_600, (3_600, false)),
            (3_601, (3_600, true)),
            (86_400, (3_600, true)),
        ];
        for (input, want) in table {
            assert_eq!(clamp_wakeup(input), want, "delay {input}");
        }
    }

    #[test]
    fn a_background_turn_names_its_sources() {
        assert_eq!(relay_source("[crew · run background · monitor]\n[monitor mon_w]"), Some("monitor".into()));
        assert_eq!(relay_source("[crew · run background · cron, wakeup]\nx"), Some("cron, wakeup".into()));
        assert_eq!(relay_source("[crew · run 3f2a · analyst]"), None);
        assert_eq!(relay_source("fix the bug"), None);
    }

    #[test]
    fn urgency_words() {
        assert_eq!(parse_urgency(""), Some(Urgency::Normal));
        assert_eq!(parse_urgency("LOW"), Some(Urgency::Low));
        assert_eq!(parse_urgency("urgent"), Some(Urgency::Urgent));
        assert_eq!(parse_urgency("whenever"), None);
    }
}
