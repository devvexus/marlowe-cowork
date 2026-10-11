//! Track N, at the tool level: BG1-BG11 against the real tool table, the real task registry and
//! real processes (monitors run bash), with the daemon's side faked (`FakeBackground`). The
//! daemon's own behaviour (the waker, schedules on a fake clock, the toast, the outbox, the close)
//! is `marlowe-daemon/tests/parity_n.rs`.

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use marlowe_agent::background::FakeBackground;
use marlowe_agent::todo::{QuestionAsker, Questions};
use marlowe_agent::{
    Background, CallCtx, Consequence, Exposure, Injection, MemoryTaskRegistry, NewTask, Services, SessionScope, TaskControl,
    TaskKind, TaskRegistry, TaskStatus, Tasks, ToolCallReq, ToolHost, ToolOutcome,
};
use marlowe_exec::specs::{self, HostSetup};
use marlowe_exec::BuiltinTools;
use marlowe_model::CancelToken;
use marlowe_proto::{Notice, NoticeSource, Question, SessionId, Severity};
use serde_json::{json, Value};

struct Rig {
    ws: Ws,
    tools: BuiltinTools,
    tasks: Arc<MemoryTaskRegistry>,
    bg: Arc<FakeBackground>,
    questions: Option<Questions>,
}

fn rig(name: &str) -> Rig {
    let ws = Ws::new(name);
    let home = ws.path("home");
    std::fs::create_dir_all(&home).unwrap();
    let tools = specs::configure(ws.tools(), &HostSetup { home: &home, workspace: Some(&ws.root) });
    Rig { ws, tools, tasks: Arc::new(MemoryTaskRegistry::new()), bg: Arc::new(FakeBackground::default()), questions: None }
}

fn session() -> SessionId {
    SessionId::from("test")
}

impl Rig {
    fn cx(&self) -> CallCtx {
        let mut services = Services::new().with(Tasks(self.tasks.clone())).with(Background(self.bg.clone()));
        if let Some(q) = &self.questions {
            services = services.with(q.clone());
        }
        CallCtx::new(ctx(Some(&self.ws.root)), SessionScope { ext: Default::default(), services })
    }

    fn call(&self, name: &str, args: Value) -> ToolOutcome {
        self.tools.call_scoped(&req(name, args), &self.cx())
    }

    fn consequence(&self, name: &str, args: Value) -> Consequence {
        let call: ToolCallReq = req(name, args);
        self.tools.consequence_exposed(&call, &Exposure::default())
    }

    fn pending(&self) -> usize {
        self.tasks.pending_injections(&session())
    }

    /// Waits until `n` injections are queued (taking none), within `secs`.
    fn wait_pending(&self, n: usize, secs: u64) {
        let end = Instant::now() + Duration::from_secs(secs);
        while self.pending() < n && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn take(&self) -> Vec<Injection> {
        self.tasks.take_injections(&session(), 10_000)
    }
}

/// A control that records its stops.
#[derive(Default)]
struct Ctl {
    stops: Mutex<u32>,
}

impl TaskControl for Ctl {
    fn stop(&self) -> Result<(), String> {
        *self.stops.lock().unwrap() += 1;
        Ok(())
    }
}

fn posix(p: &std::path::Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

// ---------------------------------------------------------------- BG1

#[test]
fn parity_bg1_one_task_of_each_kind_in_the_tool_output() {
    let r = rig("bg1");
    let ctl = Arc::new(Ctl::default());
    for (kind, raw, title) in [
        (TaskKind::Job, "j3", "cargo test"),
        (TaskKind::Agent, "r-17", "explore: find the retry code"),
        (TaskKind::Workflow, "r-18", "code-review"),
        (TaskKind::Monitor, "m1", "tail the training log"),
        (TaskKind::Cron, "5", "check CI"),
    ] {
        r.tasks.register(NewTask::new(kind, title).raw_id(raw).session(Some(session())).control(ctl.clone()));
    }
    // A finished task, and another session's.
    r.tasks.register(NewTask::new(TaskKind::Job, "old build").raw_id("j1").session(Some(session())));
    r.tasks.set_status("job_j1", TaskStatus::Done, Some("exit 0".into()));
    r.tasks.register(NewTask::new(TaskKind::Job, "someone else's").raw_id("j9").session(Some(SessionId::from("other"))));

    let o = r.call("task_list", json!({}));
    assert!(!o.is_error, "{}", o.content);
    for want in ["job_j3", "agent_r-17", "wf_r-18", "mon_m1", "cron_5", "job_j1"] {
        assert!(o.content.contains(want), "{want} missing:\n{}", o.content);
    }
    for kind in ["job", "agent", "workflow", "monitor", "cron"] {
        assert!(o.content.contains(kind), "kind {kind} missing:\n{}", o.content);
    }
    assert!(o.content.contains("6 tasks (5 active)"), "{}", o.content);
    assert!(!o.content.contains("job_j9"), "another session's task is hidden unless asked for:\n{}", o.content);
    assert!(o.content.contains("[exit 0]"), "the finished job says how it ended:\n{}", o.content);

    let all = r.call("task_list", json!({"all": true}));
    assert!(all.content.contains("job_j9") && all.content.contains("(session other)"), "{}", all.content);
    let running = r.call("task_list", json!({"running": true}));
    assert!(!running.content.contains("job_j1"), "{}", running.content);
}

#[test]
fn parity_bg1_the_job_tools_stay_as_aliases() {
    // B's job_output / job_watch / job_kill keep working beside the registry.
    let names: Vec<String> = rig("bg1-alias").tools.specs().into_iter().map(|s| s.name).collect();
    for t in ["job_output", "job_watch", "job_kill", "task_list", "task_output", "task_stop", "monitor"] {
        assert!(names.contains(&t.to_string()), "{t} missing from {names:?}");
    }
}

// ---------------------------------------------------------------- BG2

struct Answer(&'static str);

impl QuestionAsker for Answer {
    fn ask(&self, _s: &SessionId, _q: &[Question], _c: &CancelToken) -> Result<Vec<String>, String> {
        Ok(vec![self.0.to_string()])
    }
}

#[test]
fn parity_bg2_each_kind_is_stopped_through_its_control() {
    let r = rig("bg2");
    let ctls: Vec<(String, Arc<Ctl>)> = [
        (TaskKind::Job, "j1"),
        (TaskKind::Agent, "a1"),
        (TaskKind::Workflow, "w1"),
        (TaskKind::Monitor, "m1"),
        (TaskKind::Cron, "c1"),
    ]
    .into_iter()
    .map(|(kind, raw)| {
        let ctl = Arc::new(Ctl::default());
        let id = r.tasks.register(NewTask::new(kind, "x").raw_id(raw).session(Some(session())).control(ctl.clone()));
        (id, ctl)
    })
    .collect();
    for (id, ctl) in &ctls {
        let o = r.call("task_stop", json!({"id": id}));
        assert!(!o.is_error, "{id}: {}", o.content);
        assert!(o.content.contains("stopped"), "{}", o.content);
        assert_eq!(*ctl.stops.lock().unwrap(), 1, "{id} was stopped once");
        assert_eq!(r.tasks.get(id).unwrap().status, TaskStatus::Stopped);
    }
    // Stopping again, and stopping what does not exist, are errors the model can read.
    let again = r.call("task_stop", json!({"id": "job_j1"}));
    assert!(again.is_error && again.content.contains("already finished"), "{}", again.content);
    let missing = r.call("task_stop", json!({"id": "job_nope"}));
    assert!(missing.is_error && missing.content.contains("task_list"), "{}", missing.content);
    // A task with no way to stop it says so.
    r.tasks.register(NewTask::new(TaskKind::Job, "x").raw_id("fixed").session(Some(session())));
    let fixed = r.call("task_stop", json!({"id": "job_fixed"}));
    assert!(fixed.is_error && fixed.content.contains("cannot be stopped"), "{}", fixed.content);
}

#[test]
fn parity_bg2_another_sessions_task_asks_the_user() {
    let mut r = rig("bg2-ask");
    let ctl = Arc::new(Ctl::default());
    r.tasks.register(NewTask::new(TaskKind::Job, "theirs").raw_id("j7").session(Some(SessionId::from("other"))).control(ctl.clone()));
    // No one to ask: refused.
    let o = r.call("task_stop", json!({"id": "job_j7"}));
    assert!(o.is_error && o.content.contains("other"), "{}", o.content);
    assert_eq!(*ctl.stops.lock().unwrap(), 0);
    // The user says leave it.
    r.questions = Some(Questions(Arc::new(Answer("Leave it running"))));
    let o = r.call("task_stop", json!({"id": "job_j7"}));
    assert!(o.is_error && o.content.contains("left running"), "{}", o.content);
    assert_eq!(*ctl.stops.lock().unwrap(), 0);
    // The user says stop it.
    r.questions = Some(Questions(Arc::new(Answer("Stop it"))));
    let o = r.call("task_stop", json!({"id": "job_j7"}));
    assert!(!o.is_error, "{}", o.content);
    assert_eq!(*ctl.stops.lock().unwrap(), 1);
}

#[test]
fn parity_bg2_stopping_a_monitor_kills_its_whole_process_tree() {
    let r = rig("bg2-tree");
    let marker = r.ws.path("grandchild.txt");
    // The grandchild would write the marker after 3 s; the middle shell is gone at once.
    let cmd = format!("( (sleep 3; echo up > '{}') & ); sleep 30", posix(&marker));
    let o = r.call("monitor", json!({"command": cmd, "id": "tree"}));
    assert!(!o.is_error, "{}", o.content);
    std::thread::sleep(Duration::from_millis(500));
    let stopped = r.call("task_stop", json!({"id": "mon_tree"}));
    assert!(!stopped.is_error, "{}", stopped.content);
    std::thread::sleep(Duration::from_millis(4_500));
    assert!(!marker.exists(), "the grandchild outlived the stopped monitor");
    // A stop is quiet: the model asked for it, so no event follows.
    assert_eq!(r.pending(), 0, "{:?}", r.take());
}

// ---------------------------------------------------------------- BG3 and BG4

/// The text of every injection taken so far, joined.
fn texts(items: &[Injection]) -> String {
    items.iter().map(|i| i.text.clone()).collect::<Vec<_>>().join("\n---\n")
}

fn wait_status(r: &Rig, id: &str, want: TaskStatus, secs: u64) {
    let end = Instant::now() + Duration::from_secs(secs);
    while r.tasks.get(id).map(|t| t.status) != Some(want) && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(r.tasks.get(id).map(|t| t.status), Some(want), "{id}");
}

#[test]
fn parity_bg3_output_is_readable_while_the_monitor_runs() {
    let r = rig("bg3");
    let o = r.call("monitor", json!({"command": "echo first; echo second; sleep 2; echo third", "id": "log"}));
    assert!(!o.is_error, "{}", o.content);
    let file = r.tasks.get("mon_log").unwrap().output_file.expect("a monitor has an output file");
    assert!(o.content.contains(&file), "the result names the file: {}", o.content);
    // Mid-run: the lines so far are in the file and in task_output.
    r.wait_pending(1, 10);
    let mid = std::fs::read_to_string(&file).unwrap();
    assert!(mid.contains("first") && mid.contains("second") && !mid.contains("third"), "{mid:?}");
    let out = r.call("task_output", json!({"id": "mon_log"}));
    assert!(out.content.contains("first") && out.content.contains("second") && !out.content.contains("third"), "{}", out.content);
    assert!(out.content.contains("still running"), "{}", out.content);
    let next: u64 = out.content.rsplit("next since ").next().unwrap().split(';').next().unwrap().parse().unwrap();
    assert_eq!(next as usize, mid.len(), "the offset is where the file ends");
    // The rest, from the offset; and a filter.
    wait_status(&r, "mon_log", TaskStatus::Done, 15);
    let rest = r.call("task_output", json!({"id": "mon_log", "since": next}));
    assert!(rest.content.contains("third") && !rest.content.contains("first"), "{}", rest.content);
    assert!(rest.content.contains("finished"), "{}", rest.content);
    let only = r.call("task_output", json!({"id": "mon_log", "filter": "^s"}));
    assert!(only.content.starts_with("second"), "{}", only.content);
    let bad = r.call("task_output", json!({"id": "mon_log", "filter": "("}));
    assert!(bad.is_error && bad.content.contains("regular expression"), "{}", bad.content);
    let unknown = r.call("task_output", json!({"id": "mon_nope"}));
    assert!(unknown.is_error, "{}", unknown.content);
}

#[test]
fn parity_bg4_three_lines_a_second_apart_are_three_events_in_order() {
    let r = rig("bg4-lines");
    let o = r.call("monitor", json!({"command": "for i in 1 2 3; do echo line$i; sleep 1; done", "description": "count"}));
    assert!(!o.is_error, "{}", o.content);
    let id = r.tasks.list(Some(&session())).into_iter().find(|t| t.kind == TaskKind::Monitor).unwrap().id;
    // The model "works" meanwhile: events appear one by one, and nothing was waited for.
    let mut seen: Vec<Injection> = Vec::new();
    let end = Instant::now() + Duration::from_secs(20);
    while !seen.iter().any(|i| i.text.contains("ended")) && Instant::now() < end {
        seen.extend(r.take());
        std::thread::sleep(Duration::from_millis(50));
    }
    let lines: Vec<&Injection> = seen.iter().filter(|i| i.text.contains("· lines ")).collect();
    assert_eq!(lines.len(), 3, "one event per line, a second apart: {}", texts(&seen));
    for (n, i) in lines.iter().enumerate() {
        assert!(i.text.contains(&format!("line{}", n + 1)), "{}", i.text);
        assert_eq!(i.source.as_str(), "monitor");
        assert_eq!(i.task.as_deref(), Some(id.as_str()));
        assert!(!i.untrusted);
    }
    let last = seen.last().unwrap();
    assert!(last.text.contains("exited with code 0") && last.text.contains("3 lines in all"), "{}", last.text);
    assert_eq!(r.tasks.get(&id).unwrap().status, TaskStatus::Done);
}

#[test]
fn parity_bg4_a_monitor_times_out_and_says_so() {
    let r = rig("bg4-timeout");
    let o = r.call("monitor", json!({"command": "echo up; sleep 30", "timeout_ms": 1000, "id": "t"}));
    assert!(!o.is_error, "{}", o.content);
    wait_status(&r, "mon_t", TaskStatus::Stopped, 15);
    let ev = texts(&r.take());
    assert!(ev.contains("up"), "{ev}");
    assert!(ev.contains("timed out after 1 s"), "{ev}");
    assert!(ev.contains("monitor {\"id\":\"t\"}"), "the event says how to re-arm: {ev}");
    assert!(r.tasks.get("mon_t").unwrap().detail.unwrap().contains("timed out"));
}

#[test]
fn parity_bg4_a_persistent_monitor_outlives_its_would_be_timeout() {
    let r = rig("bg4-persist");
    let o = r.call("monitor", json!({"command": "echo armed; sleep 30", "timeout_ms": 1000, "persistent": true, "id": "p"}));
    assert!(!o.is_error && o.content.contains("stays armed"), "{}", o.content);
    std::thread::sleep(Duration::from_millis(2_500));
    assert_eq!(r.tasks.get("mon_p").unwrap().status, TaskStatus::Running, "still armed after the timeout it ignores");
    assert!(!texts(&r.take()).contains("timed out"));
    let stopped = r.call("task_stop", json!({"id": "mon_p"}));
    assert!(!stopped.is_error, "{}", stopped.content);
}

#[test]
fn parity_bg4_a_ten_thousand_line_flood_is_batched_and_the_file_is_complete() {
    let r = rig("bg4-flood");
    let o = r.call("monitor", json!({"command": "seq 1 10000", "id": "flood"}));
    assert!(!o.is_error, "{}", o.content);
    wait_status(&r, "mon_flood", TaskStatus::Done, 30);
    let events = r.take();
    assert!(!events.is_empty());
    for e in &events {
        let shown = e.text.lines().filter(|l| l.trim().parse::<u32>().is_ok()).count();
        assert!(shown <= 50, "an event carries at most 50 lines, this one {shown}: {}", e.text.chars().take(200).collect::<String>());
    }
    assert!(events.len() < 400, "batched, not one event per line: {}", events.len());
    let batches = events.iter().filter(|e| e.text.contains("more lines not shown")).count();
    assert!(batches > 0, "a batch that is cut says where the rest is");
    let last = events.last().unwrap();
    assert!(last.text.contains("10000 lines in all"), "{}", last.text);
    // The file has every line, whatever the events showed.
    let file = r.tasks.get("mon_flood").unwrap().output_file.unwrap();
    let text = std::fs::read_to_string(file).unwrap();
    assert_eq!(text.lines().count(), 10_000);
    assert_eq!(text.lines().last(), Some("10000"));
}

#[test]
fn parity_bg4_a_hundred_fast_monitors_lose_no_last_line() {
    // A command that prints and exits at once: the exit and the last lines land together, and the
    // pump must not read the log before it has seen the exit and then trust "nothing more".
    let r = rig("bg4-fast");
    let n = 100;
    for i in 0..n {
        let o = r.call("monitor", json!({"command": "echo x; echo y", "id": format!("f{i}")}));
        assert!(!o.is_error, "{}", o.content);
    }
    for i in 0..n {
        wait_status(&r, &format!("mon_f{i}"), TaskStatus::Done, 60);
    }
    let events = r.take();
    let shown: usize = events.iter().filter(|e| e.text.contains("· lines ") && e.text.contains("\nx\ny")).count();
    assert_eq!(shown, n, "every monitor delivered both lines");
    assert_eq!(events.iter().filter(|e| e.text.contains("ended")).count(), n);
    for i in 0..n {
        let file = r.tasks.get(&format!("mon_f{i}")).unwrap().output_file.unwrap();
        assert_eq!(std::fs::read_to_string(file).unwrap().lines().collect::<Vec<_>>(), ["x", "y"], "monitor {i}");
    }
}

#[test]
fn parity_bg4_the_same_id_rearms_and_numbers_on_from_the_last_line() {
    let r = rig("bg4-rearm");
    r.call("monitor", json!({"command": "echo a; echo b", "id": "w"}));
    wait_status(&r, "mon_w", TaskStatus::Done, 15);
    let first = texts(&r.take());
    assert!(first.contains("lines 1-2"), "{first}");
    // Armed again with no command: the same one runs, its lines are 3 and 4, the file keeps growing.
    let o = r.call("monitor", json!({"id": "w"}));
    assert!(!o.is_error, "{}", o.content);
    wait_status(&r, "mon_w", TaskStatus::Done, 15);
    let second = texts(&r.take());
    assert!(second.contains("lines 3-4"), "{second}");
    let file = r.tasks.get("mon_w").unwrap().output_file.unwrap();
    assert_eq!(std::fs::read_to_string(file).unwrap().lines().collect::<Vec<_>>(), ["a", "b", "a", "b"]);
    // Re-arming what is still running is refused; a monitor with nothing to run is too.
    r.call("monitor", json!({"command": "sleep 30", "id": "busy"}));
    let o = r.call("monitor", json!({"command": "echo x", "id": "busy"}));
    assert!(o.is_error && o.content.contains("still armed"), "{}", o.content);
    r.call("task_stop", json!({"id": "mon_busy"}));
    let o = r.call("monitor", json!({}));
    assert!(o.is_error && o.content.contains("`command` is required"), "{}", o.content);
    let o = r.call("monitor", json!({"command": "echo x", "id": "../x"}));
    assert!(o.is_error, "{}", o.content);
}

#[test]
fn parity_bg4_a_monitor_asks_like_bash() {
    let r = rig("bg4-ask");
    let c = r.consequence("monitor", json!({"command": "tail -f train.log"}));
    assert!(matches!(&c, Consequence::Ask { summary } if summary.contains("tail -f train.log")), "{c:?}");
    // Re-arming repeats a command that was allowed.
    assert_eq!(r.consequence("monitor", json!({"id": "w"})), Consequence::Free);
    // A host without the registry (a crew sandbox) does not offer the group at all.
    let bare = BuiltinTools::new(None);
    assert!(!bare.specs().iter().any(|s| s.name == "monitor"));
}

// ---------------------------------------------------------------- BG5 and BG6

#[test]
fn parity_bg5_cron_create_list_delete() {
    let r = rig("bg5");
    let made = r.call("cron_create", json!({"cron": "0 9 * * 1-5", "prompt": "check the nightly run"}));
    assert!(!made.is_error, "{}", made.content);
    assert!(made.content.contains("cron_1") && made.content.contains("dies with this session"), "{}", made.content);
    let once = r.call("cron_create", json!({"once_at": "2999-01-02T09:00", "prompt": "remind me"}));
    assert!(!once.is_error, "{}", once.content);
    let list = r.call("cron_list", json!({}));
    assert!(list.content.contains("cron `0 9 * * 1-5`") && list.content.contains("once"), "{}", list.content);
    assert!(list.content.contains("check the nightly run"), "{}", list.content);
    let del = r.call("cron_delete", json!({"id": "cron_1"}));
    assert!(!del.is_error, "{}", del.content);
    let list = r.call("cron_list", json!({}));
    assert!(!list.content.contains("nightly"), "{}", list.content);
    let missing = r.call("cron_delete", json!({"id": "cron_1"}));
    assert!(missing.is_error, "{}", missing.content);
}

#[test]
fn parity_bg5_a_bad_schedule_is_handed_back_in_words() {
    let r = rig("bg5-bad");
    let table: [(Value, &str); 8] = [
        (json!({"prompt": "x"}), "exactly one of `cron` and `once_at`"),
        (json!({"cron": "* * * * *", "once_at": "2999-01-01T00:00", "prompt": "x"}), "exactly one"),
        (json!({"cron": "* * * *", "prompt": "x"}), "5 fields"),
        (json!({"cron": "61 * * * *", "prompt": "x"}), "minute field"),
        (json!({"cron": "0 0 31 2 *", "prompt": "x"}), "never fires"),
        (json!({"once_at": "yesterday", "prompt": "x"}), "must be a local time"),
        (json!({"once_at": "2001-01-01T00:00", "prompt": "x"}), "in the past"),
        (json!({"cron": "* * * * *"}), "`prompt` is required"),
    ];
    for (args, want) in table {
        let o = r.call("cron_create", args.clone());
        assert!(o.is_error && o.content.contains(want), "{args}: {}", o.content);
    }
    assert!(r.bg.schedules.lock().unwrap().is_empty(), "nothing was scheduled by a bad call");
}

#[test]
fn parity_bg5_a_durable_schedule_asks_the_user() {
    let r = rig("bg5-durable");
    let durable = r.consequence("cron_create", json!({"cron": "0 3 * * *", "prompt": "eval", "durable": true}));
    assert!(matches!(&durable, Consequence::Ask { summary } if summary.contains("survives restarts") && summary.contains("0 3 * * *")), "{durable:?}");
    assert_eq!(r.consequence("cron_create", json!({"cron": "0 3 * * *", "prompt": "eval"})), Consequence::Free);
}

#[test]
fn parity_bg6_wakeup_delays_clamp_to_a_minute_and_an_hour() {
    let r = rig("bg6");
    for (asked, want) in [(5i64, 60i64), (60, 60), (900, 900), (3600, 3600), (99_999, 3600)] {
        let before = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
        let o = r.call("schedule_wakeup", json!({"delay_s": asked, "prompt": "keep watching", "reason": "training"}));
        assert!(!o.is_error, "{}", o.content);
        assert!(o.content.contains(&format!("woken in {want} s")), "{asked}: {}", o.content);
        assert_eq!(o.content.contains("clamped"), asked != want, "{asked}: {}", o.content);
        let at = r.bg.schedules.lock().unwrap().last().unwrap().1.once_at_ms.unwrap();
        assert!(at >= before + want * 1000 && at < before + want * 1000 + 5_000, "{asked}: wakes at +{} ms", at - before);
    }
    let o = r.call("schedule_wakeup", json!({"prompt": "x"}));
    assert!(o.is_error && o.content.contains("delay_s"), "{}", o.content);
}

// ---------------------------------------------------------------- BG7, BG8, BG9

#[test]
fn parity_bg7_notify_validates_and_reaches_the_daemon() {
    let r = rig("bg7");
    let o = r.call("notify", json!({"title": "Run finished", "body": "val loss 0.41", "urgency": "urgent"}));
    assert!(!o.is_error && o.content.contains("Notified"), "{}", o.content);
    let n = r.bg.notified.lock().unwrap().clone();
    assert_eq!(n.len(), 1);
    assert_eq!((n[0].0.as_str(), n[0].1.as_str(), n[0].2), ("Run finished", "val loss 0.41", marlowe_proto::Urgency::Urgent));
    let o = r.call("notify", json!({"title": "x", "urgency": "shout"}));
    assert!(o.is_error && o.content.contains("low, normal or urgent"), "{}", o.content);
    let o = r.call("notify", json!({"body": "no title"}));
    assert!(o.is_error, "{}", o.content);
    assert_eq!(r.consequence("notify", json!({"title": "t"})), Consequence::Free);
}

#[test]
fn parity_bg8_send_file_takes_only_files_inside_the_workspace() {
    let r = rig("bg8");
    r.ws.put("reports/final.html", "<h1>done</h1>");
    let ok = r.call("send_file", json!({"path": "reports/final.html", "note": "the report"}));
    assert!(!ok.is_error && ok.content.contains("final.html"), "{}", ok.content);
    let sent = r.bg.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1, "the report");
    assert!(sent[0].0.ends_with("final.html"));
    // Outside the workspace, a directory, and a missing file are refused without reaching the daemon.
    let outside = std::env::temp_dir().join(format!("marlowe-bg8-outside-{}.txt", std::process::id()));
    std::fs::write(&outside, "secret").unwrap();
    let o = r.call("send_file", json!({"path": outside.to_string_lossy()}));
    assert!(o.is_error && o.content.contains("outside the workspace"), "{}", o.content);
    let _ = std::fs::remove_file(&outside);
    let o = r.call("send_file", json!({"path": "reports"}));
    assert!(o.is_error && o.content.contains("not a file"), "{}", o.content);
    let o = r.call("send_file", json!({"path": "reports/none.html"}));
    assert!(o.is_error, "{}", o.content);
    assert_eq!(r.bg.sent.lock().unwrap().len(), 1);
}

fn notice(id: &str, source: NoticeSource, title: &str, at_ms: i64) -> Notice {
    Notice {
        id: id.into(),
        at_ms,
        source,
        severity: Severity::Attention,
        kind: "k".into(),
        title: title.into(),
        detail: "d".into(),
        subject: id.into(),
        seen: false,
    }
}

#[test]
fn parity_bg9_notices_come_from_every_source_and_are_read_once() {
    let r = rig("bg9");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    *r.bg.notices.lock().unwrap() = vec![
        notice("n1", NoticeSource::Trainer, "wnr5 finished", now - 5_000),
        notice("n2", NoticeSource::Task, "mon_w ended", now - 4_000),
        notice("n3", NoticeSource::Message, "session s2 wrote", now - 3_000),
    ];
    let first = r.call("notices", json!({"unread": true}));
    assert!(!first.is_error, "{}", first.content);
    assert_eq!(first.content.lines().count(), 3, "{}", first.content);
    for want in ["trainer/", "task/", "message/", "wnr5 finished"] {
        assert!(first.content.contains(want), "{want}: {}", first.content);
    }
    let second = r.call("notices", json!({"unread": true}));
    assert_eq!(second.content, "No unread notices.");
    // Not unread-only: still there, and filters work.
    let all = r.call("notices", json!({"source": "task"}));
    assert!(all.content.contains("mon_w ended") && !all.content.contains("wnr5"), "{}", all.content);
    let since = r.call("notices", json!({"since": (now - 3_500).to_string()}));
    assert!(since.content.contains("session s2") && !since.content.contains("wnr5"), "{}", since.content);
    let bad = r.call("notices", json!({"since": "whenever"}));
    assert!(bad.is_error, "{}", bad.content);
}

// ---------------------------------------------------------------- BG10 and BG11, the watch tool

#[test]
fn parity_bg10_list_agents_shows_a_subagent_a_run_and_a_session() {
    let r = rig("bg10");
    use marlowe_agent::background::AgentRow;
    *r.bg.agents.lock().unwrap() = vec![
        AgentRow { id: "agent-2".into(), kind: "subagent".into(), status: "running".into(), title: "explore: retry code".into(), workspace: None },
        AgentRow { id: "r-9f2c".into(), kind: "crew run".into(), status: "running".into(), title: "builder: add the cache".into(), workspace: None },
        AgentRow { id: "session:s-12".into(), kind: "session".into(), status: "idle".into(), title: "the eval harness".into(), workspace: Some("C:/work/eval".into()) },
    ];
    let o = r.call("list_agents", json!({}));
    assert!(!o.is_error, "{}", o.content);
    for want in ["agent-2", "subagent", "r-9f2c", "crew run", "session:s-12", "the eval harness", "C:/work/eval"] {
        assert!(o.content.contains(want), "{want}: {}", o.content);
    }
    r.bg.agents.lock().unwrap().clear();
    assert!(r.call("list_agents", json!({})).content.contains("No agents"));
}

#[test]
fn parity_bg11_send_message_routes_by_the_target() {
    let r = rig("bg11");
    let o = r.call("send_message", json!({"to": "session:s-12", "text": "the eval is waiting on your checkpoint"}));
    assert!(!o.is_error, "{}", o.content);
    let o = r.call("send_message", json!({"to": "r-9f2c", "text": "use the smaller dataset"}));
    assert!(!o.is_error, "{}", o.content);
    let sent = r.bg.messages.lock().unwrap().clone();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0], ("test".into(), "session:s-12".into(), "the eval is waiting on your checkpoint".into()));
    assert_eq!(sent[1].1, "r-9f2c");
    // A subagent's id goes to the subagent service, not the daemon.
    let o = r.call("send_message", json!({"agent_id": "agent-1", "text": "and its tests?"}));
    assert!(o.is_error && o.content.contains("subagents are not available"), "{}", o.content);
    assert_eq!(r.bg.messages.lock().unwrap().len(), 2);
    let o = r.call("send_message", json!({"to": "r-1"}));
    assert!(o.is_error, "{}", o.content);
}

#[test]
fn the_watch_tool_adds_lists_and_removes_a_folder() {
    let r = rig("watch");
    r.ws.put("runs/exp1/heartbeat.json", "{}");
    let dir = posix(&r.ws.path("runs"));
    let added = r.call("watch", json!({"action": "add", "path": dir}));
    assert!(!added.is_error && added.content.contains("now watching 1 folder"), "{}", added.content);
    // Inside the workspace a watch is free; elsewhere it asks.
    assert_eq!(r.consequence("watch", json!({"action": "add", "path": dir})), Consequence::Free);
    let elsewhere = r.consequence("watch", json!({"action": "add", "path": "C:/Users/someone/else"}));
    assert!(matches!(&elsewhere, Consequence::Ask { summary } if summary.contains("watch folder")), "{elsewhere:?}");
    assert_eq!(r.consequence("watch", json!({"action": "list"})), Consequence::Free);
    let listed = r.call("watch", json!({"action": "list"}));
    assert!(listed.content.contains("runs"), "{}", listed.content);
    let removed = r.call("watch", json!({"action": "remove", "path": dir}));
    assert!(removed.content.contains("no folders"), "{}", removed.content);
    let bad = r.call("watch", json!({"action": "add"}));
    assert!(bad.is_error && bad.content.contains("`path` is required"), "{}", bad.content);
    // A relative path is the workspace's: judged free by the consequence, and watched there.
    assert_eq!(r.consequence("watch", json!({"action": "add", "path": "runs/exp1"})), Consequence::Free);
    let rel = r.call("watch", json!({"action": "add", "path": "runs/exp1"}));
    assert!(!rel.is_error && rel.content.contains("exp1"), "{}", rel.content);
    let listed = r.call("watch", json!({"action": "list"}));
    let root = posix(&r.ws.root);
    assert!(listed.content.replace('\\', "/").contains(&format!("{root}/runs/exp1")), "{}", listed.content);
    let rel_gone = r.call("watch", json!({"action": "remove", "path": "runs/exp1"}));
    assert!(rel_gone.content.contains("no folders"), "{}", rel_gone.content);
    let rel_missing = r.call("watch", json!({"action": "add", "path": "runs/nothing"}));
    assert!(rel_missing.is_error && rel_missing.content.contains("not a folder"), "{}", rel_missing.content);
    let not_dir = r.call("watch", json!({"action": "add", "path": posix(&r.ws.path("nothing"))}));
    assert!(not_dir.is_error && not_dir.content.contains("not a folder"), "{}", not_dir.content);
}

#[test]
fn the_group_is_offered_only_where_the_daemon_provides_it() {
    let r = rig("group");
    let names: Vec<String> = r.tools.specs().into_iter().map(|s| s.name).collect();
    for t in [
        "task_list", "task_output", "task_stop", "monitor", "cron_create", "cron_list", "cron_delete", "schedule_wakeup",
        "list_agents", "watch", "notify", "send_file", "notices",
    ] {
        assert!(names.contains(&t.to_string()), "{t} missing");
    }
    let bare: Vec<String> = BuiltinTools::new(None).specs().into_iter().map(|s| s.name).collect();
    assert!(!bare.contains(&"task_list".to_string()) && !bare.contains(&"notify".to_string()));
    // Without the services a configured host still answers in words, never a panic.
    let o = r.tools.call(&req("task_list", json!({})), &ctx(Some(&r.ws.root)));
    assert!(o.is_error && o.content.contains("not available"), "{}", o.content);
    // The persona block rides with the group.
    assert!(r.tools.persona_blocks().iter().any(|b| b.text().contains("<background_work>")));
    assert!(!BuiltinTools::new(None).persona_blocks().iter().any(|b| b.text().contains("<background_work>")));
}
