//! Closing the window, in the terminal client (track N): quitting with background work running
//! asks first, a clean goodbye tells the daemon what to do, the next launch offers to resume what
//! a close interrupted, and the close handler's wire request. Key-event replays; every frame keeps
//! the palette (no backgrounds).

mod common;

use common::*;
use crossterm::event::KeyCode;
use marlowe_proto::{Event, InterruptedRun, Request, RunId, TaskInfo, TaskKind, TaskStatus};
use marlowe_tui::state::Overlay;
use marlowe_tui::State;

fn task(id: &str, kind: TaskKind, status: TaskStatus) -> TaskInfo {
    TaskInfo {
        id: id.into(),
        kind,
        session: Some(sid()),
        title: id.into(),
        status,
        started_ms: 1_800_000_000_000,
        ended_ms: None,
        output_file: None,
        detail: None,
    }
}

fn online() -> State {
    let mut s = coding_session();
    // The idle tick asks once what is running.
    let asked = s.on_idle();
    assert!(asked.contains(&Request::ListTasks { session: None }) && asked.contains(&Request::KeepRunning { on: None }), "{asked:?}");
    assert!(!s.on_idle().contains(&Request::ListTasks { session: None }), "once per Welcome");
    s
}

fn with_work(s: &mut State) {
    feed(
        s,
        0,
        Event::Tasks {
            list: vec![
                task("job_j1", TaskKind::Job, TaskStatus::Running),
                task("agent_r1", TaskKind::Agent, TaskStatus::Running),
                task("agent_r2", TaskKind::Agent, TaskStatus::Running),
                task("cron_1", TaskKind::Cron, TaskStatus::Waiting),
            ],
        },
    );
}

#[test]
fn quitting_with_nothing_running_goes_at_once_and_says_goodbye_without_keeping_anything() {
    let mut s = online();
    s.on_key(ctrl('q'));
    assert!(s.quit);
    assert_eq!(s.overlay, None);
    assert_eq!(s.exit_request(), Request::Detach { keep_running: false });
    // The same through ^c and /exit.
    let mut s = online();
    s.on_key(ctrl('c'));
    assert!(s.quit);
    let mut s = online();
    keys(&mut s, "'/exit' enter");
    assert!(s.quit);
}

#[test]
fn quitting_with_work_running_asks_and_a_stray_enter_cancels() {
    let mut s = online();
    with_work(&mut s);
    keys(&mut s, "ctrl-q");
    assert!(!s.quit, "the question comes first");
    assert!(matches!(s.overlay, Some(Overlay::Choice { sel: 2 })), "it opens on Cancel: {:?}", s.overlay);
    let frame = render(&s, W, H);
    assert_palette_only(&frame);
    let text = dump(&frame);
    for want in ["2 agent runs, 1 job", "frees the GPU", "Stop everything", "Keep running", "Cancel"] {
        assert!(text.contains(want), "{want} missing:\n{text}");
    }
    assert!(!text.contains("cron"), "schedules are not work worth asking about:\n{text}");
    // Enter on the default is Cancel; the window stays.
    keys(&mut s, "enter");
    assert!(!s.quit && s.overlay.is_none());
}

#[test]
fn stop_everything_keep_running_and_cancel_each_do_what_they_say() {
    // s: stop everything.
    let mut s = online();
    with_work(&mut s);
    keys(&mut s, "ctrl-q s");
    assert!(s.quit);
    assert_eq!(s.exit_request(), Request::Detach { keep_running: false });
    // k: keep running in the background.
    let mut s = online();
    with_work(&mut s);
    keys(&mut s, "ctrl-q k");
    assert!(s.quit);
    assert_eq!(s.exit_request(), Request::Detach { keep_running: true });
    // c and Esc: stay.
    for cancel in ["c", "esc"] {
        let mut s = online();
        with_work(&mut s);
        keys(&mut s, &format!("ctrl-q {cancel}"));
        assert!(!s.quit && s.overlay.is_none(), "{cancel}");
    }
    // Arrows and Enter pick too; a second ^q with the question up is the answer "stop".
    let mut s = online();
    with_work(&mut s);
    keys(&mut s, "ctrl-q left left enter");
    assert!(s.quit);
    assert_eq!(s.exit_request(), Request::Detach { keep_running: false });
    let mut s = online();
    with_work(&mut s);
    keys(&mut s, "ctrl-q ctrl-q");
    assert!(s.quit);
    assert_eq!(s.exit_request(), Request::Detach { keep_running: false });
}

#[test]
fn finished_work_is_no_longer_asked_about() {
    let mut s = online();
    with_work(&mut s);
    for (n, id) in ["job_j1", "agent_r1", "agent_r2"].into_iter().enumerate() {
        let kind = if id.starts_with("job") { TaskKind::Job } else { TaskKind::Agent };
        feed(&mut s, 5 + n as u64, Event::TaskUpdated(task(id, kind, TaskStatus::Done)));
    }
    keys(&mut s, "ctrl-q");
    assert!(s.quit, "nothing is running any more");
}

#[test]
fn with_keep_running_on_a_quit_never_asks_and_the_daemon_is_told_to_run_on() {
    let mut s = online();
    with_work(&mut s);
    feed(&mut s, 6, Event::KeepRunning { on: true });
    keys(&mut s, "ctrl-q");
    assert!(s.quit);
    assert_eq!(s.exit_request(), Request::Detach { keep_running: true });
}

#[test]
fn the_keep_running_command_flips_the_setting_and_reports_it() {
    let mut s = online();
    assert_eq!(keys(&mut s, "'/keep-running on' enter"), vec![Request::KeepRunning { on: Some(true) }]);
    assert_eq!(keys(&mut s, "'/keep-running off' enter"), vec![Request::KeepRunning { on: Some(false) }]);
    let q = keys(&mut s, "'/keep-running' enter");
    assert_eq!(q, vec![Request::KeepRunning { on: None }]);
    assert!(contains(&render(&s, W, H), "keep running is off"));
    let bad = keys(&mut s, "'/keep-running maybe' enter");
    assert!(bad.is_empty() && contains(&render(&s, W, H), "takes on or off"));
}

#[test]
fn a_loop_with_no_interval_is_sent_to_the_daemon_to_pace_itself() {
    let mut s = online();
    let out = keys(&mut s, "'/loop watch the build' enter");
    assert!(
        out.iter().any(|r| matches!(r, Request::Ask { text, .. } if text == "/loop watch the build")),
        "the daemon expands a self-paced loop: {out:?}"
    );
    assert!(!out.iter().any(|r| matches!(r, Request::AddSchedule { .. })));
    // With an interval it stays a stored schedule, as before.
    let mut s = online();
    let out = keys(&mut s, "'/loop 10m check the build' enter");
    assert!(out.iter().any(|r| matches!(r, Request::AddSchedule { every_secs: 600, .. })), "{out:?}");
}

fn interrupted() -> Event {
    Event::Interrupted {
        runs: vec![
            InterruptedRun { run: RunId::from("r-1"), title: "evaluate A".into(), workflow: false, stopped_at: "stopped after step 4".into(), at_ms: 1 },
            InterruptedRun { run: RunId::from("r-2"), title: "evaluate B".into(), workflow: false, stopped_at: "stopped after step 0".into(), at_ms: 1 },
            InterruptedRun { run: RunId::from("r-3"), title: "code review".into(), workflow: true, stopped_at: "stopped after step 9".into(), at_ms: 1 },
        ],
        summary: "2 crew runs, 1 workflow".into(),
    }
}

#[test]
fn the_next_launch_offers_to_resume_and_yes_resumes() {
    let mut s = online();
    feed(&mut s, 0, interrupted());
    assert!(matches!(s.overlay, Some(Overlay::Choice { sel: 0 })), "{:?}", s.overlay);
    let frame = render(&s, W, H);
    assert_palette_only(&frame);
    // The dialog wraps its sentence; the sentence itself is the dialog's.
    assert_eq!(
        s.closing.dialog.as_ref().map(|d| d.prompt()),
        Some("We were still working when you closed Marlowe last time (2 crew runs, 1 workflow). Resume?".to_string())
    );
    for part in ["We were still working when you closed", "Resume?", "Yes", "Not now"] {
        assert!(contains(&frame, part), "{part}
{}", dump(&frame));
    }
    let out = keys(&mut s, "y");
    assert_eq!(out, vec![Request::ResumeInterrupted { resume: true }]);
    assert!(s.overlay.is_none());
}

#[test]
fn not_now_leaves_the_runs_paused_and_enter_on_the_second_button_says_so() {
    let mut s = online();
    feed(&mut s, 0, interrupted());
    assert_eq!(keys(&mut s, "n"), vec![Request::ResumeInterrupted { resume: false }]);
    let mut s = online();
    feed(&mut s, 0, interrupted());
    assert_eq!(keys(&mut s, "tab enter"), vec![Request::ResumeInterrupted { resume: false }]);
    // Esc closes the question without answering: the daemon asks again next launch.
    let mut s = online();
    feed(&mut s, 0, interrupted());
    assert!(keys(&mut s, "esc").is_empty());
    assert!(s.overlay.is_none());
}

#[test]
fn an_offer_that_arrives_while_something_else_is_up_waits_for_the_screen() {
    let mut s = online();
    keys(&mut s, "ctrl-k");
    assert!(matches!(s.overlay, Some(Overlay::Palette { .. })));
    feed(&mut s, 0, interrupted());
    assert!(matches!(s.overlay, Some(Overlay::Palette { .. })), "the palette is not pushed aside");
    keys(&mut s, "esc");
    assert!(s.overlay.is_none());
    s.on_idle();
    assert!(matches!(s.overlay, Some(Overlay::Choice { .. })), "now it opens: {:?}", s.overlay);
}

#[test]
fn the_palette_keeps_to_the_theme_with_the_dialogs_up() {
    for ev in [None, Some(interrupted())] {
        let mut s = online();
        match ev {
            Some(e) => {
                feed(&mut s, 0, e);
            }
            None => {
                with_work(&mut s);
                keys(&mut s, "ctrl-q");
            }
        }
        let frame = render(&s, W, H);
        assert_palette_only(&frame);
        let small = render(&s, 60, 18);
        assert_palette_only(&small);
    }
    let _ = KeyCode::Esc;
}
