//! Consent-first discovery: finds directories that look like live runs and returns candidates.
//! Never adds a root, never writes. Directory listings only, plus reading `heartbeat.json` to
//! count live runs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use marlowe_proto::WatchCandidate;

use crate::run;

/// Directory depth scanned below the base.
pub const DEFAULT_DEPTH: usize = 4;

/// Only directories modified within this window are offered.
pub const RECENT: Duration = Duration::from_secs(7 * 24 * 3600);

/// Most directories listed in one discovery walk.
const MAX_DIRS: usize = 20_000;

/// A heartbeat fresher than this, with a live pid, makes a run live.
const LIVE_HEARTBEAT: Duration = Duration::from_secs(30 * 60);

/// Directory names never descended into.
pub(crate) fn skip_dir(name: &str) -> bool {
    name.starts_with('.')
        || matches!(
            name,
            "node_modules" | "target" | "__pycache__" | "venv" | "env" | "site-packages" | "dist-packages" | "$RECYCLE.BIN"
        )
}

/// `C:\Users\<user>\Projects` (from `USERPROFILE`, else `HOME`), when it exists.
pub fn projects_dir() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    let dir = PathBuf::from(home).join("Projects");
    dir.is_dir().then_some(dir)
}

pub(crate) fn ms(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// What one directory's listing says.
#[derive(Default)]
struct Signals {
    heartbeat: bool,
    trainer: bool,
    logs: u32,
    metric_json: u32,
    newest_ms: i64,
    has_project_marker: bool,
    is_repo: bool,
}

fn signals(dir: &Path) -> (Signals, Vec<PathBuf>) {
    let mut s = Signals::default();
    let mut subdirs = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return (s, subdirs) };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            if name == ".git" {
                s.is_repo = true;
            } else if !skip_dir(&name) {
                subdirs.push(e.path());
            }
            continue;
        }
        if !ft.is_file() {
            continue;
        }
        let lower = name.to_ascii_lowercase();
        let meta = e.metadata().ok();
        if let Some(m) = meta.as_ref().and_then(|m| m.modified().ok()) {
            s.newest_ms = s.newest_ms.max(ms(m));
        }
        match lower.as_str() {
            "heartbeat.json" => s.heartbeat = true,
            "trainer_state.json" => s.trainer = true,
            "readme.md" | "pyproject.toml" | "requirements.txt" | "setup.py" | "cargo.toml" | "package.json" | "method.md" => {
                s.has_project_marker = true
            }
            _ if lower.ends_with(".log") => s.logs += 1,
            _ if lower.ends_with(".json") && meta.is_some_and(|m| m.len() <= 1024 * 1024) => s.metric_json += 1,
            _ => {}
        }
    }
    (s, subdirs)
}

/// Run directories group under the project that holds them: the nearest ancestor (below
/// `base`) that is a git repo or has a project marker, else the parent of a `runs`/`outputs`
/// style folder, else the run directory itself.
fn candidate_root(base: &Path, run_dir: &Path, project_dirs: &[PathBuf]) -> PathBuf {
    for a in run_dir.ancestors() {
        if a == base || !a.starts_with(base) {
            break;
        }
        if project_dirs.iter().any(|p| p == a) {
            return a.to_path_buf();
        }
    }
    let parent = run_dir.parent().filter(|p| *p != base && p.starts_with(base));
    let grouping = |p: &Path| {
        let n = p.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        matches!(n.as_str(), "runs" | "outputs" | "output" | "logs" | "checkpoints" | "experiments" | "results")
    };
    match parent {
        Some(p) if grouping(p) && p.parent().is_some_and(|g| g != base && g.starts_with(base)) => p.parent().expect("checked").to_path_buf(),
        _ => run_dir.to_path_buf(),
    }
}

/// Directories under `base` (depth <= `max_depth`, skipping `node_modules`, `.git`, `target` and
/// hidden directories) modified within `recent` of `now` that look like runs: `heartbeat.json`,
/// `trainer_state.json`, or `*.log` alongside metric JSON. Runs are grouped under their project
/// directory. `watched` roots, anything under them and anything containing them are left out.
/// Newest first.
pub fn discover(base: &Path, now: SystemTime, max_depth: usize, recent: Duration, watched: &[PathBuf]) -> Vec<WatchCandidate> {
    let now_ms = ms(now);
    let cutoff = now_ms - recent.as_millis() as i64;
    let mut stack = vec![(base.to_path_buf(), 0usize)];
    let mut project_dirs = Vec::new();
    let mut run_dirs: Vec<(PathBuf, Signals)> = Vec::new();
    let mut listed = 0;
    while let Some((dir, depth)) = stack.pop() {
        listed += 1;
        if listed > MAX_DIRS {
            break;
        }
        let (s, subdirs) = signals(&dir);
        if depth > 0 && (s.is_repo || s.has_project_marker) {
            project_dirs.push(dir.clone());
        }
        let looks_like_run = s.heartbeat || s.trainer || (s.logs > 0 && s.metric_json > 0);
        if depth > 0 && looks_like_run && s.newest_ms >= cutoff {
            run_dirs.push((dir.clone(), s));
        }
        if depth < max_depth {
            stack.extend(subdirs.into_iter().map(|d| (d, depth + 1)));
        }
    }
    let mut groups: BTreeMap<PathBuf, (u32, u32, u32, i64, u32)> = BTreeMap::new();
    for (dir, s) in &run_dirs {
        let root = candidate_root(base, dir, &project_dirs);
        let g = groups.entry(root).or_default();
        g.0 += 1;
        g.1 += s.logs;
        g.2 += u32::from(s.heartbeat || s.trainer);
        g.3 = g.3.max(s.newest_ms);
        g.4 += u32::from(s.heartbeat && heartbeat_live(dir, now_ms));
    }
    let mut out: Vec<WatchCandidate> = groups
        .into_iter()
        .filter(|(root, _)| !watched.iter().any(|w| root.starts_with(w) || w.starts_with(root)))
        .map(|(root, (runs, logs, beats, newest, live))| {
            let mut why = vec![format!("{runs} run dir{}", if runs == 1 { "" } else { "s" })];
            if beats > 0 {
                why.push(format!("{beats} heartbeat{}", if beats == 1 { "" } else { "s" }));
            }
            if logs > 0 {
                why.push(format!("{logs} log{}", if logs == 1 { "" } else { "s" }));
            }
            WatchCandidate { path: root.to_string_lossy().into_owned(), reason: why.join(", "), modified_ms: newest, live_runs: live }
        })
        .collect();
    out.sort_by(|a, b| b.live_runs.cmp(&a.live_runs).then(b.modified_ms.cmp(&a.modified_ms)));
    out
}

fn heartbeat_live(dir: &Path, now_ms: i64) -> bool {
    let path = dir.join("heartbeat.json");
    if std::fs::metadata(&path).map(|m| m.len() > 64 * 1024).unwrap_or(true) {
        return false;
    }
    let Some(hb) = std::fs::read_to_string(&path).ok().and_then(|t| run::parse_heartbeat(&t)) else { return false };
    let fresh = hb.time_ms().is_some_and(|t| now_ms - t <= LIVE_HEARTBEAT.as_millis() as i64);
    fresh && hb.pid.is_some_and(|p| crate::process::alive_for(p, hb.time_ms()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn offers_recent_run_projects_only() {
        let base = std::env::temp_dir().join(format!("sentinel-discover-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let now = SystemTime::now();
        let now_s = now.duration_since(UNIX_EPOCH).unwrap().as_secs_f64();
        // PruningResearch/valiance/runs/night1 with a live heartbeat (this process).
        let night1 = base.join("PruningResearch").join("valiance").join("runs").join("night1");
        fs::create_dir_all(&night1).unwrap();
        fs::write(night1.join("heartbeat.json"), format!(r#"{{"stage":"audit","time":{now_s},"pid":{},"model":"m"}}"#, std::process::id())).unwrap();
        fs::write(night1.join("stage_27b.log"), "[05:56:27] === stage wnr finished\n").unwrap();
        fs::write(base.join("PruningResearch").join("valiance").join("METHOD.md"), "m").unwrap();
        // An HF output dir in a repo, with trainer_state in a checkpoint folder.
        let ckpt = base.join("tuner").join("out").join("checkpoint-500");
        fs::create_dir_all(&ckpt).unwrap();
        fs::create_dir_all(base.join("tuner").join(".git")).unwrap();
        fs::write(ckpt.join("trainer_state.json"), "{}").unwrap();
        // A stale run (two weeks old) and junk under node_modules.
        let old = base.join("old").join("run1");
        fs::create_dir_all(&old).unwrap();
        for f in ["train.log", "metrics.json"] {
            let p = old.join(f);
            fs::write(&p, "x").unwrap();
            fs::File::options().write(true).open(&p).unwrap().set_modified(now - Duration::from_secs(14 * 24 * 3600)).unwrap();
        }
        let nm = base.join("web").join("node_modules").join("pkg");
        fs::create_dir_all(&nm).unwrap();
        fs::write(nm.join("heartbeat.json"), "{}").unwrap();
        // Too deep.
        let deep = base.join("a").join("b").join("c").join("d").join("e");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("heartbeat.json"), "{}").unwrap();

        let found = discover(&base, now, DEFAULT_DEPTH, RECENT, &[]);
        let paths: Vec<&str> = found.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, vec![base.join("PruningResearch").join("valiance").to_str().unwrap(), base.join("tuner").to_str().unwrap()], "{found:?}");
        assert_eq!(found[0].live_runs, 1);
        assert!(found[0].reason.contains("1 run dir") && found[0].reason.contains("heartbeat"), "{}", found[0].reason);

        // Watched roots (or anything containing them) are not offered again.
        let watched = vec![base.join("PruningResearch").join("valiance")];
        let found = discover(&base, now, DEFAULT_DEPTH, RECENT, &watched);
        assert_eq!(found.len(), 1);
        assert!(found[0].path.ends_with("tuner"));
        let _ = fs::remove_dir_all(&base);
    }
}
