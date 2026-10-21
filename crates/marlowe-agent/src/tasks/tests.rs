use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use super::*;

fn sid(s: &str) -> SessionId {
    SessionId(s.to_string())
}

fn temp_file(tag: &str, content: &[u8]) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("marlowe-tasks-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("out.log");
    std::fs::write(&path, content).unwrap();
    path
}

struct Stopper {
    stopped: AtomicBool,
    fail: bool,
}

impl TaskControl for Stopper {
    fn stop(&self) -> Result<(), String> {
        if self.fail {
            return Err("the process would not die".into());
        }
        self.stopped.store(true, Ordering::SeqCst);
        Ok(())
    }
}

fn stopper() -> Arc<Stopper> {
    Arc::new(Stopper { stopped: AtomicBool::new(false), fail: false })
}

#[test]
fn ids_carry_their_kind() {
    let reg = MemoryTaskRegistry::new();
    let auto = reg.register(NewTask::new(TaskKind::Monitor, "tail the log"));
    assert!(auto.starts_with("mon_"), "{auto}");
    let owned = reg.register(NewTask::new(TaskKind::Job, "cargo test").raw_id("j7"));
    assert_eq!(owned, "job_j7");
    assert_eq!(TaskKind::split(&owned), Some((TaskKind::Job, "j7")));
    assert_eq!(TaskKind::split("wf_1a2b"), Some((TaskKind::Workflow, "1a2b")));
    assert_eq!(TaskKind::split("cron_"), None, "a prefix alone names nothing");
    assert_eq!(TaskKind::split("nonsense"), None);
    for k in TaskKind::ALL {
        assert_eq!(TaskKind::split(&k.id("x")), Some((k, "x")));
    }
    let a = reg.register(NewTask::new(TaskKind::Cron, "a"));
    let b = reg.register(NewTask::new(TaskKind::Cron, "b"));
    assert_ne!(a, b, "allocated ids are unique");
}

#[test]
fn registering_an_id_again_re_arms_it() {
    let reg = MemoryTaskRegistry::new();
    let id = reg.register(NewTask::new(TaskKind::Monitor, "first").raw_id("ci"));
    reg.set_status(&id, TaskStatus::Done, Some("timed out".into()));
    let again = reg.register(NewTask::new(TaskKind::Monitor, "second").raw_id("ci"));
    assert_eq!(id, again);
    let t = reg.get(&id).unwrap();
    assert_eq!((t.title.as_str(), t.status, t.detail.as_deref(), t.ended_ms), ("second", TaskStatus::Running, None, None));
    assert_eq!(reg.list(None).len(), 1);
}

#[test]
fn a_session_sees_its_own_and_daemon_wide_tasks_newest_first() {
    let reg = MemoryTaskRegistry::new();
    reg.register(NewTask::new(TaskKind::Job, "mine").session(Some(sid("a"))));
    std::thread::sleep(std::time::Duration::from_millis(3));
    reg.register(NewTask::new(TaskKind::Job, "theirs").session(Some(sid("b"))));
    std::thread::sleep(std::time::Duration::from_millis(3));
    reg.register(NewTask::new(TaskKind::Cron, "shared"));
    let titles = |l: Vec<TaskInfo>| l.into_iter().map(|t| t.title).collect::<Vec<_>>();
    assert_eq!(titles(reg.list(Some(&sid("a")))), ["shared", "mine"]);
    assert_eq!(titles(reg.list(None)), ["shared", "theirs", "mine"]);
}

struct FixedSource {
    tasks: Vec<TaskInfo>,
    stopped: Mutex<Vec<String>>,
}

impl TaskSource for FixedSource {
    fn list(&self) -> Vec<TaskInfo> {
        self.tasks.clone()
    }
    fn stop(&self, id: &str) -> Option<Result<(), String>> {
        self.tasks.iter().any(|t| t.id == id).then(|| {
            self.stopped.lock().unwrap().push(id.to_string());
            Ok(())
        })
    }
}

fn info(id: &str, kind: TaskKind, started: i64) -> TaskInfo {
    TaskInfo {
        id: id.into(),
        kind,
        session: Some(sid("a")),
        title: id.into(),
        status: TaskStatus::Running,
        started_ms: started,
        ended_ms: None,
        output_file: None,
        detail: None,
    }
}

#[test]
fn sources_are_listed_merged_and_stopped_without_mirroring() {
    let reg = MemoryTaskRegistry::new();
    let src = Arc::new(FixedSource {
        tasks: vec![info("job_j1", TaskKind::Job, 5), info("agent_3", TaskKind::Agent, 9)],
        stopped: Mutex::new(Vec::new()),
    });
    reg.add_source(src.clone());
    reg.register(NewTask::new(TaskKind::Monitor, "m").raw_id("1"));
    let ids: Vec<String> = reg.list(None).into_iter().map(|t| t.id).collect();
    for want in ["job_j1", "agent_3", "mon_1"] {
        assert!(ids.contains(&want.to_string()), "{ids:?}");
    }
    assert_eq!(reg.get("agent_3").unwrap().kind, TaskKind::Agent);

    let stopped = reg.stop("job_j1").unwrap();
    assert_eq!(stopped.status, TaskStatus::Stopped);
    assert_eq!(*src.stopped.lock().unwrap(), ["job_j1"]);
    assert!(reg.stop("job_nope").unwrap_err().contains("no task"));
}

#[test]
fn stop_runs_the_control_and_a_finished_task_stays_finished() {
    let reg = MemoryTaskRegistry::new();
    let ctl = stopper();
    let id = reg.register(NewTask::new(TaskKind::Workflow, "review").control(ctl.clone()));
    let t = reg.stop(&id).unwrap();
    assert_eq!(t.status, TaskStatus::Stopped);
    assert!(t.ended_ms.is_some());
    assert!(ctl.stopped.load(Ordering::SeqCst));
    // A late "done" from the thread that was winding down cannot resurrect or rewrite it.
    reg.set_status(&id, TaskStatus::Done, Some("finished".into()));
    assert_eq!(reg.get(&id).unwrap().status, TaskStatus::Stopped);
    assert!(reg.stop(&id).unwrap_err().contains("already finished"));
}

#[test]
fn stop_reports_why_it_could_not() {
    let reg = MemoryTaskRegistry::new();
    let stuck = reg.register(
        NewTask::new(TaskKind::Job, "stuck").control(Arc::new(Stopper { stopped: AtomicBool::new(false), fail: true })),
    );
    assert_eq!(reg.stop(&stuck).unwrap_err(), "the process would not die");
    assert_eq!(reg.get(&stuck).unwrap().status, TaskStatus::Running, "a failed stop changes nothing");
    let plain = reg.register(NewTask::new(TaskKind::Cron, "no handle"));
    assert!(reg.stop(&plain).unwrap_err().contains("cannot be stopped"));
    assert!(reg.stop("wf_missing").unwrap_err().contains("task_list"));
}

#[test]
fn listeners_hear_every_change_and_may_call_back_in() {
    let reg = Arc::new(MemoryTaskRegistry::new());
    let seen = Arc::new(Mutex::new(Vec::<(String, TaskStatus, String)>::new()));
    {
        let (seen, reg2) = (seen.clone(), reg.clone());
        reg.subscribe(Arc::new(move |t: &TaskInfo| {
            // Calling the registry from a listener must not deadlock.
            let _ = reg2.list(None);
            seen.lock().unwrap().push((t.id.clone(), t.status, t.title.clone()));
        }));
    }
    let id = reg.register(NewTask::new(TaskKind::Cron, "nightly").raw_id("n"));
    reg.set_title(&id, "nightly, next 03:00");
    reg.set_title(&id, "nightly, next 03:00");
    reg.set_status(&id, TaskStatus::Waiting, None);
    reg.set_status(&id, TaskStatus::Failed, Some("bad cron".into()));
    reg.set_status("cron_unknown", TaskStatus::Done, None);
    let seen = seen.lock().unwrap();
    let statuses: Vec<TaskStatus> = seen.iter().map(|s| s.1).collect();
    assert_eq!(
        statuses,
        [TaskStatus::Running, TaskStatus::Running, TaskStatus::Waiting, TaskStatus::Failed],
        "an unchanged title and an unknown id are silent"
    );
    assert_eq!(seen[1].2, "nightly, next 03:00");
}

#[test]
fn output_continues_by_offset_and_waits_for_a_line_to_end() {
    let path = temp_file("cont", b"one\ntwo\nthr");
    let reg = MemoryTaskRegistry::new();
    let id = reg.register(NewTask::new(TaskKind::Job, "x").output_file(path.display().to_string()));
    let a = reg.output(&id, None, None).unwrap();
    assert_eq!((a.text.as_str(), a.next_since, a.done), ("one\ntwo\n", 8, false), "the half-written line waits");
    std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"ee\nfour\n").unwrap();
    let b = reg.output(&id, Some(a.next_since), None).unwrap();
    assert_eq!((b.text.as_str(), b.done), ("three\nfour\n", false));
    let again = reg.output(&id, Some(b.next_since), None).unwrap();
    assert_eq!((again.text.as_str(), again.next_since, again.done), ("", b.next_since, false), "nothing new while it runs");
    reg.set_status(&id, TaskStatus::Done, None);
    let end = reg.output(&id, Some(b.next_since), None).unwrap();
    assert!(end.done, "finished and fully read");
    let whole = reg.output(&id, None, None).unwrap();
    assert_eq!(whole.text, "one\ntwo\nthree\nfour\n");
}

#[test]
fn a_finished_task_yields_its_unterminated_last_line() {
    let path = temp_file("tail", b"build ok\nexit 0");
    let reg = MemoryTaskRegistry::new();
    let id = reg.register(NewTask::new(TaskKind::Job, "x").output_file(path.display().to_string()));
    reg.set_status(&id, TaskStatus::Done, None);
    let c = reg.output(&id, None, None).unwrap();
    assert_eq!((c.text.as_str(), c.done), ("build ok\nexit 0", true));
}

#[test]
fn filter_keeps_matching_lines_and_still_advances() {
    let path = temp_file("filter", b"warn: a\ninfo: b\nwarn: c\n");
    let reg = MemoryTaskRegistry::new();
    let id = reg.register(NewTask::new(TaskKind::Job, "x").output_file(path.display().to_string()));
    let c = reg.output(&id, None, Some("^warn")).unwrap();
    assert_eq!((c.text.as_str(), c.next_since), ("warn: a\nwarn: c\n", 24));
    assert!(reg.output(&id, None, Some("(")).unwrap_err().contains("regular expression"));
}

#[test]
fn a_large_file_is_read_in_chunks_that_reassemble_exactly() {
    // Multi-byte text with no newline, bigger than one chunk: no chunk may end inside a character.
    let text = format!("a{}", "é".repeat(MAX_OUTPUT_CHUNK));
    let path = temp_file("big", text.as_bytes());
    let reg = MemoryTaskRegistry::new();
    let id = reg.register(NewTask::new(TaskKind::Job, "x").output_file(path.display().to_string()));
    reg.set_status(&id, TaskStatus::Done, None);
    let (mut got, mut since, mut chunks) = (String::new(), 0u64, 0);
    loop {
        let c = reg.output(&id, Some(since), None).unwrap();
        assert!(!c.text.contains('\u{fffd}'), "a chunk split a character");
        got.push_str(&c.text);
        since = c.next_since;
        chunks += 1;
        if c.done {
            break;
        }
        assert!(chunks < 10, "no progress");
    }
    assert_eq!(got, text);
    assert!(chunks >= 2);
}

#[test]
fn output_comes_from_the_control_before_the_file() {
    struct Live;
    impl TaskControl for Live {
        fn stop(&self) -> Result<(), String> {
            Ok(())
        }
        fn output(&self, since: Option<u64>, _f: Option<&str>) -> Option<Result<TaskChunk, String>> {
            Some(Ok(TaskChunk { text: format!("from control since {}", since.unwrap_or(0)), next_since: 9, done: false }))
        }
    }
    let reg = MemoryTaskRegistry::new();
    let id = reg.register(NewTask::new(TaskKind::Workflow, "w").control(Arc::new(Live)));
    assert_eq!(reg.output(&id, Some(3), None).unwrap().text, "from control since 3");
    let bare = reg.register(NewTask::new(TaskKind::Monitor, "m"));
    assert!(reg.output(&bare, None, None).unwrap_err().contains("no output file"));
    assert!(reg.output("job_none", None, None).unwrap_err().contains("no task"));
}

#[test]
fn the_waker_runs_for_each_injection_after_the_lock_is_released() {
    let reg = Arc::new(MemoryTaskRegistry::new());
    let woke = Arc::new(AtomicUsize::new(0));
    {
        let (woke, reg2) = (woke.clone(), reg.clone());
        reg.set_waker(Arc::new(move |s: &SessionId| {
            // The waker asks the registry what is pending, as the daemon does to start a turn.
            assert!(reg2.pending_injections(s) > 0);
            woke.fetch_add(1, Ordering::SeqCst);
        }));
    }
    reg.inject(Injection::new(sid("a"), InjectionSource::Job, "job exited"));
    reg.inject(Injection::new(sid("b"), InjectionSource::Cron, "fired"));
    assert_eq!(woke.load(Ordering::SeqCst), 2);
}

#[test]
fn injections_batch_per_session_and_never_cross() {
    let reg = MemoryTaskRegistry::new();
    for i in 0..5 {
        reg.inject(Injection::new(sid("a"), InjectionSource::Monitor, format!("line {i}")));
    }
    reg.inject(Injection::new(sid("b"), InjectionSource::Message, "hello").untrusted(true).from("session a"));
    assert_eq!(reg.pending_injections(&sid("a")), 5);
    let first = reg.take_injections(&sid("a"), 3);
    assert_eq!(first.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(), ["line 0", "line 1", "line 2"]);
    let rest = reg.take_injections(&sid("a"), 50);
    assert_eq!(rest.len(), 2);
    assert!(reg.take_injections(&sid("a"), 50).is_empty());
    let msg = reg.take_injections(&sid("b"), 50);
    assert_eq!(msg.len(), 1);
    assert!(msg[0].untrusted, "a message from another session is never trusted");
    assert_eq!(msg[0].from.as_deref(), Some("session a"));
    assert_eq!(msg[0].source.as_str(), "message");
}

#[test]
fn the_queue_is_bounded_and_clears_with_its_session() {
    let reg = MemoryTaskRegistry::new();
    for i in 0..(MAX_PENDING_INJECTIONS + 10) {
        reg.inject(Injection::new(sid("a"), InjectionSource::Cron, format!("fire {i}")));
    }
    assert_eq!(reg.pending_injections(&sid("a")), MAX_PENDING_INJECTIONS);
    assert_eq!(reg.take_injections(&sid("a"), 1)[0].text, "fire 10", "the oldest were dropped");
    reg.clear_session(&sid("a"));
    assert_eq!(reg.pending_injections(&sid("a")), 0);
}

#[test]
fn only_the_newest_finished_tasks_are_kept() {
    let reg = MemoryTaskRegistry::new();
    let live = reg.register(NewTask::new(TaskKind::Monitor, "still armed"));
    for i in 0..(KEEP_FINISHED + 20) {
        let id = reg.register(NewTask::new(TaskKind::Job, format!("j{i}")));
        reg.set_status(&id, TaskStatus::Done, None);
    }
    reg.register(NewTask::new(TaskKind::Job, "trigger a prune"));
    let all = reg.list(None);
    assert!(all.iter().any(|t| t.id == live), "a running task is never pruned");
    assert!(all.iter().filter(|t| t.status.is_terminal()).count() <= KEEP_FINISHED);
}

#[test]
fn the_registry_is_a_session_service() {
    let svc = crate::Services::new().with(Tasks(Arc::new(MemoryTaskRegistry::new())));
    let tasks = svc.get::<Tasks>().expect("installed");
    let id = tasks.0.register(NewTask::new(TaskKind::Job, "x"));
    assert!(tasks.0.get(&id).is_some());
}

#[test]
fn resolve_id_maps_bare_short_and_wrong_prefix_ids() {
    let reg = MemoryTaskRegistry::new();
    let agent = reg.register(NewTask::new(TaskKind::Agent, "researcher").raw_id("a990e8f6f05e494c9827b944f6364dfa"));
    let wf = reg.register(NewTask::new(TaskKind::Workflow, "fan out").raw_id("0b12c3d4e5f6478899aabbccddeeff00"));
    let job = reg.register(NewTask::new(TaskKind::Job, "cargo test").raw_id("j7"));
    assert_eq!(resolve_id(&reg, &agent).unwrap(), agent, "an exact id wins");
    assert_eq!(resolve_id(&reg, "a990e8f6f05e494c9827b944f6364dfa").unwrap(), agent, "a bare full id");
    assert_eq!(resolve_id(&reg, "a990e8f6").unwrap(), agent, "a unique 8-char prefix");
    assert_eq!(resolve_id(&reg, "agent_a990e8").unwrap(), agent, "a short id under its prefix");
    assert_eq!(resolve_id(&reg, "agent_0b12c3d4e5f6478899aabbccddeeff00").unwrap(), wf, "a workflow sent as agent_");
    assert_eq!(resolve_id(&reg, "run a990e8f6").unwrap(), agent);
    assert_eq!(resolve_id(&reg, "`wf_0b12c3d4`").unwrap(), wf, "quotes are dropped");
    assert_eq!(resolve_id(&reg, "j7").unwrap(), job, "a short raw id matches only when equal");
    assert!(resolve_id(&reg, "a99").unwrap_err().contains("no task"), "under 6 characters is no prefix");
}

#[test]
fn resolve_id_lists_ambiguous_and_nearest() {
    let reg = MemoryTaskRegistry::new();
    let a = reg.register(NewTask::new(TaskKind::Agent, "one").raw_id("abc123000000"));
    let b = reg.register(NewTask::new(TaskKind::Workflow, "two").raw_id("abc123ffffff"));
    let e = resolve_id(&reg, "abc123").unwrap_err();
    assert!(e.contains(&a) && e.contains(&b) && e.contains("matches 2"), "{e}");
    let e = resolve_id(&reg, "agent_zzzzzz").unwrap_err();
    assert!(e.contains("no task") && e.contains(&a) && e.contains("nearest"), "{e}");
    let empty = MemoryTaskRegistry::new();
    assert!(resolve_id(&empty, "x1").unwrap_err().contains("task_list"));
}

#[test]
fn resolve_id_finds_a_subagent_by_its_own_id() {
    let reg = MemoryTaskRegistry::new();
    let id = reg.register(NewTask::new(TaskKind::Agent, "reader").raw_id("s-1.agent-3"));
    assert_eq!(resolve_id(&reg, "agent-3").unwrap(), id);
}
