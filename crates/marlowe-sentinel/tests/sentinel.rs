//! End-to-end Sentinel tests on temp-dir fixtures shaped like a real research tree
//! (`runs/night1` with a heartbeat and stage logs, `scratch/27b` audit JSON siblings, an HF
//! `trainer_state.json`, a JSONL metric log, a git repo). Never touches a real project.

use std::collections::BTreeMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError};
use marlowe_sentinel::{Notice, NoticeSource, RunStatus, Sentinel, SentinelConfig, SentinelEvent, Severity};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos();
        let p = std::env::temp_dir().join(format!("sentinel-it-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn now_s() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64()
}

fn fast_cfg() -> SentinelConfig {
    SentinelConfig {
        fs_debounce: Duration::from_millis(50),
        poll_interval: Duration::from_millis(250),
        git_interval: Duration::from_millis(500),
        git_timeout: Duration::from_secs(10),
        ollama_addr: None,
        gpu: false,
        stall_floor: Duration::from_secs(120),
        plateau_evals: 3,
        end_grace: Duration::from_millis(500),
        ..SentinelConfig::default()
    }
}

fn git_available() -> bool {
    Command::new("git").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    assert!(ok, "git {args:?}");
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn append(path: &Path, text: &str) {
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(text.as_bytes()).unwrap();
}

fn heartbeat(pid: u32, time_s: f64, stage: &str) -> String {
    format!(r#"{{"stage":"{stage}","time":{time_s},"pid":{pid},"layer":9,"name":"marlowe-valiance-22b-wnr5"}}"#)
}

/// A pid that existed a moment ago and has exited.
fn dead_pid() -> u32 {
    let mut child = Command::new("git").arg("--version").stdout(Stdio::null()).spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

/// The valiance-shaped tree. Returns the run dir.
fn fixture(root: &Path, with_git: bool) -> PathBuf {
    let night1 = root.join("runs").join("night1");
    write(&night1.join("heartbeat.json"), &heartbeat(std::process::id(), now_s(), "wnr-p5"));
    write(&night1.join("state.json"), r#"{"_orchestrator": {"pid": 4464, "started": 1791421726.9}, "audit-parent": {"attempts": 1, "status": "done"}}"#);
    write(
        &night1.join("stage_27b.log"),
        "[05:19:34] === stage wnr {'name': 'marlowe-valiance-22b-wnr5', 'set': 'b'}\n\
         C:\\code\\vstages.py:620: FutureWarning: You are using `torch.load` with `weights_only=False`\n  W = torch.load(wpath)\n\
         [05:19:40] wnr marlowe-valiance-22b-wnr5: step 0.25 x Newton(cv): held-out KL 0.16401, top-1 88.49%\n",
    );
    write(&night1.join("alerts.log"), "[09:15:41] ALERT orchestrator (pid 24752) is dead with the queue unfinished -> relaunching\n");
    // alerts.log is older than the stage log, so the stage log is the run's newest.
    let old = SystemTime::now() - Duration::from_secs(3600);
    std::fs::File::options().write(true).open(night1.join("alerts.log")).unwrap().set_modified(old).unwrap();
    let scratch = root.join("scratch").join("27b");
    write(
        &scratch.join("audit3_marlowe-valiance-22b-wnr3.json"),
        r#"{"thinking": {"loss": 1.2101365725199382, "kl": 0.08271126720743875, "top1": 0.8753255208333334, "unigram": 7.20926757551885, "retained": 0.9918812690453025}}"#,
    );
    write(
        &scratch.join("audit3_marlowe-valiance-22b-refit1.json"),
        r#"{"thinking": {"loss": 1.25, "kl": 0.09, "top1": 0.87, "unigram": 7.3, "retained": 0.99}}"#,
    );
    write(&root.join("node_modules").join("pkg").join("noise.log"), "Traceback (most recent call last):\n");
    write(&root.join("README.md"), "valiance\n");
    if with_git {
        git(root, &["init", "-q"]);
        git(root, &["add", "-A"]);
        git(root, &["commit", "-q", "-m", "fixture"]);
    }
    night1
}

fn wait_for<T>(what: &str, timeout: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn find_notice(s: &Sentinel, source: NoticeSource, kind: &str) -> Option<Notice> {
    s.notices("test", 200, false).unwrap().into_iter().find(|n| n.source == source && n.kind == kind)
}

fn drain(rx: &Receiver<SentinelEvent>) -> Vec<SentinelEvent> {
    rx.try_iter().collect()
}

const WAIT: Duration = Duration::from_secs(20);

#[test]
fn end_to_end_on_a_valiance_like_root() {
    let has_git = git_available();
    let root = TempDir::new("root");
    let home = TempDir::new("home");
    let night1 = fixture(&root.0, has_git);
    let (tx, rx) = unbounded();
    let s = Sentinel::start_with(&home.0, vec![], tx, fast_cfg()).unwrap();
    assert!(s.state().roots.is_empty(), "nothing is watched without consent");
    s.add_root(&root.0).unwrap();

    // The run is alive with its heartbeat fields and the stage log's last meaningful line.
    let run = wait_for("night1 alive", WAIT, || s.state().runs.into_iter().find(|r| r.dir.ends_with("night1") && r.status == RunStatus::Alive));
    assert_eq!((run.name.as_str(), run.stage.as_deref(), run.layer, run.pid), ("marlowe-valiance-22b-wnr5", Some("wnr-p5"), Some(9), Some(std::process::id())));
    assert!(run.last_line.as_deref().unwrap().contains("held-out KL 0.16401"), "{:?}", run.last_line);
    let state = s.state();
    assert_eq!(state.roots.len(), 1);
    assert!(state.runs.iter().all(|r| !r.dir.contains("node_modules")));
    if has_git {
        let g = wait_for("git info", WAIT, || s.state().roots[0].git.clone());
        assert_eq!((g.branch.as_str(), g.dirty, g.subject.as_deref()), ("main", 0, Some("fixture")));
    }
    // The baseline is silent: history (the ALERT line, existing audits) raises nothing.
    std::thread::sleep(Duration::from_millis(600));
    assert!(s.notices("test", 100, false).unwrap().is_empty(), "{:?}", s.notices("test", 100, false));

    // A new sibling audit: deltas against wnr3, best-so-far by name heuristics only.
    write(
        &root.0.join("scratch").join("27b").join("audit3_marlowe-valiance-22b-wnr4.json"),
        r#"{"thinking": {"loss": 1.218, "kl": 0.0803, "top1": 0.875, "unigram": 7.21, "retained": 0.9905}}"#,
    );
    let m = wait_for("metric notice", WAIT, || find_notice(&s, NoticeSource::Metric, "result"));
    assert_eq!(m.severity, Severity::Good, "{m:?}");
    assert_eq!(m.title, "audit3_marlowe-valiance-22b-wnr4: best kl so far");
    assert!(m.detail.contains("loss 1.218 (wnr3 1.21, +0.007863)"), "{}", m.detail);
    assert!(m.detail.contains("kl 0.0803 (wnr3 0.08271, -0.002411, best)"), "{}", m.detail);
    assert!(!m.detail.contains("unigram"), "no direction is claimed for unigram: {}", m.detail);

    // Best on more than three metrics: the title counts them instead of listing them.
    write(
        &root.0.join("scratch").join("27b").join("audit3_marlowe-valiance-22b-wnr5.json"),
        r#"{"thinking": {"loss": 1.2, "kl": 0.07, "top1": 0.9, "unigram": 7.0, "retained": 0.995}}"#,
    );
    let m5 = wait_for("wnr5 notice", WAIT, || {
        s.notices("test", 100, false).unwrap().into_iter().find(|n| n.source == NoticeSource::Metric && n.title.contains("wnr5"))
    });
    assert_eq!(m5.title, "audit3_marlowe-valiance-22b-wnr5: best so far on 4 metrics");

    // An OOM line appended to the stage log.
    append(&night1.join("stage_27b.log"), "[05:19:50] torch.OutOfMemoryError: CUDA out of memory. Tried to allocate 2.00 GiB\n");
    let oom = wait_for("oom notice", WAIT, || find_notice(&s, NoticeSource::Log, "oom"));
    assert_eq!(oom.severity, Severity::Alarm);
    assert_eq!(oom.title, "stage_27b.log ran out of memory at 05:19:50");

    // An HF checkpoint whose trainer_state logged a NaN loss.
    write(
        &root.0.join("runs").join("hf").join("checkpoint-20").join("trainer_state.json"),
        r#"{"global_step": 20, "max_steps": 100, "log_history": [{"step": 10, "loss": 2.31}, {"step": 20, "loss": NaN, "grad_norm": NaN}]}"#,
    );
    let nan = wait_for("trainer nan", WAIT, || find_notice(&s, NoticeSource::Trainer, "nan"));
    assert_eq!(nan.severity, Severity::Alarm);
    assert!(nan.subject.ends_with("hf"), "checkpoints share the output dir: {}", nan.subject);

    // A JSONL metric log whose eval loss plateaus.
    let lines = [1.90, 1.70, 1.72, 1.71, 1.70]
        .iter()
        .enumerate()
        .map(|(i, e)| format!("{{\"step\": {}, \"loss\": {}, \"eval_loss\": {e}}}\n", (i + 1) * 100, 2.0 - i as f64 * 0.1))
        .collect::<String>();
    write(&night1.join("metrics.jsonl"), &lines);
    let plateau = wait_for("plateau", WAIT, || find_notice(&s, NoticeSource::Trainer, "plateau"));
    assert_eq!(plateau.severity, Severity::Attention);
    assert!(plateau.detail.contains("eval_loss 1.7"), "{}", plateau.detail);
    // A dataset-shaped JSONL is ignored.
    write(&night1.join("data.jsonl"), "{\"id\": 1, \"text\": \"loss of signal\"}\n");

    if has_git {
        git(&root.0, &["commit", "-q", "--allow-empty", "-m", "try wnr6"]);
        let g = wait_for("git notice", WAIT, || find_notice(&s, NoticeSource::Git, "new_commits"));
        assert!(g.title.ends_with(": 1 new commit on main"), "{}", g.title);
        assert_eq!(g.detail, "latest: try wnr6");
        let ctx = s.git_context(&night1).unwrap();
        assert_eq!(ctx.subject.as_deref(), Some("try wnr6"));
    }

    // Broadcasts carried the notices; seen marks are per client.
    let events = drain(&rx);
    let kinds: Vec<&str> = events.iter().filter_map(|e| if let SentinelEvent::Notice(n) = e { Some(n.kind.as_str()) } else { None }).collect();
    for k in ["result", "oom", "nan", "plateau"] {
        assert!(kinds.contains(&k), "{k} missing from {kinds:?}");
    }
    assert!(events.iter().any(|e| matches!(e, SentinelEvent::State(_))));
    assert!(!kinds.contains(&"error"), "data.jsonl and node_modules raise nothing: {kinds:?}");
    let ranked = s.notices("tui", 100, false).unwrap();
    assert_eq!(ranked[0].severity, Severity::Alarm, "ranked by severity");
    assert!(s.mark_seen("tui", &oom.id).unwrap());
    assert!(s.notices("tui", 100, true).unwrap().iter().all(|n| n.id != oom.id));
    assert!(s.notices("cli", 100, true).unwrap().iter().any(|n| n.id == oom.id));
    assert!(s.notices_since(0).unwrap().len() >= 4);

    // Removing the root forgets it; shutdown closes the sink.
    let state = s.remove_root(&root.0).unwrap();
    assert!(state.roots.is_empty() && state.runs.is_empty());
    assert!(matches!(s.remove_root(&root.0), Err(marlowe_sentinel::SentinelError::NotWatched(_))));
    assert!(matches!(s.add_root(&root.0.join("nope")), Err(marlowe_sentinel::SentinelError::NotADirectory(_))));
    s.shutdown();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Err(RecvTimeoutError::Disconnected) => break,
            _ => assert!(Instant::now() < deadline, "sink still open after shutdown"),
        }
    }
}

#[test]
fn stall_resume_and_finish_are_reported() {
    let root = TempDir::new("stall");
    let home = TempDir::new("stall-home");
    let night1 = fixture(&root.0, false);
    let (tx, _rx) = unbounded();
    let cfg = SentinelConfig { stall_floor: Duration::from_secs(2), ..fast_cfg() };
    let s = Sentinel::start_with(&home.0, vec![], tx, cfg).unwrap();
    s.add_root(&root.0).unwrap();
    let night = |s: &Sentinel| s.state().runs.into_iter().find(|r| r.dir.ends_with("night1"));
    wait_for("alive", WAIT, || night(&s).filter(|r| r.status == RunStatus::Alive));
    // No heartbeat for longer than the floor while the pid lives: stalled.
    let stalled = wait_for("stalled notice", WAIT, || find_notice(&s, NoticeSource::Run, "stalled"));
    assert_eq!(stalled.severity, Severity::Attention);
    assert_eq!(stalled.title, "marlowe-valiance-22b-wnr5 stalled");
    assert!(stalled.detail.starts_with("no heartbeat for ") && stalled.detail.contains("stage wnr-p5 · layer 9"), "{}", stalled.detail);
    assert_eq!(night(&s).unwrap().status, RunStatus::Stalled);
    // A fresh heartbeat resumes it.
    write(&night1.join("heartbeat.json"), &heartbeat(std::process::id(), now_s(), "audit"));
    wait_for("resumed", WAIT, || find_notice(&s, NoticeSource::Run, "resumed"));
    // The process exits after logging the stage's end: finished.
    append(&night1.join("stage_27b.log"), "[05:56:27] === stage audit finished\n");
    write(&night1.join("heartbeat.json"), &heartbeat(dead_pid(), now_s(), "audit"));
    let done = wait_for("finished", WAIT, || find_notice(&s, NoticeSource::Run, "finished"));
    assert_eq!(done.severity, Severity::Good);
    assert!(done.detail.contains("=== stage audit finished"), "{}", done.detail);
    assert_eq!(night(&s).unwrap().status, RunStatus::Finished);
    s.shutdown();
}

#[test]
fn changes_while_down_are_reported_on_restart() {
    let root = TempDir::new("restart");
    let home = TempDir::new("restart-home");
    let night1 = fixture(&root.0, false);
    {
        let (tx, _rx) = unbounded();
        let s = Sentinel::start_with(&home.0, vec![], tx, fast_cfg()).unwrap();
        s.add_root(&root.0).unwrap();
        wait_for("alive", WAIT, || s.state().runs.into_iter().find(|r| r.status == RunStatus::Alive));
        s.shutdown();
    }
    // While the daemon is down the run hits an OOM and its process exits.
    append(&night1.join("stage_27b.log"), "[06:00:01] RuntimeError: CUDA out of memory\n");
    write(&night1.join("heartbeat.json"), &heartbeat(dead_pid(), now_s() - 5.0, "wnr-p6"));
    let (tx, _rx) = unbounded();
    let s = Sentinel::start_with(&home.0, vec![], tx, fast_cfg()).unwrap();
    // The root persisted; no re-add needed.
    let failed = wait_for("failed notice", WAIT, || find_notice(&s, NoticeSource::Run, "failed"));
    assert_eq!(failed.severity, Severity::Alarm, "an OOM ending is an alarm");
    assert!(find_notice(&s, NoticeSource::Log, "oom").is_some());
    assert_eq!(s.state().roots.len(), 1);
    assert_eq!(s.state().runs[0].status, RunStatus::Failed);
    s.shutdown();
}

#[test]
fn a_run_that_ended_before_its_root_was_added_keeps_its_own_end_time() {
    let root = TempDir::new("baseline-ended");
    let home = TempDir::new("baseline-ended-home");
    let run = root.0.join("runs").join("old1");
    let three_h = Duration::from_secs(3 * 3600);
    write(&run.join("heartbeat.json"), &heartbeat(dead_pid(), now_s() - three_h.as_secs_f64(), "audit"));
    write(&run.join("stage.log"), "[01:00:00] === stage audit finished\n");
    let old = SystemTime::now() - three_h;
    for f in ["heartbeat.json", "stage.log"] {
        std::fs::File::options().write(true).open(run.join(f)).unwrap().set_modified(old).unwrap();
    }
    let added_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
    let (tx, _rx) = unbounded();
    let s = Sentinel::start_with(&home.0, vec![], tx, fast_cfg()).unwrap();
    s.add_root(&root.0).unwrap();
    let r = wait_for("the old run", WAIT, || s.state().runs.into_iter().find(|r| r.status == RunStatus::Finished));
    assert!(r.since_ms <= added_ms - 2 * 3600 * 1000, "since is the last activity, not the add time: {}", r.since_ms);
    s.shutdown();
    // The stored time survives a restart.
    let (tx, _rx) = unbounded();
    let s = Sentinel::start_with(&home.0, vec![], tx, fast_cfg()).unwrap();
    let again = wait_for("the old run again", WAIT, || s.state().runs.into_iter().find(|r| r.status == RunStatus::Finished));
    assert_eq!(again.since_ms, r.since_ms);
    s.shutdown();
}

#[test]
fn tails_respect_the_per_tick_byte_cap() {
    let root = TempDir::new("cap");
    let home = TempDir::new("cap-home");
    let night1 = fixture(&root.0, false);
    let (tx, _rx) = unbounded();
    let cfg = SentinelConfig { max_read_per_tick: 128, ..fast_cfg() };
    let s = Sentinel::start_with(&home.0, vec![], tx, cfg).unwrap();
    s.add_root(&root.0).unwrap();
    // ~1.5 KiB appended at once: read 128 bytes per tick, the OOM at the end still arrives.
    let mut text: String = (0..30).map(|i| format!("[06:{:02}:00] wnr step {i} held-out KL 0.16{i:02}\n", i % 60)).collect();
    text.push_str("[06:30:00] torch.OutOfMemoryError: CUDA out of memory\n");
    let started = Instant::now();
    append(&night1.join("stage_27b.log"), &text);
    let oom = wait_for("oom after capped reads", WAIT, || find_notice(&s, NoticeSource::Log, "oom"));
    assert!(oom.title.ends_with("at 06:30:00"));
    assert!(started.elapsed() >= Duration::from_millis(1500), "the cap spread the read over several ticks");
    s.shutdown();
}

/// Every file and directory under `dir`: kind, length, modified time and content hash.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, (bool, u64, SystemTime, u64)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            // fs::metadata reads the entry itself; DirEntry metadata on Windows comes from the
            // parent's directory index, whose timestamps NTFS updates lazily.
            let m = std::fs::metadata(&p).unwrap();
            let rel = p.strip_prefix(dir).unwrap().to_path_buf();
            if m.is_dir() {
                out.insert(rel, (true, 0, m.modified().unwrap(), 0));
                stack.push(p);
            } else {
                let mut h = DefaultHasher::new();
                std::fs::read(&p).unwrap().hash(&mut h);
                out.insert(rel, (false, m.len(), m.modified().unwrap(), h.finish()));
            }
        }
    }
    out
}

#[test]
fn never_writes_inside_a_watched_root() {
    let has_git = git_available();
    let root = TempDir::new("readonly");
    let home = TempDir::new("readonly-home");
    fixture(&root.0, has_git);
    // A JSONL and trainer state present at baseline too, so every reader runs.
    write(&root.0.join("runs").join("night1").join("metrics.jsonl"), "{\"step\": 1, \"loss\": 2.0}\n");
    write(&root.0.join("out").join("trainer_state.json"), r#"{"global_step": 5, "max_steps": 5, "log_history": [{"step": 5, "loss": 1.0}]}"#);
    std::thread::sleep(Duration::from_millis(100));
    let before = snapshot(&root.0);

    let (tx, _rx) = unbounded();
    let s = Sentinel::start_with(&home.0, vec![root.0.clone()], tx, fast_cfg()).unwrap();
    wait_for("runs", WAIT, || (s.state().runs.len() >= 2).then_some(()));
    if has_git {
        wait_for("git info", WAIT, || s.state().roots[0].git.clone());
        assert!(s.git_context(&root.0).is_some());
    }
    // Several poll ticks, including full rescans and git probes.
    std::thread::sleep(Duration::from_millis(2500));
    s.shutdown();

    let after = snapshot(&root.0);
    assert_eq!(before.keys().collect::<Vec<_>>(), after.keys().collect::<Vec<_>>(), "no file created or removed inside the root");
    for (p, b) in &before {
        assert_eq!(b, &after[p], "{} changed", p.display());
    }
    // State lives in the home directory only.
    assert!(home.0.join("cowork.db").exists());
    // A root passed to start() is watched for this process only.
    let (tx, _rx) = unbounded();
    let again = Sentinel::start_with(&home.0, vec![], tx, fast_cfg()).unwrap();
    assert!(again.state().roots.is_empty());
    again.shutdown();
}
