//! Parity with Claude Code, what is drawn: U1 inline diffs, U2 clickable `file:line`, U4 the
//! status line, U11 the Ctrl+O transcript, the build DAG strip, and the locked palette.
#![allow(non_snake_case)]

mod common;

use common::*;
use marlowe_model::{Part, Role, ToolCall, ToolResult};
use marlowe_proto::{
    ApprovalId, CrewBudget, CrewRunInfo, CrewRunStatus, CrewSpent, Event, Playbook, Request, RunId, SettingsScope, StoredTurn,
};
use marlowe_surface::chrome::Ink;
use marlowe_tui::coding::{Effect, LocalEvent};
use marlowe_tui::links::{LinkEmitter, LinkRegion};
use marlowe_tui::state::Entry;
use marlowe_tui::{ClientEvent, View};
use ratatui::style::Color;
use serde_json::json;

const GATE_OLD: &str = "fn gate(x: f32) -> bool {\n    x > 0.5\n}";
const GATE_NEW: &str = "fn gate(x: f32) -> bool {\n    x > 0.62\n}";

// ---- U1 --------------------------------------------------------------------------------------------

#[test]
fn parity_U1_an_edit_approval_shows_the_diff_not_the_arguments() {
    let mut s = coding_session();
    feed(
        &mut s,
        3,
        Event::ApprovalRequested {
            approval: ApprovalId(3),
            session: sid(),
            tool: "edit".into(),
            summary: "edit src/gate.rs".into(),
            detail: json!({"path": "src/gate.rs", "old_string": GATE_OLD, "new_string": GATE_NEW}).to_string(),
            run: None,
            proposed_rule: Some("edit(src/**)".into()),
        },
    );
    let buf = palette_frame(&s);
    let text = dump(&buf);
    for want in ["src/gate.rs", "+1 −1", "−     x > 0.5", "+     x > 0.62", "fn gate(x: f32) -> bool {"] {
        assert!(text.contains(want), "{want:?}\n{text}");
    }
    assert!(!text.contains("old_string"), "the argument names are not shown\n{text}");
    snapshot("approval_diff", &buf);
}

#[test]
fn parity_U1_a_write_approval_is_all_additions_and_a_long_one_scrolls_before_allow_works() {
    let mut s = coding_session();
    let content: String = (1..=40).map(|i| format!("line {i}\n")).collect();
    feed(
        &mut s,
        3,
        Event::ApprovalRequested {
            approval: ApprovalId(4),
            session: sid(),
            tool: "write".into(),
            summary: "create notes.md".into(),
            detail: json!({"path": "notes.md", "content": content}).to_string(),
            run: None,
            proposed_rule: None,
        },
    );
    let buf = palette_frame(&s);
    assert!(contains(&buf, "notes.md") && contains(&buf, "write") && contains(&buf, "replaces the whole file") && contains(&buf, "+ line 1"), "{}", dump(&buf));
    assert!(contains(&buf, "more lines") && contains(&buf, "to enable allow"), "the whole diff must be read first\n{}", dump(&buf));
    s.now_ms += 1_000;
    assert!(keys(&mut s, "y").is_empty(), "allow is not enabled until the end has been on screen");
    keys(&mut s, "end");
    let reqs = keys(&mut s, "y");
    assert_eq!(reqs.len(), 1);
}

/// What the daemon sends for a tool call live: `ToolStarted.summary` is `host.describe(call)` and
/// `ToolFinished.summary` is the tool's one dim line, so a single line each. The arguments and the
/// result reach the client in the stored transcript, which it asks for when the turn ends.
fn stored(calls: &[(ToolCall, &str)]) -> Event {
    let mut turns = vec![StoredTurn { idx: 0, role: Role::User, content: vec![Part::Text { text: "fix the gate".into() }], at_ms: 1 }];
    for (i, (call, result)) in calls.iter().enumerate() {
        turns.push(StoredTurn { idx: 1 + 2 * i as u32, role: Role::Assistant, content: vec![Part::ToolCall(call.clone())], at_ms: 2 });
        turns.push(StoredTurn {
            idx: 2 + 2 * i as u32,
            role: Role::Tool,
            content: vec![Part::ToolResult(ToolResult { call_id: call.id.clone(), content: (*result).into(), is_error: false, parts: vec![] })],
            at_ms: 3,
        });
    }
    Event::Transcript { session: sid(), turns }
}

/// The turn ends; the client asks for the transcript; the daemon answers it.
fn end_turn_and_fetch(s: &mut marlowe_tui::State, seq: u64, calls: &[(ToolCall, &str)]) {
    let reqs = feed(s, seq, Event::TurnFinished { session: sid(), turn: 1, usage: usage(), stop: marlowe_proto::TurnStop::Done });
    assert!(reqs.contains(&Request::OpenSession { session: sid() }), "the turn is over: the details are asked for: {reqs:?}");
    feed(s, 0, Event::SessionOpened { session: sid(), title: "gate work".into(), workspace: Some("C:/work/valiance".into()) });
    feed(s, 0, stored(calls));
}

fn gate_edit() -> ToolCall {
    ToolCall { id: "c1".into(), name: "edit".into(), args: json!({"path": "src/gate.rs", "old_string": GATE_OLD, "new_string": GATE_NEW}) }
}

#[test]
fn parity_U1_an_edit_result_carries_its_diff_under_the_tool_line() {
    let mut s = busy_session();
    // Exactly what the daemon sends: one line each way, no arguments, no diff.
    feed(&mut s, 3, tool_started("c1", "edit", "edit src/gate.rs"));
    feed(&mut s, 4, tool_finished("c1", true, "edited src/gate.rs · exact"));
    let before = dump(&palette_frame(&s));
    assert!(before.contains("edit src/gate.rs") && !before.contains("x > 0.62"), "the live line alone has no diff\n{before}");
    let entries = s.entries.len();
    end_turn_and_fetch(&mut s, 5, &[(gate_edit(), "edited src/gate.rs · exact")]);
    let buf = palette_frame(&s);
    let text = dump(&buf);
    // The gutter carries the file's line numbers, so match the sign and the text after it.
    for want in ["edit src/gate.rs", "src/gate.rs  +1 −1", "−     x > 0.5", "+     x > 0.62", "fn gate(x: f32) -> bool {"] {
        assert!(text.contains(want), "{want:?}\n{text}");
    }
    snapshot("inline_diff", &buf);
    // The conversation was not rebuilt, and the tool line is still one line.
    assert_eq!(s.entries.len(), entries + 1, "only the usage line was added");
    assert_eq!(s.entries.iter().filter(|e| matches!(e, Entry::Tool { .. })).count(), 1);
}

#[test]
fn parity_U1_a_long_diff_is_capped_with_the_way_to_the_rest() {
    let mut s = busy_session();
    let body: String = (1..=60).map(|i| format!("added {i}\n")).collect();
    let call = ToolCall { id: "c1".into(), name: "write".into(), args: json!({"path": "big.txt", "content": body}) };
    feed(&mut s, 3, tool_started("c1", "write", "write big.txt"));
    feed(&mut s, 4, tool_finished("c1", true, "60 lines"));
    end_turn_and_fetch(&mut s, 5, &[(call, "60 lines")]);
    let buf = palette_frame(&s);
    assert!(contains(&buf, "more rows") && contains(&buf, "^o shows all"), "{}", dump(&buf));
    // The transcript view has them all.
    keys(&mut s, "ctrl-o");
    keys(&mut s, "home");
    let top = palette_frame(&s);
    assert!(contains(&top, "added 1") && contains(&top, "more"), "scrolled to the start says how much follows\n{}", dump(&top));
    keys(&mut s, "end");
    assert!(contains(&palette_frame(&s), "added 60"));
}

#[test]
fn parity_U1_a_diff_the_daemon_never_sent_is_fetched_when_ctrl_o_opens_between_turns() {
    let mut s = coding_session();
    // A call the client only has a line for (a reconnect, a call from before the client joined).
    s.entries.push(Entry::Tool { call_id: "c1".into(), summary: "edit src/gate.rs".into(), status: marlowe_tui::state::ToolStatus::Done("edited".into()) });
    let reqs = keys(&mut s, "ctrl-o");
    assert!(reqs.contains(&Request::OpenSession { session: sid() }), "{reqs:?}");
    feed(&mut s, 0, stored(&[(gate_edit(), "edited")]));
    assert!(contains(&palette_frame(&s), "x > 0.62"), "{}", dump(&palette_frame(&s)));
    // Once the details are here, opening it again asks for nothing.
    keys(&mut s, "ctrl-o");
    assert!(!keys(&mut s, "ctrl-o").contains(&Request::OpenSession { session: sid() }));
}

#[test]
fn parity_U1_a_turn_with_no_tool_calls_fetches_nothing() {
    let mut s = busy_session();
    feed(&mut s, 3, Event::TextDelta { session: sid(), turn: 1, text: "Done.".into() });
    let reqs = feed(&mut s, 4, Event::TurnFinished { session: sid(), turn: 1, usage: usage(), stop: marlowe_proto::TurnStop::Done });
    assert!(!reqs.iter().any(|r| matches!(r, Request::OpenSession { .. })), "{reqs:?}");
}

#[test]
fn parity_U1_a_replayed_session_shows_the_diff_of_its_edits() {
    let mut s = coding_session();
    let call = ToolCall { id: "t1".into(), name: "edit".into(), args: json!({"path": "src/gate.rs", "old_string": GATE_OLD, "new_string": GATE_NEW}) };
    feed(
        &mut s,
        0,
        Event::Transcript {
            session: sid(),
            turns: vec![
                StoredTurn { idx: 0, role: Role::User, content: vec![Part::Text { text: "raise the threshold".into() }], at_ms: 1 },
                StoredTurn { idx: 1, role: Role::Assistant, content: vec![Part::ToolCall(call)], at_ms: 2 },
                StoredTurn {
                    idx: 2,
                    role: Role::Tool,
                    content: vec![Part::ToolResult(ToolResult { call_id: "t1".into(), content: "edited src/gate.rs".into(), is_error: false, parts: vec![] })],
                    at_ms: 3,
                },
            ],
        },
    );
    let text = dump(&palette_frame(&s));
    assert!(text.contains("+     x > 0.62") && text.contains("−     x > 0.5"), "{text}");
}

// ---- U2 --------------------------------------------------------------------------------------------

fn reply_with(s: &mut marlowe_tui::State, text: &str) {
    feed(s, 9, Event::TextDelta { session: sid(), turn: 1, text: text.into() });
}

fn plan_of(s: &marlowe_tui::State) -> Vec<LinkRegion> {
    render(s, W, H);
    s.code.link_plan.borrow().clone()
}

#[test]
fn parity_U2_file_line_and_urls_in_the_conversation_become_osc_8_links() {
    let mut s = busy_session();
    reply_with(&mut s, "The failure is in src/main.rs:42 and the docs are at https://example.com/docs.");
    let plan = plan_of(&s);
    let urls: Vec<&str> = plan.iter().map(|r| r.url.as_str()).collect();
    assert_eq!(urls, ["file:///C:/work/valiance/src/main.rs", "https://example.com/docs"]);
    // The editor_url setting decides what a file opens in.
    feed(&mut s, 10, settings_event(&[(SettingsScope::User, "editor_url = \"vscode://file/{path}:{line}:{col}\"\n")]));
    let plan = plan_of(&s);
    assert_eq!(plan[0].url, "vscode://file/C:/work/valiance/src/main.rs:42:1");
    // The bytes the terminal gets: the visible cells wrapped in OSC 8, nothing else changed.
    let buf = render(&s, W, H);
    let bytes = String::from_utf8(LinkEmitter::default().frame(&buf, &plan)).unwrap();
    assert!(bytes.contains("\x1b]8;;vscode://file/C:/work/valiance/src/main.rs:42:1\x1b\\"), "{bytes:?}");
    assert!(bytes.contains("src/main.rs:42"), "{bytes:?}");
    assert!(bytes.contains("\x1b]8;;https://example.com/docs\x1b\\"), "{bytes:?}");
    assert_eq!(bytes.matches("\x1b]8;;\x1b\\").count(), 2, "every link is closed");
    // The buffer itself holds plain text: tests that read cells never see an escape.
    assert!(!dump(&buf).contains('\x1b'));
}

#[test]
fn parity_U2_a_path_on_a_tool_line_and_in_a_diff_header_is_a_link_too() {
    let mut s = busy_session();
    feed(&mut s, 3, tool_started("c1", "edit", "edit src/gate.rs"));
    feed(&mut s, 4, tool_finished("c1", true, "edited src/gate.rs · exact"));
    end_turn_and_fetch(&mut s, 5, &[(gate_edit(), "edited src/gate.rs · exact")]);
    let plan = plan_of(&s);
    assert!(plan.len() >= 2, "{plan:?}");
    assert!(plan.iter().all(|r| r.url == "file:///C:/work/valiance/src/gate.rs"));
}

#[test]
fn parity_U2_prose_that_looks_like_a_path_is_left_alone() {
    let mut s = busy_session();
    reply_with(&mut s, "e.g. the ratio is 3.14, v1.2.3 shipped, and/or maybe.");
    assert!(plan_of(&s).is_empty());
}

// ---- U4 --------------------------------------------------------------------------------------------

#[test]
fn parity_U4_the_status_line_command_runs_with_the_session_as_json_and_its_line_is_shown() {
    let mut s = coding_session();
    s.on_idle();
    assert!(s.take_effects().is_empty(), "no command is set");
    feed(&mut s, 0, settings_event(&[(SettingsScope::User, "[statusline]\ncommand = \"echo STATUS-OK-42\"\n")]));
    s.on_idle();
    let effects = s.take_effects();
    assert_eq!(effects.len(), 1, "{effects:?}");
    let Effect::Statusline { command, input, cwd } = effects[0].clone() else { panic!("{effects:?}") };
    assert_eq!(command, "echo STATUS-OK-42");
    assert_eq!(cwd.as_deref(), Some("C:/work/valiance"));
    let v: serde_json::Value = serde_json::from_str(&input).unwrap();
    assert_eq!(v["session_id"], "5e55");
    assert_eq!(v["model"]["display_name"], "Claude Sonnet 4.5");
    assert_eq!(v["workspace"]["current_dir"], "C:/work/valiance");
    assert_eq!(v["mode"], "default");
    // Run it for real, the way the run loop does, and feed the answer back.
    let ev = marlowe_tui::local::run(effects[0].clone()).expect("a worker's effect");
    assert_eq!(ev, LocalEvent::Statusline(Ok("STATUS-OK-42".into())));
    s.on_client(ClientEvent::Local(ev));
    let buf = palette_frame(&s);
    let bottom = dump(&buf).lines().find(|l| l.contains("(i)")).map(str::to_string).expect("the message box's bottom border");
    assert!(bottom.contains("STATUS-OK-42"), "{bottom}");
    snapshot("statusline_custom", &buf);
    // An unchanged input does not run it again; a changed one does, after the debounce.
    s.on_idle();
    assert!(s.take_effects().is_empty());
    keys(&mut s, "shift-tab");
    s.now_ms += 400;
    s.on_idle();
    let again = s.take_effects();
    assert_eq!(again.len(), 1);
    let Effect::Statusline { input, .. } = &again[0] else { panic!() };
    assert!(input.contains("accept_edits"));
}

#[test]
fn parity_U4_without_a_command_the_built_in_line_shows_spend_and_jobs() {
    let mut s = busy_session();
    feed(&mut s, 3, Event::TurnFinished { session: sid(), turn: 1, usage: usage(), stop: marlowe_proto::TurnStop::Done });
    let buf = palette_frame(&s);
    let bottom = dump(&buf).lines().find(|l| l.contains("(i)")).map(str::to_string).unwrap();
    assert!(bottom.contains("$0.0496"), "{bottom}");
}

#[test]
fn parity_U4_a_status_line_colours_are_removed_and_a_failing_command_shows_nothing() {
    use marlowe_tui::local::strip_ansi;
    assert_eq!(strip_ansi("\u{1b}[38;2;255;0;0mred\u{1b}[0m text"), "red text");
    let mut s = coding_session();
    feed(&mut s, 0, settings_event(&[(SettingsScope::User, "[statusline]\ncommand = \"false\"\n")]));
    s.on_idle();
    s.take_effects();
    s.on_client(ClientEvent::Local(LocalEvent::Statusline(Err("exited 1".into()))));
    let bottom = dump(&palette_frame(&s)).lines().find(|l| l.contains("(i)")).map(str::to_string).unwrap();
    assert!(!bottom.contains("exited"), "a broken command is quiet: {bottom}");
}

// ---- U11 -------------------------------------------------------------------------------------------

#[test]
fn parity_U11_ctrl_o_shows_every_tool_call_in_full_and_ctrl_o_leaves() {
    let mut s = busy_session();
    feed(&mut s, 3, Event::ReasoningDelta { session: sid(), turn: 1, text: "The threshold is compared to a float.".into(), tokens: 8 });
    feed(&mut s, 4, tool_started("c1", "bash", "bash cargo test -p gate"));
    let out: String = (1..=20).map(|i| format!("test case_{i} ... ok\n")).collect();
    // The daemon's line for a finished call: short, no output.
    feed(&mut s, 5, tool_finished("c1", true, "exit 0"));
    feed(&mut s, 6, Event::TextDelta { session: sid(), turn: 1, text: "All thirty pass.".into() });
    let inline = palette_frame(&s);
    assert!(!contains(&inline, "case_17"), "the conversation shows the one line");
    assert!(!contains(&inline, "The threshold is compared"), "thinking is collapsed");
    // Mid-turn the stored transcript has not got this turn yet: the view shows what the client has.
    keys(&mut s, "ctrl-o");
    let live = palette_frame(&s);
    assert!(contains(&live, "bash cargo test -p gate") && contains(&live, "exit 0") && !contains(&live, "case_20"), "{}", dump(&live));
    keys(&mut s, "ctrl-o");
    // The turn ends; the arguments and the output come from the stored transcript.
    let call = ToolCall { id: "c1".into(), name: "bash".into(), args: json!({"command": "cargo test -p gate"}) };
    end_turn_and_fetch(&mut s, 7, &[(call, &format!("20 passed\n{out}"))]);
    keys(&mut s, "ctrl-o");
    let buf = palette_frame(&s);
    for want in ["transcript", "The threshold is compared to a float.", "bash cargo test -p gate", "\"command\": \"cargo test -p gate\"", "case_20 ... ok", "All thirty pass."] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    snapshot("transcript_view", &buf);
    keys(&mut s, "ctrl-o");
    assert_eq!(s.overlay, None);
    assert!(!contains(&palette_frame(&s), "case_17"));
}

#[test]
fn parity_U11_a_replayed_session_keeps_its_tool_arguments_and_results() {
    let mut s = coding_session();
    let call = ToolCall { id: "t1".into(), name: "grep".into(), args: json!({"pattern": "threshold", "path": "src"}) };
    feed(
        &mut s,
        0,
        Event::Transcript {
            session: sid(),
            turns: vec![
                StoredTurn { idx: 0, role: Role::User, content: vec![Part::Text { text: "find it".into() }], at_ms: 1 },
                StoredTurn { idx: 1, role: Role::Assistant, content: vec![Part::ToolCall(call)], at_ms: 2 },
                StoredTurn {
                    idx: 2,
                    role: Role::Tool,
                    content: vec![Part::ToolResult(ToolResult { call_id: "t1".into(), content: "src/gate.rs:10: threshold\nsrc/eval.rs:3: threshold".into(), is_error: false, parts: vec![] })],
                    at_ms: 3,
                },
            ],
        },
    );
    keys(&mut s, "ctrl-o");
    let buf = palette_frame(&s);
    for want in ["grep threshold", "\"pattern\": \"threshold\"", "src/gate.rs:10: threshold", "src/eval.rs:3: threshold"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
}

// ---- the build DAG strip ---------------------------------------------------------------------------

const DAG_REPORT: &str = r#"dag {"stage":"implement","hubs":["Cargo.toml","src/lib.rs"],"tasks":[
    {"id":"models","goal":"models","deps":[],"state":"done"},
    {"id":"schema","goal":"schema","deps":[],"state":"done"},
    {"id":"api","goal":"routes","deps":["models","schema"],"state":"running"},
    {"id":"ui","goal":"pages","deps":["api"],"state":"waiting"}]}"#;

fn build_run() -> CrewRunInfo {
    CrewRunInfo {
        id: RunId::from("bb11cc22"),
        session: Some(sid()),
        role: "lead".into(),
        title: "add the evals page".into(),
        goal: "add the evals page".into(),
        playbook: Some(Playbook::Build),
        status: CrewRunStatus::Running,
        workspace: Some("C:/work/valiance".into()),
        sandbox: None,
        budget: CrewBudget { tokens: Some(400_000), steps: Some(40), usd: Some(0.5) },
        spent: CrewSpent { tokens: 41_200, steps: 6, usd: 0.0213 },
        step: 6,
        current_action: None,
        note: None,
        tainted: false,
        created_ms: 1_800_000_000_000 - 20 * 60_000,
        updated_ms: 1_800_000_000_000 - 60_000,
        result: None,
    }
}

#[test]
fn the_build_dag_strip_shows_the_stage_and_the_task_graph_in_the_run_view() {
    let mut s = coding_session();
    feed(&mut s, 3, Event::Runs { list: vec![build_run()] });
    keys(&mut s, "alt-r");
    assert_eq!(s.view, View::Crew);
    keys(&mut s, "enter");
    feed(&mut s, 4, Event::RunStep { run: RunId::from("bb11cc22"), step: 6, action: DAG_REPORT.replace('\n', " "), text_delta: None });
    let buf = palette_frame(&s);
    let text = dump(&buf);
    for want in ["scope", "contract", "● implement", "integrate", "2 of 4 tasks done", "✓ models", "✓ schema", "● api ← models,schema", "○ ui ← api", "frozen after the contract: Cargo.toml, src/lib.rs"] {
        assert!(text.contains(want), "{want:?}\n{text}");
    }
    snapshot("build_dag_strip", &buf);
    // The report is the strip's, not a line in the transcript.
    assert!(!text.contains("dag {"));
    // The rail shows the stage too.
    assert_eq!(s.crew.get(&RunId::from("bb11cc22")).and_then(|r| r.current_action.clone()).as_deref(), Some("build · implement · 2 of 4 tasks done"));
    // A later report replaces it.
    feed(&mut s, 5, Event::RunStep { run: RunId::from("bb11cc22"), step: 7, action: DAG_REPORT.replace("running", "done").replace("\"waiting\"", "\"serial\"").replace('\n', " "), text_delta: None });
    let text = dump(&palette_frame(&s));
    assert!(text.contains("3 of 4 tasks done") && text.contains("↺ ui"), "{text}");
}

// ---- the palette is locked -------------------------------------------------------------------------

#[test]
fn the_palette_is_locked() {
    let th = theme();
    let rgb = |c: Color| match c {
        Color::Rgb(r, g, b) => (r, g, b),
        other => panic!("{other:?} is not an RGB colour"),
    };
    assert_eq!(rgb(th.accent()), (0x9B, 0x7E, 0xDE), "accent #9B7EDE");
    assert_eq!(rgb(Ink::Green.color(&th)), (0x8F, 0xD4, 0xA8), "green #8fd4a8");
    assert_eq!(rgb(Ink::Amber.color(&th)), (0xD6, 0xA9, 0x5F), "amber #d6a95f");
    assert_eq!(rgb(Ink::Red.color(&th)), (0xDD, 0x7F, 0x85), "red #dd7f85");
    assert_eq!(rgb(th.dim_color()), (0x6A, 0x64, 0x80), "weight 2 #6a6480");
    assert_eq!(rgb(th.dimmer_color()), (0x4A, 0x44, 0x60), "weight 3 #4a4460");
    // Speech is the accent tinted toward white: within a shade of the mockup's #b39ae8.
    let (r, g, b) = rgb(th.speech());
    assert!(r.abs_diff(0xB3) < 10 && g.abs_diff(0x9A) < 12 && b.abs_diff(0xE8) < 4, "speech {r:02x}{g:02x}{b:02x}");
    // Structure and hover are the accent scaled, never a colour of their own.
    let (ar, ag, ab) = rgb(th.accent());
    for (name, c, f) in [("structure", th.structure(), 0.30f32), ("hover", th.hover_color(), 0.72), ("structure_dim", th.structure_dim(), 0.18)] {
        let (r, g, b) = rgb(c);
        for (got, base) in [(r, ar), (g, ag), (b, ab)] {
            assert!((f32::from(got) - f32::from(base) * f).abs() <= 1.5, "{name}: {got} vs {base} x {f}");
        }
    }
}

/// No source in this crate paints a background, reverses video, or names a colour of its own.
#[test]
fn nothing_in_the_client_reaches_past_the_theme() {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    walk(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
    assert!(files.len() > 30);
    let mut offenders = Vec::new();
    for f in files {
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        let text = std::fs::read_to_string(&f).unwrap();
        // Only the code that ships: the unit tests at the bottom may name any colour they like.
        let shipped = text.lines().position(|l| l.contains("#[cfg(test)]")).unwrap_or(usize::MAX);
        for (i, l) in text.lines().enumerate().take_while(|(i, _)| *i < shipped) {
            let code = l.split("//").next().unwrap_or("");
            // links.rs reads cell colours to repeat them as escape codes; it never paints one, so
            // the colour names it matches on are not a palette breach. Fills and reverse video are
            // checked there as everywhere.
            // A match arm (`Color::Rgb(r, g, b) => ...`) reads a themed colour back; it paints nothing.
            let reads_only = name == "links.rs" || code.contains("=>");
            let bad = code.contains(".bg(")
                || code.contains("Modifier::REVERSED")
                || (!reads_only
                    && (code.contains("Color::Indexed(")
                        || code.contains("Color::Rgb(")
                        || ["Color::Blue", "Color::LightBlue", "Color::Cyan", "Color::Magenta", "Color::Yellow", "Color::White", "Color::Black"].iter().any(|c| code.contains(c))));
            if bad {
                offenders.push(format!("{name}:{}: {}", i + 1, l.trim()));
            }
        }
    }
    assert!(offenders.is_empty(), "{offenders:#?}");
}

/// Every new screen draws only palette colours and no fill.
#[test]
fn every_coding_screen_stays_inside_the_palette() {
    let mut s = busy_session();
    feed(&mut s, 3, tool_started("c1", "edit", "edit src/gate.rs"));
    feed(&mut s, 4, tool_finished("c1", true, "edited src/gate.rs · exact"));
    end_turn_and_fetch(&mut s, 6, &[(gate_edit(), "edited src/gate.rs · exact")]);
    feed(&mut s, 7, Event::Todos { session: sid(), items: vec![marlowe_proto::TodoItem { content: "x".into(), status: marlowe_proto::TodoStatus::InProgress, active_form: None }] });
    keys(&mut s, "'one' enter");
    palette_frame(&s);
    for cmd in ["/help", "/config", "/permissions", "/hooks", "/agents", "/mcp", "/memory", "/context", "/cost", "/diff", "/rewind", "/jobs"] {
        s.composer.clear();
        keys(&mut s, &format!("'{cmd}' enter"));
        palette_frame(&s);
        keys(&mut s, "esc");
    }
    keys(&mut s, "ctrl-o");
    palette_frame(&s);
    keys(&mut s, "ctrl-o ctrl-r");
    palette_frame(&s);
    keys(&mut s, "esc '@'");
    palette_frame(&s);
    keys(&mut s, "ctrl-u '/'");
    palette_frame(&s);
}
