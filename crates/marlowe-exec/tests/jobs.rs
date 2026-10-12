//! Background jobs, notices, Ctrl+B and the output summary, through the tool host exactly as the
//! loop drives it (design 2.1 T12-T18, T32). Sessions are unique per test because the job
//! registry (`marlowe_os::job::bg_hub`) is process-wide.

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::Ws;
use marlowe_agent::{CallCtx, Conversation, StepInfo, ToolCallReq, ToolCtx, ToolHost, ToolMiddleware, ToolOutcome};
use marlowe_exec::{BuiltinTools, ShellLimits};
use marlowe_model::{CancelToken, Part};
use marlowe_os::job::{bg_hub, BgEvent, NoticeKind};
use marlowe_proto::SessionId;
use serde_json::{json, Value};

static NEXT: AtomicU32 = AtomicU32::new(0);

/// One session on one host, with the host's middleware.
struct Rig {
    ws: Ws,
    tools: Arc<BuiltinTools>,
    session: String,
    mw: Vec<Arc<dyn ToolMiddleware>>,
}

impl Rig {
    fn new(name: &str) -> Self {
        let ws = Ws::new(name);
        let tools = Arc::new(ws.tools());
        let session = format!("jobs-{name}-{}", NEXT.fetch_add(1, Ordering::Relaxed));
        let mw = tools.middleware();
        Self { ws, tools, session, mw }
    }

    fn cx(&self) -> CallCtx {
        CallCtx::detached(ToolCtx {
            session: SessionId::from(self.session.as_str()),
            turn: 1,
            workspace: Some(self.ws.root.clone()),
            cancel: CancelToken::new(),
            tainted: false,
        })
    }

    fn convo(&self) -> Conversation {
        Conversation::new(SessionId::from(self.session.as_str()), Some(self.ws.root.clone()))
    }

    /// A tool call the way the loop makes it: middleware's `before_call`, the tool, `after_call`.
    fn run(&self, id: &str, name: &str, args: Value) -> ToolOutcome {
        let call = ToolCallReq { id: id.into(), name: name.into(), args };
        let cx = self.cx();
        for m in &self.mw {
            let _ = m.before_call(&call, &cx);
        }
        let mut out = self.tools.call_scoped(&call, &cx);
        for m in &self.mw {
            m.after_call(&call, &cx, &mut out);
        }
        out
    }

    /// What the middleware hands the model at the end of a step.
    fn step_notices(&self) -> Vec<String> {
        let mut convo = self.convo();
        let mut out = Vec::new();
        for m in &self.mw {
            for msg in m.after_step(StepInfo { turn: 1, step: 1 }, &mut convo) {
                out.push(text_of(&msg.content));
            }
        }
        out
    }
}

fn text_of(parts: &[Part]) -> String {
    parts
        .iter()
        .filter_map(|p| match p {
            Part::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn job_id(started: &ToolOutcome) -> String {
    let rest = started.content.split("started background job ").nth(1).unwrap_or_else(|| panic!("{}", started.content));
    rest.split(':').next().unwrap().to_string()
}

fn posix(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

fn wait_for(what: &str, within: Duration, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + within;
    while Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(f(), "timed out waiting for {what}");
}

fn next_since(out: &ToolOutcome) -> u64 {
    let tail = out.content.rsplit("next offset=").next().unwrap();
    tail.trim_end_matches(']').trim().parse().unwrap_or_else(|_| panic!("no next offset in {}", out.content))
}

/// Notices of `session` that are already queued, drained from the registry.
fn hub_notices(session: &str) -> Vec<marlowe_os::job::BgNotice> {
    bg_hub().take_notices(session)
}

#[test]
fn parity_t12_t13_a_shell_never_sees_the_api_keys() {
    // Only this test touches these variables.
    std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-test-secret");
    std::env::set_var("OPENROUTER_API_KEY", "sk-or-test-secret");
    let rig = Rig::new("env");
    let o = rig.run("c1", "bash", json!({"command": "echo \"[${ANTHROPIC_API_KEY:-unset}] [${OPENROUTER_API_KEY:-unset}]\""}));
    assert_eq!(o.content.trim(), "[unset] [unset]", "{}", o.content);
    let o = rig.run("c2", "powershell", json!({"command": "Write-Output \"[$env:ANTHROPIC_API_KEY] [$env:OPENROUTER_API_KEY]\""}));
    assert_eq!(o.content.trim(), "[] []", "{}", o.content);
    // A background job is a child of the same spawner.
    let o = rig.run("c3", "bash", json!({"command": "echo \"[${ANTHROPIC_API_KEY:-unset}]\"", "background": true}));
    assert!(o.content.contains("[unset]") || o.content.contains("started background job"), "{}", o.content);
    let id = job_id(&o);
    wait_for("the job", Duration::from_secs(20), || {
        rig.run("c4", "task_output", json!({"id": id, "offset": 0})).content.contains("exited")
    });
    let read = rig.run("c5", "task_output", json!({"id": id, "offset": 0}));
    assert!(read.content.contains("[unset]") && !read.content.contains("sk-ant"), "{}", read.content);
}

#[test]
fn parity_t14_the_cap_is_ten_minutes_and_a_timeout_kills_the_tree() {
    assert_eq!(ShellLimits::production().with_timeout(99_000_000).timeout_ms, 600_000);
    assert_eq!(ShellLimits::production().with_timeout(1).timeout_ms, 1);
    // A background job with its own deadline: the grandchild would write at ~2.5 s.
    let rig = Rig::new("t14");
    let marker = rig.ws.path("alive.txt");
    let cmd = format!("( (sleep 2.5; echo up > '{}') & ); sleep 60", posix(&marker));
    let o = rig.run("c1", "bash", json!({"command": cmd, "background": true, "timeout_ms": 700}));
    let id = job_id(&o);
    wait_for("the stop notice", Duration::from_secs(20), || bg_hub().waking_sessions().contains(&rig.session));
    let notes = hub_notices(&rig.session);
    let text = notes.iter().map(|n| n.text.as_str()).collect::<Vec<_>>().join("\n");
    assert!(text.contains(&format!("job {id} was stopped")) && text.contains("at its timeout"), "{text}");
    std::thread::sleep(Duration::from_millis(3_500));
    assert!(!marker.exists(), "the grandchild outlived the timeout");
}

#[test]
fn parity_t15_job_output_streams_since_an_offset() {
    let rig = Rig::new("t15");
    let o = rig.run(
        "c1",
        "bash",
        json!({"command": "for i in 1 2 3 4; do echo line$i; sleep 0.4; done; echo end", "background": true}),
    );
    assert!(!o.is_error && o.content.contains("started background job"), "{}", o.content);
    let id = job_id(&o);
    wait_for("the job to finish", Duration::from_secs(30), || {
        rig.run("c2", "task_output", json!({"id": id, "offset": 0})).content.contains("exited")
    });
    // An explicit offset reads the stream from exactly there ("line1\n" is 6 bytes).
    let rest = rig.run("c5", "task_output", json!({"id": id, "offset": 6}));
    assert!(rest.content.starts_with("line2") && rest.content.contains("end") && !rest.content.contains("line1"), "{}", rest.content);
    assert_eq!(next_since(&rest), 28);
    let past = rig.run("c5b", "task_output", json!({"id": id, "offset": 999}));
    assert!(past.content.starts_with("(no new output") && next_since(&past) == 28, "{}", past.content);
    let all = rig.run("c6", "task_output", json!({"id": id, "offset": 0}));
    assert!(all.content.contains("line1") && all.content.contains("line4") && all.content.contains("end"), "{}", all.content);
    assert!(all.content.contains("exited 0"), "{}", all.content);
    // Without `since`: what is new since the last such read, once.
    let new1 = rig.run("c7", "task_output", json!({"id": id}));
    assert!(new1.content.contains("line1") && new1.content.contains("end"), "{}", new1.content);
    let new2 = rig.run("c8", "task_output", json!({"id": id}));
    assert!(new2.content.contains("no new output"), "{}", new2.content);
    // A filter keeps matching lines only.
    let f = rig.run("c9", "task_output", json!({"id": id, "offset": 0, "filter": "line[24]"}));
    assert!(f.content.contains("line2") && f.content.contains("line4") && !f.content.contains("line3"), "{}", f.content);
    // Unknown jobs and bad arguments say so.
    let bad = rig.run("c10", "task_output", json!({"id": "nope"}));
    assert!(bad.is_error && bad.content.contains("no job `nope`"), "{}", bad.content);
    let bad = rig.run("c11", "task_output", json!({"id": id, "filter": "("}));
    assert!(bad.is_error, "{}", bad.content);
    bg_hub().take_notices(&rig.session);
}

#[test]
fn parity_t15_a_running_job_streams_from_where_the_last_read_ended() {
    let rig = Rig::new("t15b");
    let o = rig.run("c1", "bash", json!({"command": "echo first; sleep 1; echo second; sleep 60", "background": true}));
    let id = job_id(&o);
    wait_for("the first line", Duration::from_secs(20), || {
        rig.run("c2", "task_output", json!({"id": id})).content.starts_with("first")
    });
    // The read above moved the cursor: nothing is new until the job writes again.
    let idle = rig.run("c3", "task_output", json!({"id": id}));
    assert!(idle.content.starts_with("(no new output)") || idle.content.starts_with("second"), "{}", idle.content);
    let second = if idle.content.starts_with("second") {
        idle
    } else {
        wait_for("the second line", Duration::from_secs(20), || {
            let r = rig.run("c4", "task_output", json!({"id": id, "offset": 6}));
            r.content.starts_with("second")
        });
        rig.run("c5", "task_output", json!({"id": id, "offset": 6}))
    };
    assert!(second.content.contains("running") && !second.content.contains("first"), "{}", second.content);
    assert_eq!(next_since(&second), 13);
    rig.run("c6", "task_stop", json!({"id": id}));
    bg_hub().take_notices(&rig.session);
}

#[test]
fn parity_t16_an_exit_notice_reaches_the_next_step_and_wakes_an_idle_session() {
    let rig = Rig::new("t16");
    let events = bg_hub().subscribe();
    let o = rig.run("c1", "bash", json!({"command": "sleep 2; echo done", "background": true}));
    let id = job_id(&o);
    // The call came back at once, and the job is announced to the daemon.
    assert!(o.content.contains("you will be told when it exits"), "{}", o.content);
    let started = loop {
        match events.recv_timeout(Duration::from_secs(5)).expect("a Started event") {
            BgEvent::Started(i) if i.id == id => break i,
            _ => {}
        }
    };
    assert!(started.session == rig.session && started.exit_code.is_none());
    assert!(rig.step_notices().is_empty(), "nothing has happened yet");
    // The turn ends here. The job exits while the session is idle: the registry says to wake it.
    wait_for("the session to be woken", Duration::from_secs(20), || bg_hub().waking_sessions().contains(&rig.session));
    let exited = loop {
        match events.recv_timeout(Duration::from_secs(5)).expect("an Exited event") {
            BgEvent::Exited { id: i, code, .. } if i == id => break code,
            _ => {}
        }
    };
    assert_eq!(exited, 0);
    // The daemon starts a turn with the notice as its context.
    let notes = hub_notices(&rig.session);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0].kind, NoticeKind::Exit);
    assert!(notes[0].text.contains(&format!("job {id} exited with code 0")) && notes[0].text.contains("done"), "{}", notes[0].text);
    assert!(bg_hub().waking_sessions().iter().all(|s| s != &rig.session), "taking the notice clears the wake");
}

#[test]
fn parity_t16_a_notice_lands_between_steps_and_ahead_of_the_next_turn() {
    // While the model works: delivered by `after_step`, once.
    let rig = Rig::new("t16b");
    let o = rig.run("c1", "bash", json!({"command": "echo quick; exit 3", "background": true}));
    // A command that has already failed says so in its own result, and queues nothing; a slow
    // start (a busy machine) gets the notice instead.
    if o.content.contains("already exited") {
        assert!(o.is_error && o.content.contains("code 3") && o.content.contains("quick"), "{}", o.content);
        assert!(rig.step_notices().is_empty(), "the result already told the model");
    } else {
        wait_for("the quick job's notice", Duration::from_secs(20), || bg_hub().waking_sessions().contains(&rig.session));
        assert_eq!(rig.step_notices().len(), 1);
    }
    let o = rig.run("c2", "bash", json!({"command": "sleep 1; echo slow; exit 4", "background": true}));
    let id = job_id(&o);
    wait_for("the exit notice", Duration::from_secs(20), || bg_hub().waking_sessions().contains(&rig.session));
    let notes = rig.step_notices();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains(&format!("job {id} exited with code 4")) && notes[0].contains("slow"), "{}", notes[0]);
    assert!(rig.step_notices().is_empty(), "a notice is delivered once");

    // Between turns: `before_turn` puts it ahead of the user's message.
    let o = rig.run("c3", "bash", json!({"command": "sleep 1; echo later", "background": true}));
    let id = job_id(&o);
    wait_for("the second exit", Duration::from_secs(20), || bg_hub().waking_sessions().contains(&rig.session));
    let mut convo = rig.convo();
    let mut text = "what now?".to_string();
    for m in &rig.mw {
        m.before_turn(&mut convo, &mut text);
    }
    assert_eq!(convo.messages.len(), 1);
    let injected = text_of(&convo.messages[0].content);
    assert!(injected.contains(&format!("job {id} exited with code 0")) && injected.contains("later"), "{injected}");
}

#[test]
fn parity_t16_job_watch_reports_lines_until_the_pattern_appears() {
    let rig = Rig::new("t16c");
    let o = rig.run(
        "c1",
        "bash",
        json!({"command": "echo boot; sleep 0.8; echo INFO ready on 8080; sleep 0.8; echo after; sleep 30", "background": true}),
    );
    let id = job_id(&o);
    let w = rig.run("c2", "task_output", json!({"id": id, "watch": true, "until": "ready on \\d+"}));
    assert!(!w.is_error && w.content.contains("watching job"), "{}", w.content);
    // The match ends the watch and wakes the session even though the job still runs.
    wait_for("the match", Duration::from_secs(20), || bg_hub().waking_sessions().contains(&rig.session));
    let notes = hub_notices(&rig.session);
    let all = notes.iter().map(|n| n.text.as_str()).collect::<Vec<_>>().join("\n");
    assert!(all.contains("boot") || all.contains("matched"), "{all}");
    assert!(all.contains("matched /ready on \\d+/") && all.contains("INFO ready on 8080"), "{all}");
    assert!(notes.iter().any(|n| n.kind == NoticeKind::Match));
    // The watch is over: later lines are not reported.
    std::thread::sleep(Duration::from_millis(1_500));
    assert!(hub_notices(&rig.session).is_empty());
    let k = rig.run("c3", "task_stop", json!({"id": id}));
    assert!(!k.is_error && k.content.contains("stopped job"), "{}", k.content);
    let bad = rig.run("c4", "task_output", json!({"id": id, "watch": true, "until": "("}));
    assert!(bad.is_error || bad.content.contains("already exited"), "{}", bad.content);
}

#[test]
fn parity_t16_a_line_printed_before_job_watch_still_matches() {
    let rig = Rig::new("t16d");
    // The usual order: the server prints "ready" within a second, the model watches a step later.
    let o = rig.run("c1", "bash", json!({"command": "echo boot; echo INFO ready on 8080; printf 'half a li'; sleep 30", "background": true}));
    let id = job_id(&o);
    wait_for("the ready line", Duration::from_secs(20), || {
        rig.run("c0", "task_output", json!({"id": id, "offset": 0})).content.contains("ready on 8080")
    });
    let w = rig.run("c2", "task_output", json!({"id": id, "watch": true, "until": "ready on \\d+"}));
    assert!(!w.is_error && w.content.contains("already matched") && w.content.contains("INFO ready on 8080"), "{}", w.content);
    assert!(w.content.contains("no watch was started"), "{}", w.content);
    // Nothing was installed, so no notice follows.
    std::thread::sleep(Duration::from_millis(400));
    assert!(hub_notices(&rig.session).is_empty());
    // A pattern that has not printed yet installs a watch.
    let w2 = rig.run("c3", "task_output", json!({"id": id, "watch": true, "until": "never printed"}));
    assert!(!w2.is_error && w2.content.contains("watching job"), "{}", w2.content);
    let k = rig.run("c4", "task_stop", json!({"id": id}));
    assert!(!k.is_error, "{}", k.content);
}

#[test]
fn a_watch_started_mid_line_sees_the_whole_next_line() {
    let rig = Rig::new("t16e");
    let marker = rig.ws.path("go.txt");
    let cmd = format!("printf 'half a li'; while [ ! -e '{}' ]; do sleep 0.1; done; echo 'ne done'; sleep 30", posix(&marker));
    let o = rig.run("c1", "bash", json!({"command": cmd, "background": true}));
    let id = job_id(&o);
    wait_for("the fragment", Duration::from_secs(20), || {
        rig.run("c0", "task_output", json!({"id": id, "offset": 0})).content.contains("half a li")
    });
    let w = rig.run("c2", "task_output", json!({"id": id, "watch": true, "until": "^half a line done$"}));
    assert!(!w.is_error && w.content.contains("watching job"), "{}", w.content);
    std::fs::write(&marker, "go").unwrap();
    wait_for("the match", Duration::from_secs(20), || bg_hub().waking_sessions().contains(&rig.session));
    let all = hub_notices(&rig.session).iter().map(|n| n.text.clone()).collect::<Vec<_>>().join("
");
    assert!(all.contains("half a line done"), "{all}");
    rig.run("c3", "task_stop", json!({"id": id}));
}

#[test]
fn parity_t32_a_flood_of_output_is_summarised_from_a_bounded_window() {
    let rig = Rig::new("t32flood");
    // About 20 MB of progress lines with one failure far from both ends.
    let script = "echo 'starting'; seq 1 1200000 | sed 's/^/compiling unit /'; echo 'error[E0599]: no method named `x`'; \
                  echo '  --> src/main.rs:7:9'; seq 1 1200000 | sed 's/^/linking object /'; echo 'build failed'; exit 1";
    let o = rig.run("c1", "bash", json!({"command": script}));
    assert!(o.is_error && o.summary == "exit 1", "{}", o.summary);
    assert!(o.content.len() <= 30_000 + 600, "{} bytes", o.content.len());
    assert!(o.content.contains("error[E0599]") && o.content.contains("src/main.rs:7:9"), "the failure in the middle was lost");
    assert!(o.content.contains("starting") && o.content.contains("build failed") && o.content.contains("[exit 1]"));
    let (log, lines) = pointer_of(&o.content);
    assert_eq!(lines, 2_400_004, "the pointer counts the whole log, not the window");
    assert!(std::fs::metadata(log).unwrap().len() > 20 << 20);
}

#[test]
fn parity_t17_job_kill_takes_the_whole_tree() {
    let rig = Rig::new("t17");
    let marker = rig.ws.path("grandchild.txt");
    // The middle subshell exits at once, so the writer has no living parent in the tree.
    let cmd = format!("( (sleep 3; echo up > '{}') & ); sleep 60", posix(&marker));
    let o = rig.run("c1", "bash", json!({"command": cmd, "background": true}));
    let id = job_id(&o);
    std::thread::sleep(Duration::from_millis(400));
    let k = rig.run("c2", "task_stop", json!({"id": id}));
    assert!(!k.is_error && k.content.contains(&format!("stopped job {id}")), "{}", k.content);
    // The model asked for it, so no notice follows.
    assert!(hub_notices(&rig.session).is_empty());
    std::thread::sleep(Duration::from_millis(4_000));
    assert!(!marker.exists(), "the grandchild survived job_kill");
    let again = rig.run("c3", "task_stop", json!({"id": id}));
    assert!(again.content.contains("had already exited"), "{}", again.content);
    // The registry lists it as finished, and killing by id works from the daemon's side too.
    let listed = bg_hub().list(Some(&rig.session));
    assert_eq!(listed.len(), 1);
    assert!(listed[0].exit_code.is_some());
    assert!(!bg_hub().kill(&id), "already gone");
}

#[test]
fn parity_t18_a_detached_foreground_call_keeps_its_output() {
    let rig = Rig::new("t18");
    let events = bg_hub().subscribe();
    let tools = Arc::clone(&rig.tools);
    let cx = rig.cx();
    let call = ToolCallReq {
        id: "call-detach-1".into(),
        name: "bash".into(),
        args: json!({"command": "echo first; sleep 2; echo second; sleep 0.2"}),
    };
    let mw = rig.mw.clone();
    let started = Instant::now();
    let worker = std::thread::spawn(move || {
        for m in &mw {
            let _ = m.before_call(&call, &cx);
        }
        tools.call_scoped(&call, &cx)
    });
    // The foreground call is visible to Ctrl+B once it has started.
    wait_for("the foreground shell", Duration::from_secs(20), || {
        bg_hub().list(Some(&rig.session)).iter().any(|j| j.foreground)
    });
    std::thread::sleep(Duration::from_millis(500));
    let detached = bg_hub().detach(&rig.session, Some("call-detach-1")).expect("the call was detached");
    let out = worker.join().unwrap();
    assert!(started.elapsed() < Duration::from_millis(1_900), "the call waited for the command: {:?}", started.elapsed());
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains("moved to the background") && out.content.contains(&detached) && out.content.contains("first"),
        "{}",
        out.content
    );
    let so_far = out.content.split("output so far:").nth(1).unwrap_or("");
    assert!(so_far.contains("first") && !so_far.contains("second"), "{}", out.content);
    // It runs on as a job: announced, and its whole output is kept.
    let announced = loop {
        match events.recv_timeout(Duration::from_secs(5)).expect("a Started event") {
            BgEvent::Started(i) if i.id == detached => break i,
            _ => {}
        }
    };
    assert!(!announced.foreground);
    wait_for("the exit", Duration::from_secs(30), || bg_hub().waking_sessions().contains(&rig.session));
    let notes = hub_notices(&rig.session);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].text.contains(&format!("job {detached} exited with code 0")), "{}", notes[0].text);
    let log = std::fs::read_to_string(announced.log.expect("a log")).unwrap();
    assert!(log.contains("first") && log.contains("second"), "{log}");
    let read = rig.run("c2", "task_output", json!({"id": detached, "offset": 0}));
    assert!(read.content.contains("first") && read.content.contains("second"), "{}", read.content);
    // Detaching again, or something that is not running, does nothing.
    assert!(bg_hub().detach(&rig.session, Some("call-detach-1")).is_none());
}

#[test]
fn a_foreground_call_that_finishes_cannot_be_detached_and_a_wrong_call_id_misses() {
    let rig = Rig::new("t18b");
    let o = rig.run("call-1", "bash", json!({"command": "echo plain"}));
    assert_eq!(o.content.trim(), "plain");
    assert!(bg_hub().detach(&rig.session, Some("call-1")).is_none());
    assert!(bg_hub().list(Some(&rig.session)).is_empty(), "a finished foreground call leaves no job behind");
    assert!(bg_hub().detach("no-such-session", None).is_none());
}

#[test]
fn the_user_bang_shell_streams_output_and_becomes_context_without_waking_anyone() {
    let rig = Rig::new("bang");
    // The host learns its session from a turn; give it one the way the loop does.
    let mut convo = rig.convo();
    let mut text = String::new();
    for m in &rig.mw {
        m.before_turn(&mut convo, &mut text);
    }
    let events = bg_hub().subscribe();
    let id = bg_hub().user_shell(&rig.session, "echo from the user; exit 2").expect("started");
    let mut seen = String::new();
    let end = Instant::now() + Duration::from_secs(30);
    let mut code = None;
    while Instant::now() < end && code.is_none() {
        match events.recv_timeout(Duration::from_millis(200)) {
            Ok(BgEvent::Output { id: i, text, .. }) if i == id => seen.push_str(&text),
            Ok(BgEvent::Exited { id: i, code: c, .. }) if i == id => code = Some(c),
            _ => {}
        }
    }
    assert_eq!(code, Some(2));
    assert!(seen.contains("from the user"), "{seen:?}");
    assert!(!bg_hub().waking_sessions().contains(&rig.session), "the user's own command never wakes the model");
    let mut convo = rig.convo();
    let mut text = "next".to_string();
    for m in &rig.mw {
        m.before_turn(&mut convo, &mut text);
    }
    let injected = convo.messages.iter().map(|m| text_of(&m.content)).collect::<Vec<_>>().join("\n");
    assert!(injected.contains("the user ran `echo from the user; exit 2` themselves") && injected.contains("from the user"), "{injected}");
}

/// The log path named by the pointer, `[full output: path, N lines]`.
fn pointer_of(content: &str) -> (PathBuf, usize) {
    let at = content.find("[full output: ").unwrap_or_else(|| panic!("no pointer in {}", &content[content.len().saturating_sub(300)..]));
    let rest = &content[at + "[full output: ".len()..];
    let end = rest.find(" lines]").unwrap();
    let (path, n) = rest[..end].rsplit_once(", ").unwrap();
    (PathBuf::from(path), n.parse().unwrap())
}

#[test]
fn parity_t32_a_long_failing_command_returns_30k_with_the_whole_log_readable() {
    let rig = Rig::new("t32");
    // 4000 progress lines, one error with its detail, more lines, exit 1.
    let script = "for i in $(seq 1 4000); do echo \"compiling unit $i of 4000\"; done; \
                  echo 'error[E0308]: mismatched types'; echo '  --> src/lib.rs:12:5'; \
                  for i in $(seq 1 4000); do echo \"linking object $i\"; done; echo 'build failed'; exit 1";
    let o = rig.run("c1", "bash", json!({"command": script}));
    assert!(o.is_error && o.summary == "exit 1", "{} / {}", o.summary, &o.content[o.content.len().saturating_sub(200)..]);
    assert!(o.content.len() <= 30_000 + 600, "{} bytes", o.content.len());
    assert!(o.content.contains("error[E0308]: mismatched types") && o.content.contains("src/lib.rs:12:5"), "the error was cut");
    assert!(o.content.contains("build failed") && o.content.contains("[exit 1]"));
    let (log, lines) = pointer_of(&o.content);
    assert_eq!(lines, 8_003, "the pointer counts the whole output");
    let whole = std::fs::read_to_string(&log).unwrap();
    assert_eq!(whole.lines().count(), 8_003);
    assert!(whole.contains("compiling unit 2000 of 4000") && whole.lines().any(|l| l == "linking object 1"), "the log has what the result left out");
    // `raw` cuts the middle instead of choosing lines: the error in the middle is gone from the
    // result, but still in the log.
    let raw = rig.run("c2", "bash", json!({"command": script, "raw": true}));
    assert!(raw.content.len() <= 30_000 + 600);
    assert!(raw.content.contains("lines omitted") && raw.content.contains("compiling unit 1 of 4000"), "{}", &raw.content[..200]);
    assert!(!raw.content.contains("error[E0308]"), "raw keeps head and tail only");
    let (log2, _) = pointer_of(&raw.content);
    assert!(std::fs::read_to_string(log2).unwrap().contains("error[E0308]"));
    // Output that fits leaves no log behind and no pointer.
    let small = rig.run("c3", "bash", json!({"command": "echo hi"}));
    assert_eq!(small.content.trim(), "hi");
    // `grep` opens the log: the model's route to the omitted part.
    let found = marlowe_exec::search::grep(
        &marlowe_exec::search::SearchEnv { root: None, port: &marlowe_os::workspace::LocalFs },
        &json!({"pattern": "linking object 3999", "path": log.to_string_lossy(), "output_mode": "content"}),
    );
    assert!(found.content.contains("linking object 3999"), "{}", found.content);
}

#[test]
fn a_powershell_job_runs_in_the_background_and_is_killed_as_a_tree() {
    let rig = Rig::new("ps");
    let o = rig.run("c1", "powershell", json!({"command": "Write-Output ps-up; Start-Sleep -Seconds 60", "background": true}));
    assert!(!o.is_error && o.content.contains("started background job"), "{}", o.content);
    let id = job_id(&o);
    wait_for("the output", Duration::from_secs(30), || {
        rig.run("c2", "task_output", json!({"id": id, "offset": 0})).content.contains("ps-up")
    });
    let k = rig.run("c3", "task_stop", json!({"id": id}));
    assert!(!k.is_error, "{}", k.content);
    assert!(rig.run("c4", "task_output", json!({"id": id, "offset": 0})).content.contains("exited"));
    assert!(hub_notices(&rig.session).is_empty(), "the kill was the model's own");
}

/// A9: `grep` over this whole workspace, warm, in 300 ms. A timing test is only honest on a quiet
/// machine, so it runs on request: `cargo test -p marlowe-exec --test jobs -- --ignored a9`.
#[test]
#[ignore = "timing: needs an idle machine"]
fn a9_a_workspace_wide_grep_is_fast_warm() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf();
    let tools = BuiltinTools::new(Some(root));
    let run = || {
        let started = Instant::now();
        let o = common::call(&tools, "grep", json!({"pattern": "fn [a-z_]+[(]", "glob": "*.rs"}));
        (started.elapsed(), o)
    };
    let (cold, o) = run();
    assert!(!o.is_error, "{}", o.content);
    let warm = (0..3).map(|_| run().0).min().unwrap();
    eprintln!("a9: cold {cold:?}, warm {warm:?}; {}", o.summary);
    assert!(warm < Duration::from_millis(300), "warm grep took {warm:?}");
}

#[test]
fn reading_a_job_log_back_does_not_taint_the_session() {
    use marlowe_exec::specs::{configure, HostSetup};
    let home = Ws::new("logs-home");
    let ws = Ws::new("logs-ws");
    let tools = configure(ws.tools(), &HostSetup { home: &home.root, workspace: Some(&ws.root) });
    let cx = CallCtx::detached(ToolCtx {
        session: SessionId::from("logs"),
        turn: 1,
        workspace: Some(ws.root.clone()),
        cancel: CancelToken::new(),
        tainted: false,
    });
    let call = ToolCallReq { id: "c1".into(), name: "bash".into(), args: json!({"command": "seq 1 20000", "background": false}) };
    let out = tools.call_scoped(&call, &cx);
    let (log, _) = pointer_of(&out.content);
    assert!(log.starts_with(posix(&home.root.join("jobs"))), "logs live under the profile's jobs folder: {}", log.display());
    let read = ToolCallReq { id: "c2".into(), name: "read".into(), args: json!({"path": log.to_string_lossy()}) };
    assert!(!tools.taints(&read), "the harness's own log is not untrusted content");
    let elsewhere = ToolCallReq { id: "c3".into(), name: "read".into(), args: json!({"path": home.root.join("other.txt").to_string_lossy()}) };
    assert!(tools.taints(&elsewhere), "the rest of the profile folder still is");
}

#[test]
fn parity_t32_a_long_line_in_short_output_comes_back_whole_with_nothing_deleted() {
    let rig = Rig::new("t32long");
    // 5,000 bytes on one line: Claude Code returns it as it is.
    let o = rig.run("c1", "bash", json!({"command": "printf '%05000d' 0"}));
    assert!(!o.is_error, "{}", o.content);
    assert_eq!(o.content.trim().len(), 5_000, "the line was clipped: {} bytes", o.content.len());
    assert!(!o.content.contains("more bytes]") && !o.content.contains("[full output"), "{}", &o.content[..80]);
    // Over the budget a clipped long line keeps its log.
    let big = rig.run("c2", "bash", json!({"command": "for i in $(seq 1 40); do printf '%01500d %d\n' 0 $i; done"}));
    let (log, _) = pointer_of(&big.content);
    assert!(std::fs::read_to_string(log).unwrap().contains("00000000"), "the log holds the whole output");
}

#[test]
fn a_crew_host_reads_its_job_log_back_without_tainting_the_run() {
    use marlowe_exec::Leftovers;
    let ws = Ws::new("crew-logs");
    // Built the way `crew_tool_factory` does, with no `specs::configure`.
    let tools = BuiltinTools::new_sandbox(ws.root.clone()).with_shell_dir(ws.root.clone()).with_leftovers(Leftovers::KillWithHost);
    let cx = CallCtx::detached(ToolCtx {
        session: SessionId::from("crew-logs"),
        turn: 1,
        workspace: Some(ws.root.clone()),
        cancel: CancelToken::new(),
        tainted: false,
    });
    let call = ToolCallReq { id: "c1".into(), name: "bash".into(), args: json!({"command": "seq 1 20000"}) };
    let out = tools.call_scoped(&call, &cx);
    let (log, _) = pointer_of(&out.content);
    assert!(log.starts_with(posix(&ws.root)), "the log is in the sandbox, not %TEMP%: {}", log.display());
    let read = ToolCallReq { id: "c2".into(), name: "read".into(), args: json!({"path": log.to_string_lossy()}) };
    assert!(!tools.taints(&read), "following the pointer must not taint a crew run");
    let found = ToolCallReq { id: "c3".into(), name: "grep".into(), args: json!({"pattern": "^19999$", "path": log.to_string_lossy()}) };
    assert!(!tools.taints(&found));
}

#[test]
fn the_user_bang_shell_works_in_a_session_that_has_run_no_turn() {
    use marlowe_exec::specs::{configure, HostSetup};
    let home = Ws::new("fresh-home");
    let ws = Ws::new("fresh-ws");
    let other = Ws::new("fresh-other");
    // A configured host is registered at once; no middleware, no turn.
    let tools = configure(ws.tools(), &HostSetup { home: &home.root, workspace: Some(&ws.root) });
    let session = "fresh-session";
    assert!(!bg_hub().serves(session));
    assert!(bg_hub().user_shell(session, "echo hi").is_err(), "a session no host serves has no shell");
    let root = ws.root.to_string_lossy().to_string();
    assert!(!bg_hub().claim(session, Some(&other.root.to_string_lossy())), "another workspace's host is not taken");
    assert!(bg_hub().claim(session, Some(&root)));
    assert!(bg_hub().serves(session));
    let events = bg_hub().subscribe();
    let id = bg_hub().user_shell(session, "echo from the first input").expect("started");
    let mut seen = String::new();
    let end = Instant::now() + Duration::from_secs(30);
    let mut done = false;
    while Instant::now() < end && !done {
        match events.recv_timeout(Duration::from_millis(200)) {
            Ok(BgEvent::Output { id: i, text, .. }) if i == id => seen.push_str(&text),
            Ok(BgEvent::Exited { id: i, .. }) if i == id => done = true,
            _ => {}
        }
    }
    assert!(done && seen.contains("from the first input"), "{seen:?}");
    // The user's command counts as running work while it runs: the daemon keeps the session.
    let long = bg_hub().user_shell(session, "sleep 30").expect("started");
    assert_eq!(bg_hub().running(session), 1, "a running `!` command keeps its session from idling out");
    assert!(bg_hub().kill(&long));
    drop(tools);
}

#[test]
fn two_unclaimed_hosts_in_one_workspace_are_not_guessed_between() {
    use marlowe_exec::specs::{configure, HostSetup};
    let home = Ws::new("amb-home");
    let ws = Ws::new("amb-ws");
    let setup = HostSetup { home: &home.root, workspace: Some(&ws.root) };
    let first = configure(ws.tools(), &setup);
    let second = configure(ws.tools(), &setup);
    let root = ws.root.to_string_lossy().to_string();
    // Another session's host built but not used yet looks the same as this session's own.
    assert!(!bg_hub().claim("amb-session", Some(&root)), "which of two hosts is not known");
    assert!(!bg_hub().serves("amb-session"));
    drop(second);
    assert!(bg_hub().claim("amb-session", Some(&root)), "the one left is the only candidate");
    assert!(bg_hub().serves("amb-session"));
    drop(first);
}
