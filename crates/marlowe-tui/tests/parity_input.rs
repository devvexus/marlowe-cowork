//! Parity with Claude Code, the Message box: key-event replay (a scripted key stream in, the
//! requests and the screen out). One test per row of the design's parity matrix, named
//! `parity_<row>`: U3 image paste, U6 queued input, U7 `@` mentions, U8 `!` shell, U9 Shift+Tab,
//! U10 history and Ctrl+R, T18 Ctrl+B, M1 the plan-mode surface.
#![allow(non_snake_case)]

mod common;

use common::*;
use marlowe_model::ImageRef;
use marlowe_proto::{
    ApprovalDecision, ApprovalId, Event, JobInfo, PermissionMode, Request, SessionId,
};
use marlowe_tui::attach;
use marlowe_tui::coding::{Effect, ImageSource, LocalEvent};
use marlowe_tui::state::{Entry, Overlay};
use marlowe_tui::ClientEvent;

fn local(s: &mut marlowe_tui::State, ev: LocalEvent) -> Vec<Request> {
    s.on_client(ClientEvent::Local(ev))
}

fn ask(text: &str) -> Request {
    Request::Ask { session: Some(sid()), text: text.into(), workspace: None, images: vec![], options: None }
}

// ---- U9, M1 ----------------------------------------------------------------------------------------

#[test]
fn parity_U9_shift_tab_cycles_the_mode_and_the_box_says_which() {
    let mut s = coding_session();
    assert_eq!(keys(&mut s, "shift-tab"), vec![Request::SetMode { session: sid(), mode: PermissionMode::AcceptEdits }]);
    assert!(contains(&palette_frame(&s), "accept edits"));
    assert_eq!(keys(&mut s, "shift-tab"), vec![Request::SetMode { session: sid(), mode: PermissionMode::Plan }]);
    assert!(contains(&palette_frame(&s), "plan mode"));
    assert_eq!(keys(&mut s, "shift-tab"), vec![Request::SetMode { session: sid(), mode: PermissionMode::Default }]);
    let buf = palette_frame(&s);
    assert!(!contains(&buf, "plan mode") && !contains(&buf, "accept edits"), "the default mode shows nothing");
    // The daemon has the last word.
    feed(&mut s, 5, Event::ModeChanged { session: sid(), mode: PermissionMode::Bypass });
    assert!(contains(&palette_frame(&s), "bypass"));
}

#[test]
fn parity_U9_a_mode_chosen_before_the_session_exists_is_sent_when_it_does() {
    let mut s = state();
    welcome(&mut s, 0);
    feed(&mut s, 1, library_with_sonnet());
    assert!(keys(&mut s, "shift-tab").is_empty(), "no session to send it to yet");
    keys(&mut s, "'hi' enter");
    let reqs = feed(&mut s, 0, Event::SessionOpened { session: sid(), title: "hi".into(), workspace: None });
    assert_eq!(reqs, vec![Request::SetMode { session: sid(), mode: PermissionMode::AcceptEdits }]);
}

#[test]
fn parity_M1_plan_mode_is_a_chip_and_exit_plan_is_an_approval_of_the_plan() {
    let mut s = coding_session();
    keys(&mut s, "shift-tab shift-tab");
    assert_eq!(s.code.mode, PermissionMode::Plan);
    assert!(contains(&palette_frame(&s), "plan mode"));
    feed(
        &mut s,
        6,
        Event::ApprovalRequested {
            approval: ApprovalId(11),
            session: sid(),
            tool: "exit_plan".into(),
            summary: "leave plan mode".into(),
            detail: r#"{"plan":"1. Read the gate code\n2. Change the threshold\n3. Run the tests"}"#.into(),
            run: None,
            proposed_rule: Some("exit_plan".into()),
        },
    );
    let buf = palette_frame(&s);
    for want in ["has a plan", "1. Read the gate code", "3. Run the tests", "approve the plan", "keep planning"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    assert!(!contains(&buf, "always"), "a plan has nothing to remember");
    snapshot("exit_plan_approval", &buf);
    s.now_ms += 1_000;
    keys(&mut s, "pgdn");
    // 'a' is not a plan decision.
    assert!(keys(&mut s, "a").is_empty());
    let reqs = keys(&mut s, "y");
    assert_eq!(reqs, vec![Request::Approve { approval: ApprovalId(11), decision: ApprovalDecision::Allow, reason: None }]);
    feed(&mut s, 7, Event::ModeChanged { session: sid(), mode: PermissionMode::Default });
    assert!(!contains(&palette_frame(&s), "plan mode"));
}

// ---- U10 -------------------------------------------------------------------------------------------

#[test]
fn parity_U10_up_and_down_walk_the_history_at_the_edge_of_the_buffer() {
    let mut s = coding_session();
    keys(&mut s, "'alpha' enter 'beta' enter");
    keys(&mut s, "up");
    assert_eq!(s.composer.text(), "beta");
    keys(&mut s, "up");
    assert_eq!(s.composer.text(), "alpha");
    keys(&mut s, "up");
    assert_eq!(s.composer.text(), "alpha", "nothing older");
    keys(&mut s, "down");
    assert_eq!(s.composer.text(), "beta");
    keys(&mut s, "down");
    assert_eq!(s.composer.text(), "", "past the newest is the draft again");
    // A half-typed line is the draft, and a multi-line buffer moves its cursor first.
    keys(&mut s, "'one' shift-enter 'two'");
    keys(&mut s, "up");
    assert_eq!(s.composer.text(), "one\ntwo", "up moved to the first line");
    keys(&mut s, "up");
    assert_eq!(s.composer.text(), "beta");
    keys(&mut s, "down");
    assert_eq!(s.composer.text(), "one\ntwo", "the draft comes back");
}

#[test]
fn parity_U10_ctrl_r_searches_the_history() {
    let mut s = coding_session();
    keys(&mut s, "'fix the gate' enter 'run the tests' enter 'fix the build' enter");
    keys(&mut s, "'half typed'");
    keys(&mut s, "ctrl-r 'fix'");
    let buf = palette_frame(&s);
    for want in ["history", "search › fix", "fix the build", "fix the gate"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    snapshot("history_search", &buf);
    // The newest match first; ^r again goes older; enter puts it in the box without sending.
    keys(&mut s, "ctrl-r");
    let reqs = keys(&mut s, "enter");
    assert!(reqs.is_empty(), "a found prompt is edited, not sent");
    assert_eq!(s.composer.text(), "fix the gate");
    // Esc puts back what was there.
    keys(&mut s, "ctrl-u 'draft' ctrl-r 'zzz'");
    assert!(contains(&palette_frame(&s), "nothing matches"));
    keys(&mut s, "esc");
    assert_eq!(s.composer.text(), "draft");
    assert!(!contains(&palette_frame(&s), "search ›"));
}

// ---- U6 --------------------------------------------------------------------------------------------

#[test]
fn parity_U6_a_message_typed_while_the_model_works_is_queued_and_sent_at_the_step_boundary() {
    let mut s = busy_session();
    let reqs = keys(&mut s, "'also check CI' enter");
    assert!(reqs.is_empty(), "held, not sent: {reqs:?}");
    let buf = palette_frame(&s);
    assert!(contains(&buf, "queued") && contains(&buf, "also check CI"), "{}", dump(&buf));
    assert!(contains(&buf, "1 queued"), "the box says how many");
    snapshot("queued_message", &buf);
    // A tool starts and finishes: the model is about to take its next step.
    assert!(feed(&mut s, 3, tool_started("c1", "read", "read src/gate.rs")).is_empty());
    let reqs = feed(&mut s, 4, tool_finished("c1", true, "80 lines"));
    assert_eq!(reqs, vec![ask("also check CI")]);
    assert_eq!(s.entries.last(), Some(&Entry::User { text: "also check CI".into(), steer: true }));
    assert_eq!(s.code.queue_len(), 0);
    assert!(!contains(&palette_frame(&s), "1 queued"));
}

#[test]
fn parity_U6_queued_messages_become_the_next_turns_one_at_a_time() {
    let mut s = busy_session();
    keys(&mut s, "'first' enter 'second' enter");
    assert_eq!(s.code.queue_len(), 2);
    let reqs = feed(&mut s, 3, Event::TurnFinished { session: sid(), turn: 1, usage: Default::default(), stop: marlowe_proto::TurnStop::Done });
    assert!(reqs.contains(&ask("first")), "the turn is over: the first starts the next one: {reqs:?}");
    assert!(!reqs.contains(&ask("second")), "the second waits its turn");
    assert_eq!(s.code.queued_texts(), ["second"]);
}

#[test]
fn parity_U6_up_takes_the_last_queued_message_back_to_edit_and_esc_still_interrupts() {
    let mut s = busy_session();
    keys(&mut s, "'one' enter 'two' enter");
    keys(&mut s, "up");
    assert_eq!(s.composer.text(), "two");
    assert_eq!(s.code.queued_texts(), ["one"]);
    assert_eq!(keys(&mut s, "esc"), vec![Request::Interrupt { session: sid() }]);
    assert_eq!(s.code.queue_len(), 1, "an interrupt keeps what was queued");
}

#[test]
fn parity_U6_esc_stops_the_model_and_the_queued_messages_come_back_into_the_box_unsent() {
    let mut s = busy_session();
    keys(&mut s, "'one' enter 'two' enter");
    assert_eq!(keys(&mut s, "esc"), vec![Request::Interrupt { session: sid() }]);
    // The tool the interrupt cancelled finishes first: that is not a step boundary to steer at.
    feed(&mut s, 3, tool_started("c1", "bash", "bash sleep 60"));
    let reqs = feed(&mut s, 4, tool_finished("c1", false, "interrupted"));
    assert!(!reqs.iter().any(|r| matches!(r, Request::Ask { .. })), "after Esc nothing is steered in: {reqs:?}");
    let reqs = feed(&mut s, 5, Event::TurnFinished { session: sid(), turn: 1, usage: Default::default(), stop: marlowe_proto::TurnStop::Interrupted });
    assert!(!reqs.iter().any(|r| matches!(r, Request::Ask { .. })), "the stopped model is not started again: {reqs:?}");
    assert_eq!(s.code.queue_len(), 0);
    assert_eq!(s.composer.text(), "one\ntwo", "the queued messages are back in the box");
    assert!(contains(&palette_frame(&s), "queued messages are back in the box"));
    // Ahead of a draft typed since.
    let mut s = busy_session();
    keys(&mut s, "'one' enter 'draft'");
    keys(&mut s, "esc");
    feed(&mut s, 5, Event::TurnFinished { session: sid(), turn: 1, usage: Default::default(), stop: marlowe_proto::TurnStop::Interrupted });
    assert_eq!(s.composer.text(), "one\ndraft");
}

#[test]
fn parity_U6_a_queued_message_keeps_its_image_and_goes_up_with_it() {
    let mut s = busy_session();
    keys(&mut s, "ctrl-v");
    s.take_effects();
    let png = png_bytes(640, 480);
    local(&mut s, LocalEvent::Image { source: ImageSource::Clipboard, result: Ok(("image/png".into(), png.clone())) });
    let reqs = keys(&mut s, "'why is this red' enter");
    assert!(reqs.is_empty(), "held while the model works: {reqs:?}");
    assert_eq!(s.code.queue_len(), 1);
    assert!(s.code.attachments.is_empty(), "the image went into the queue with its text");
    // The step boundary: the image uploads, then the Ask carries it.
    let reqs = feed(&mut s, 4, tool_finished("c1", true, "80 lines"));
    assert_eq!(reqs, vec![Request::PutBlob { mime: "image/png".into(), base64: attach::base64(&png) }]);
    let reqs = feed(&mut s, 0, Event::BlobStored { blob: "b1".into(), mime: "image/png".into(), w: 640, h: 480 });
    assert_eq!(
        reqs,
        vec![Request::Ask {
            session: Some(sid()),
            text: "[Image #1] why is this red".into(),
            workspace: None,
            images: vec![ImageRef { mime: "image/png".into(), blob: "b1".into(), w: 640, h: 480 }],
            options: None,
        }]
    );
    // Taken back with Up, the image comes back with it.
    let mut s = busy_session();
    keys(&mut s, "ctrl-v");
    s.take_effects();
    local(&mut s, LocalEvent::Image { source: ImageSource::Clipboard, result: Ok(("image/png".into(), png_bytes(8, 8))) });
    keys(&mut s, "'x' enter");
    assert!(s.code.attachments.is_empty());
    keys(&mut s, "up");
    assert_eq!(s.code.attachments.len(), 1);
    assert!(s.composer.text().starts_with("[Image #1]"));
}

// ---- U8 --------------------------------------------------------------------------------------------

fn job(id: &str, command: &str, code: Option<i32>) -> JobInfo {
    JobInfo { id: id.into(), session: Some(sid()), command: command.into(), started_ms: 1_800_000_000_000, exit_code: code }
}

#[test]
fn parity_U8_bang_runs_a_command_as_the_user_and_its_output_lands_in_the_conversation() {
    let mut s = coding_session();
    let reqs = keys(&mut s, "'!git status --short' enter");
    assert_eq!(reqs, vec![Request::UserShell { session: sid(), command: "git status --short".into() }]);
    feed(&mut s, 2, Event::JobStarted { job: job("j1", "git status --short", None) });
    feed(&mut s, 3, Event::JobOutput { session: Some(sid()), id: "j1".into(), text: " M src/gate.rs\n?? notes.md\n".into() });
    let buf = palette_frame(&s);
    for want in ["$ git status --short", "running", "M src/gate.rs", "?? notes.md"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    feed(&mut s, 4, Event::JobExited { session: Some(sid()), id: "j1".into(), code: 0 });
    let buf = palette_frame(&s);
    assert!(contains(&buf, "done") && !contains(&buf, "running"));
    snapshot("shell_output", &buf);
    // It is in the prompt history like any other line.
    keys(&mut s, "up");
    assert_eq!(s.composer.text(), "!git status --short");
}

#[test]
fn parity_U8_a_bang_before_any_session_opens_one_first() {
    let mut s = state();
    welcome(&mut s, 0);
    feed(&mut s, 1, library_with_sonnet());
    let reqs = keys(&mut s, "'!echo hi' enter");
    assert_eq!(reqs, vec![Request::NewSession { workspace: Some("C:/work/valiance".into()) }]);
    let reqs = feed(&mut s, 0, Event::SessionOpened { session: sid(), title: String::new(), workspace: None });
    assert_eq!(reqs, vec![Request::UserShell { session: SessionId::from("5e55"), command: "echo hi".into() }]);
}

// ---- T18 -------------------------------------------------------------------------------------------

#[test]
fn parity_T18_ctrl_b_moves_the_running_shell_command_to_a_job_that_keeps_its_output() {
    let mut s = busy_session();
    feed(&mut s, 3, tool_started("c9", "bash", "bash cargo test --workspace"));
    let reqs = keys(&mut s, "ctrl-b");
    assert_eq!(reqs, vec![Request::DetachCall { session: sid(), call_id: "c9".into() }]);
    assert!(contains(&palette_frame(&s), "moved to the background"));
    // The daemon starts the job; its output keeps arriving.
    feed(&mut s, 4, Event::JobStarted { job: job("j7", "cargo test --workspace", None) });
    feed(&mut s, 5, Event::JobOutput { session: Some(sid()), id: "j7".into(), text: "running 120 tests\n".into() });
    feed(&mut s, 6, Event::JobOutput { session: Some(sid()), id: "j7".into(), text: "test result: ok\n".into() });
    s.flash = None;
    let buf = palette_frame(&s);
    assert!(contains(&buf, "running 120 tests") && contains(&buf, "test result: ok"), "{}", dump(&buf));
    assert!(contains(&buf, "1 job"), "the box counts it");
    feed(&mut s, 7, Event::JobExited { session: Some(sid()), id: "j7".into(), code: 0 });
    assert!(!contains(&palette_frame(&s), "1 job"));
}

#[test]
fn parity_T18_ctrl_b_with_nothing_to_move_says_so() {
    let mut s = busy_session();
    feed(&mut s, 3, tool_started("c1", "read", "read a.rs"));
    assert!(keys(&mut s, "ctrl-b").is_empty());
    assert!(contains(&palette_frame(&s), "no command is running"));
}

// ---- U7 --------------------------------------------------------------------------------------------

fn index() -> Vec<String> {
    ["src/render/mod.rs", "src/model.rs", "README.md", "docs/shot.png", "Cargo.toml"].iter().map(|s| s.to_string()).collect()
}

#[test]
fn parity_U7_at_asks_for_the_file_list_once_and_completes_a_fuzzy_match() {
    let mut s = coding_session();
    keys(&mut s, "'@'");
    assert_eq!(s.take_effects(), vec![Effect::IndexFiles { root: "C:/work/valiance".into() }]);
    assert!(contains(&palette_frame(&s), "looking through the workspace"));
    local(&mut s, LocalEvent::FileIndex { root: "C:/work/valiance".into(), files: index() });
    keys(&mut s, "ctrl-u 'look at @mod'");
    assert!(s.take_effects().is_empty(), "the list is asked for once");
    let buf = palette_frame(&s);
    for want in ["files", "src/render/mod.rs", "src/model.rs"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    snapshot("mention_popup", &buf);
    keys(&mut s, "enter");
    assert_eq!(s.composer.text(), "look at @src/render/mod.rs ");
    assert!(!contains(&palette_frame(&s), "src/model.rs"), "the list closes");
    // Down picks the second.
    keys(&mut s, "'@mod' down enter");
    assert!(s.composer.text().ends_with("@src/model.rs "), "{:?}", s.composer.text());
}

#[test]
fn parity_U7_an_image_picked_with_at_becomes_an_attachment() {
    let mut s = coding_session();
    keys(&mut s, "'@'");
    s.take_effects();
    local(&mut s, LocalEvent::FileIndex { root: "C:/work/valiance".into(), files: index() });
    keys(&mut s, "'shot' enter");
    assert_eq!(s.take_effects(), vec![Effect::ReadImage { path: "C:/work/valiance/docs/shot.png".into() }]);
    assert_eq!(s.composer.text(), "", "the token typed to find it is gone; the marker replaces it");
    local(&mut s, LocalEvent::Image { source: ImageSource::Path("C:/work/valiance/docs/shot.png".into()), result: Ok(("image/png".into(), png_bytes(32, 32))) });
    assert_eq!(s.composer.text(), "[Image #1] ");
    assert!(contains(&palette_frame(&s), "1 image"));
}

// ---- U3 --------------------------------------------------------------------------------------------

#[test]
fn parity_U3_ctrl_v_attaches_the_clipboard_image_and_the_ask_carries_it() {
    let mut s = coding_session();
    assert!(keys(&mut s, "ctrl-v").is_empty());
    assert_eq!(s.take_effects(), vec![Effect::ClipboardImage]);
    let png = png_bytes(640, 480);
    local(&mut s, LocalEvent::Image { source: ImageSource::Clipboard, result: Ok(("image/png".into(), png.clone())) });
    assert_eq!(s.composer.text(), "[Image #1] ");
    let buf = palette_frame(&s);
    assert!(contains(&buf, "1 image"), "{}", dump(&buf));
    assert!(contains(&buf, "[Image #1] 640×480"), "the chip names its size");
    // Send: the image goes up first, the Ask follows with the reference.
    let reqs = keys(&mut s, "'what is wrong here' enter");
    assert_eq!(reqs, vec![Request::PutBlob { mime: "image/png".into(), base64: attach::base64(&png) }]);
    let reqs = feed(&mut s, 0, Event::BlobStored { blob: "b1".into(), mime: "image/png".into(), w: 640, h: 480 });
    assert_eq!(
        reqs,
        vec![Request::Ask {
            session: Some(sid()),
            text: "[Image #1] what is wrong here".into(),
            workspace: None,
            images: vec![ImageRef { mime: "image/png".into(), blob: "b1".into(), w: 640, h: 480 }],
            options: None,
        }]
    );
    assert!(!contains(&palette_frame(&s), "1 image"));
}

#[test]
fn parity_U3_an_attachment_whose_marker_was_deleted_is_not_sent() {
    let mut s = coding_session();
    keys(&mut s, "ctrl-v");
    s.take_effects();
    local(&mut s, LocalEvent::Image { source: ImageSource::Clipboard, result: Ok(("image/png".into(), png_bytes(8, 8))) });
    keys(&mut s, "backspace backspace backspace backspace backspace backspace backspace backspace backspace backspace backspace");
    assert_eq!(s.composer.text(), "");
    assert_eq!(keys(&mut s, "'plain' enter"), vec![ask("plain")]);
}

#[test]
fn parity_U3_a_dropped_image_path_and_an_empty_paste_look_for_the_image() {
    let mut s = coding_session();
    s.on_paste("\"C:\\Users\\m\\Pictures\\shot 1.png\"");
    assert_eq!(s.take_effects(), vec![Effect::ReadImage { path: "C:\\Users\\m\\Pictures\\shot 1.png".into() }]);
    assert_eq!(s.composer.text(), "", "nothing is typed until the file is read");
    local(&mut s, LocalEvent::Image { source: ImageSource::Path("C:\\Users\\m\\Pictures\\shot 1.png".into()), result: Ok(("image/png".into(), png_bytes(10, 10))) });
    assert_eq!(s.composer.text(), "[Image #1] ");
    // A path that is not an image after all is just text.
    local(&mut s, LocalEvent::Image { source: ImageSource::Path("C:\\x\\fake.png".into()), result: Err("the file is not the image its name says".into()) });
    assert!(s.composer.text().ends_with("C:\\x\\fake.png "));
    // Ordinary text pastes as text.
    s.composer.clear();
    s.on_paste("just some text\nover two lines");
    assert_eq!(s.composer.text(), "just some text\nover two lines");
    assert!(s.take_effects().is_empty());
    // An empty paste is how some terminals say "the clipboard holds a picture".
    s.on_paste("");
    assert_eq!(s.take_effects(), vec![Effect::ClipboardImage]);
}

#[test]
fn parity_U3_no_image_on_the_clipboard_is_said_plainly() {
    let mut s = coding_session();
    keys(&mut s, "alt-p");
    assert_eq!(s.take_effects(), vec![Effect::ClipboardImage]);
    local(&mut s, LocalEvent::Image { source: ImageSource::Clipboard, result: Err(String::new()) });
    assert!(contains(&palette_frame(&s), "no image on the clipboard"));
}

#[test]
fn the_overlay_open_during_a_paste_is_not_the_message_box() {
    let mut s = coding_session();
    keys(&mut s, "ctrl-k");
    assert!(matches!(s.overlay, Some(Overlay::Palette { .. })));
    s.on_paste("alpha");
    assert!(s.composer.text().is_empty());
}
