//! Run directories: `heartbeat.json` + pid liveness + the newest log's last line -> [`RunStatus`].

use std::path::Path;
use std::time::Duration;

use marlowe_proto::RunStatus;
use serde_json::Value;

use crate::logs::LineKind;

/// `heartbeat.json`, e.g.
/// `{"stage":"wnr-p5","time":1760000000,"pid":48440,"layer":9,"name":"marlowe-valiance-22b-wnr5"}`.
/// Every field is optional; unknown fields are ignored. `model` stands in for a missing `name`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Heartbeat {
    pub stage: Option<String>,
    /// Epoch seconds (fractional allowed).
    pub time_s: Option<f64>,
    pub pid: Option<u32>,
    pub layer: Option<i64>,
    pub name: Option<String>,
}

impl Heartbeat {
    /// `time_s` as epoch ms.
    pub fn time_ms(&self) -> Option<i64> {
        self.time_s.map(|t| (t * 1000.0) as i64)
    }
}

/// Parses a heartbeat file's text. `None` when it is not a JSON object.
pub fn parse_heartbeat(text: &str) -> Option<Heartbeat> {
    let v: Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
    let o = v.as_object()?;
    let text_of = |k: &str| match o.get(k)? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    };
    let num = |k: &str| match o.get(k)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    };
    Some(Heartbeat {
        stage: text_of("stage"),
        time_s: num("time").or_else(|| num("timestamp")).filter(|t| t.is_finite() && *t > 0.0),
        pid: num("pid").filter(|p| *p >= 1.0 && *p <= u32::MAX as f64).map(|p| p as u32),
        layer: num("layer").filter(|l| l.is_finite()).map(|l| l as i64),
        name: text_of("name").or_else(|| text_of("model")),
    })
}

/// The orchestrator pid in a `state.json` (`{"_orchestrator":{"pid":4464}}`), used when the
/// heartbeat names none.
pub fn state_pid(text: &str) -> Option<u32> {
    let v: Value = serde_json::from_str(text).ok()?;
    let pid = v.get("_orchestrator").and_then(|o| o.get("pid")).or_else(|| v.get("pid"))?.as_u64()?;
    u32::try_from(pid).ok().filter(|p| *p > 0)
}

/// Whether `dir`'s listing makes it a run directory: `heartbeat.json`, `state.json` or a `*.log`.
pub fn is_run_dir(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else { return false };
    entries.flatten().any(|e| {
        let name = e.file_name().to_string_lossy().to_ascii_lowercase();
        let file = e.file_type().is_ok_and(|t| t.is_file());
        file && (name == "heartbeat.json" || name == "state.json" || name.ends_with(".log"))
    })
}

/// How long a heartbeat may go quiet before the run counts as stalled:
/// max(`floor`, 5x `usual_interval`).
pub fn stall_after(usual_interval: Option<Duration>, floor: Duration) -> Duration {
    usual_interval.map(|i| i * 5).unwrap_or_default().max(floor)
}

/// The usual gap between heartbeat updates: the median of the gaps between distinct times.
pub fn usual_interval(times_ms: &[i64]) -> Option<Duration> {
    let mut gaps: Vec<i64> = times_ms.windows(2).map(|w| w[1] - w[0]).filter(|g| *g > 0).collect();
    if gaps.is_empty() {
        return None;
    }
    gaps.sort_unstable();
    Some(Duration::from_millis(gaps[gaps.len() / 2] as u64))
}

/// Inputs to [`classify`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RunObservation {
    /// Heartbeat time, epoch ms. For a run with no pid, the newest activity (heartbeat or log
    /// write).
    pub heartbeat_ms: Option<i64>,
    /// `None` when there is no pid to check.
    pub pid_alive: Option<bool>,
    pub now_ms: i64,
    /// Typical gap between heartbeat updates, when known.
    pub usual_interval: Option<Duration>,
    pub stall_floor: Duration,
    /// Kind of the newest log's last meaningful line.
    pub last_line: Option<LineKind>,
}

/// Alive / Stalled / Finished / Failed / Ended.
///
/// With a live pid: Stalled once the heartbeat is older than [`stall_after`], else Alive. With no
/// pid: Alive while activity is fresh. Otherwise the last log line says how it ended.
pub fn classify(obs: &RunObservation) -> RunStatus {
    let limit = stall_after(obs.usual_interval, obs.stall_floor).as_millis() as i64;
    let stale = obs.heartbeat_ms.is_none_or(|t| obs.now_ms - t > limit);
    match obs.pid_alive {
        Some(true) if obs.heartbeat_ms.is_some() && stale => RunStatus::Stalled,
        Some(true) => RunStatus::Alive,
        None if !stale => RunStatus::Alive,
        _ => match obs.last_line {
            Some(LineKind::Completion) => RunStatus::Finished,
            Some(k) if k.is_failure() => RunStatus::Failed,
            _ => RunStatus::Ended,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_heartbeats_with_aliases() {
        let hb = parse_heartbeat(r#"{"stage":"wnr-p5","time":1760000000.5,"pid":48440,"layer":9,"name":"marlowe-valiance-22b-wnr5"}"#).unwrap();
        assert_eq!(hb.stage.as_deref(), Some("wnr-p5"));
        assert_eq!(hb.time_ms(), Some(1_760_000_000_500));
        assert_eq!((hb.pid, hb.layer), (Some(48440), Some(9)));
        assert_eq!(hb.name.as_deref(), Some("marlowe-valiance-22b-wnr5"));
        let real = parse_heartbeat(r#"{"stage": "audit", "time": 1791543474.25, "pid": 28828, "model": "marlowe-valiance-22b-wnr6", "layer": 24}"#).unwrap();
        assert_eq!(real.name.as_deref(), Some("marlowe-valiance-22b-wnr6"));
        assert_eq!(parse_heartbeat("[1,2]"), None);
        assert_eq!(parse_heartbeat("{}"), Some(Heartbeat::default()));
        assert_eq!(state_pid(r#"{"_orchestrator":{"pid":4464,"started":1.0},"x":{"t0":2}}"#), Some(4464));
    }

    #[test]
    fn usual_interval_is_the_median_gap() {
        assert_eq!(usual_interval(&[0, 1000, 2000, 2000, 9000]), Some(Duration::from_millis(1000)));
        assert_eq!(usual_interval(&[5]), None);
        assert_eq!(stall_after(Some(Duration::from_secs(300)), Duration::from_secs(600)), Duration::from_secs(1500));
        assert_eq!(stall_after(None, Duration::from_secs(600)), Duration::from_secs(600));
    }

    #[test]
    fn classifies_runs() {
        let min = 60_000;
        let base = RunObservation {
            heartbeat_ms: Some(100 * min),
            pid_alive: Some(true),
            now_ms: 105 * min,
            usual_interval: Some(Duration::from_secs(30)),
            stall_floor: Duration::from_secs(600),
            last_line: Some(LineKind::Progress),
        };
        assert_eq!(classify(&base), RunStatus::Alive);
        assert_eq!(classify(&RunObservation { now_ms: 111 * min, ..base }), RunStatus::Stalled);
        // A slow heartbeat (every 4 min) earns 20 min of grace.
        let slow = RunObservation { usual_interval: Some(Duration::from_secs(240)), now_ms: 115 * min, ..base };
        assert_eq!(classify(&slow), RunStatus::Alive);
        let dead = RunObservation { pid_alive: Some(false), ..base };
        assert_eq!(classify(&dead), RunStatus::Ended);
        assert_eq!(classify(&RunObservation { last_line: Some(LineKind::Completion), ..dead }), RunStatus::Finished);
        assert_eq!(classify(&RunObservation { last_line: Some(LineKind::Oom), ..dead }), RunStatus::Failed);
        // No pid: fresh activity is alive; stale activity ends by the last line.
        let nopid = RunObservation { pid_alive: None, ..base };
        assert_eq!(classify(&nopid), RunStatus::Alive);
        assert_eq!(classify(&RunObservation { now_ms: 200 * min, last_line: Some(LineKind::Completion), ..nopid }), RunStatus::Finished);
        assert_eq!(classify(&RunObservation { heartbeat_ms: None, ..nopid }), RunStatus::Ended);
    }

    #[test]
    fn detects_run_dirs_from_the_listing() {
        let dir = std::env::temp_dir().join(format!("sentinel-rundir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!is_run_dir(&dir));
        std::fs::write(dir.join("notes.md"), "x").unwrap();
        assert!(!is_run_dir(&dir));
        std::fs::write(dir.join("stage_27b.log"), "x").unwrap();
        assert!(is_run_dir(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
