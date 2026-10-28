//! The work block and the lines outside it, replayed from tonight's recorded research runs
//! (`tests/fixtures/work`: r1 sent its report and finished a project, r5 made two project calls
//! between two thoughts), exactly as the daemon's events would have drawn them.
//!
//! Each recorded row becomes the events that made it: a thought is streamed in three deltas between
//! the row before it and its own time, a call is announced (arguments streaming), started and
//! finished at the times the rows were written. The one composed moment is the parallel one: r5's
//! two calls are started before either finishes.

mod common;

use common::*;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use marlowe_model::{Part, Role, Usage};
use marlowe_proto::{Envelope, Event, Request, SentFile, SessionId, StoredTurn, TurnStop};
use marlowe_surface::chrome::Ink;
use marlowe_tui::coding::Effect;
use marlowe_tui::panel::Panel;
use marlowe_tui::state::Entry;
use marlowe_tui::{ClientEvent, State};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use serde_json::Value;

struct Rec {
    sid: SessionId,
    turns: Vec<StoredTurn>,
}

fn fixture_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join("work")
}

fn recorded(run: &str) -> Rec {
    let text = std::fs::read_to_string(fixture_dir().join(format!("{run}.json"))).expect("fixture");
    let v: Value = serde_json::from_str(&text).expect("fixture json");
    Rec { sid: SessionId::from(v["session"].as_str().unwrap()), turns: serde_json::from_value(v["turns"].clone()).expect("stored turns") }
}

/// A client with the recorded conversation's session open and nothing in it yet.
fn client(rec: &Rec) -> State {
    let mut s = state();
    s.date = "Wednesday 28 October".into();
    welcome(&mut s, 0);
    feed(&mut s, 1, library_with_sonnet());
    feed(&mut s, 0, Event::SessionOpened { session: rec.sid.clone(), title: "tonight".into(), workspace: Some("C:/work/valiance".into()) });
    s.session = Some(rec.sid.clone());
    s.title = Some("tonight".into());
    s
}

/// The next event of the stream, at `rec_at` ms on the recorded clock.
fn at(s: &mut State, rec_at: i64, event: Event) {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(100);
    let seq = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    s.now_ms = rec_at;
    s.on_client(ClientEvent::Envelope(Envelope { seq, at_ms: rec_at, event }));
}

fn text_of(t: &StoredTurn) -> String {
    t.content.iter().filter_map(|p| if let Part::Text { text } = p { Some(text.as_str()) } else { None }).collect::<Vec<_>>().join("\n")
}

/// How far to replay a turn: up to a moment inside it.
#[derive(Clone, Copy, PartialEq)]
enum Until {
    /// The model is streaming the arguments of the call in row `idx`.
    Streaming(u32),
    /// The call in row `idx` has started and not finished.
    Started(u32),
    /// Row `idx` has been written.
    Row(u32),
    /// The turn is over.
    End,
}

/// The events of rows `from..` of the recorded turn, as far as `until`. Returns the clock at the end.
fn replay(s: &mut State, rec: &Rec, from: usize, until: Until) -> i64 {
    let mut clock = 0;
    for (i, t) in rec.turns.iter().enumerate().skip(from) {
        let prev = if i > 0 { rec.turns[i - 1].at_ms } else { t.at_ms };
        match t.role {
            Role::User if i == from => {
                at(s, t.at_ms, Event::TurnStarted { session: rec.sid.clone(), turn: 1, model: sonnet_ref(), user: text_of(t) });
            }
            Role::User => break,
            Role::Assistant => {
                for (k, p) in t.content.iter().enumerate() {
                    match p {
                        Part::Thinking(th) => {
                            // Three deltas between the row before this one and this one's own time.
                            let chars: Vec<char> = th.text.chars().collect();
                            let third = chars.len().div_ceil(3).max(1);
                            let steps = chars.len().div_ceil(third) as i64;
                            let (a, b) = (prev + 150, t.at_ms - 5);
                            for (k, part) in chars.chunks(third).enumerate() {
                                let when = a + (b - a) * (k as i64 + 1) / steps;
                                let tokens = part.len() as u64 / 4;
                                at(s, when, Event::ReasoningDelta { session: rec.sid.clone(), turn: 1, text: part.iter().collect(), tokens });
                            }
                        }
                        // Text the model wrote before it thought and called came out first.
                        Part::Text { text } => {
                            let when = if k == 0 { prev + 100 } else { t.at_ms };
                            at(s, when, Event::TextDelta { session: rec.sid.clone(), turn: 1, text: text.clone() })
                        }
                        Part::ToolCall(c) => {
                            at(s, t.at_ms - 4, Event::ToolPreparing { session: rec.sid.clone(), turn: 1, call_id: c.id.clone(), tool: c.name.clone() });
                            if until == Until::Streaming(t.idx) {
                                return t.at_ms - 4;
                            }
                            let first = c.args.as_object().and_then(|o| o.values().find_map(|v| v.as_str())).unwrap_or("");
                            let summary = format!("{} {}", c.name, first.lines().next().unwrap_or("").chars().take(60).collect::<String>());
                            at(s, t.at_ms, Event::ToolStarted { session: rec.sid.clone(), turn: 1, call_id: c.id.clone(), tool: c.name.clone(), summary });
                            if until == Until::Started(t.idx) {
                                return t.at_ms;
                            }
                        }
                        _ => {}
                    }
                }
            }
            Role::Tool => {
                for p in &t.content {
                    let Part::ToolResult(r) = p else { continue };
                    let sent = SentFile::from_tool_result(&r.content);
                    let summary = match &sent {
                        Some(f) => format!("sent {}", f.name),
                        None => r.content.lines().next().unwrap_or("").chars().take(60).collect(),
                    };
                    if let Some(f) = sent {
                        let file = SentFile { digest: "00".repeat(32), ..f };
                        at(s, t.at_ms - 1, Event::FileSent { session: rec.sid.clone(), file });
                    }
                    at(s, t.at_ms, Event::ToolFinished { session: rec.sid.clone(), turn: 1, call_id: r.call_id.clone(), ok: !r.is_error, summary });
                }
            }
            Role::System => {}
        }
        clock = t.at_ms;
        if until == Until::Row(t.idx) {
            return clock;
        }
    }
    let usage = Usage { input: 7_600, output: 420, reasoning: 380, ..Usage::default() };
    at(s, clock + 20, Event::TurnFinished { session: rec.sid.clone(), turn: 1, usage, stop: TurnStop::Done });
    clock + 20
}

fn shot(name: &str, s: &State) -> Buffer {
    let buf = render(s, 120, 30);
    assert_palette_only(&buf);
    snapshot(&format!("{name}_120x30"), &buf);
    buf
}

/// Where `needle` starts on screen (column, row).
fn find(buf: &Buffer, needle: &str) -> (u16, u16) {
    for y in buf.area.top()..buf.area.bottom() {
        let line: String = (buf.area.left()..buf.area.right()).map(|x| buf.cell((x, y)).map_or(" ", |c| c.symbol())).collect();
        if let Some(i) = line.find(needle) {
            return (line[..i].chars().count() as u16, y);
        }
    }
    panic!("{needle:?} is not on screen:\n{}", dump(buf));
}

fn ink_at(buf: &Buffer, needle: &str) -> ratatui::style::Color {
    let (x, y) = find(buf, needle);
    buf.cell((x, y)).unwrap().fg
}

fn mouse(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
    MouseEvent { kind, column: x, row: y, modifiers: KeyModifiers::NONE }
}

fn click_on(s: &mut State, buf: &Buffer, needle: &str) -> Vec<Request> {
    let (x, y) = find(buf, needle);
    s.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), x, y), Rect::new(0, 0, buf.area.width, buf.area.height))
}

/// r5's turn up to a moment in it, the clock `after` ms later.
fn r5_at(until: Until, after: i64) -> State {
    let rec = recorded("r5");
    let mut s = client(&rec);
    let clock = replay(&mut s, &rec, 0, until);
    s.now_ms = clock + after;
    s
}

// ---- the ticker ----------------------------------------------------------------------------------

#[test]
fn the_ticker_streams_a_call_amber_runs_it_blue_and_settles_green_or_red() {
    let th = theme();
    // The model is still writing the second call's arguments.
    let s = r5_at(Until::Streaming(3), 1_000);
    let buf = shot("work_streaming", &s);
    assert!(contains(&buf, "▸ thinking · 7.0s · 2 tools") && contains(&buf, "└ manage_project   streaming"), "{}", dump(&buf));
    assert_eq!(ink_at(&buf, "streaming"), Ink::Amber.color(&th));

    // The call has started: blue, with the time it has run.
    let s = r5_at(Until::Started(3), 2_000);
    let buf = shot("work_running", &s);
    assert!(contains(&buf, "└ manage_project  create   running · 2.0s"), "{}", dump(&buf));
    assert_eq!(ink_at(&buf, "running · 2.0s"), Ink::Info.color(&th));
    assert_eq!(ink_at(&buf, "▸ thinking"), Ink::Hover.color(&th), "the live header is lit");

    // The first call failed, the second worked: each in its colour.
    let s = r5_at(Until::Row(2), 0);
    let buf = render(&s, 120, 30);
    assert!(contains(&buf, "└ manage_project  create   ✗ plan.fields[0].values: need"), "{}", dump(&buf));
    assert_eq!(ink_at(&buf, "✗ plan"), Ink::Red.color(&th));
    let s = r5_at(Until::Row(4), 0);
    let buf = render(&s, 120, 30);
    assert_eq!(ink_at(&buf, "✓ Project"), Ink::Green.color(&th));
}

#[test]
fn calls_that_overlap_read_as_one_row() {
    let rec = recorded("r5");
    let mut s = client(&rec);
    let (t1, t3) = (&rec.turns[1], &rec.turns[3]);
    at(&mut s, rec.turns[0].at_ms, Event::TurnStarted { session: rec.sid.clone(), turn: 1, model: sonnet_ref(), user: text_of(&rec.turns[0]) });
    // Both of the recorded calls begin before either has an answer.
    for t in [t1, t3] {
        let Part::ToolCall(c) = t.content.iter().find(|p| matches!(p, Part::ToolCall(_))).unwrap() else { unreachable!() };
        at(&mut s, t.at_ms, Event::ToolStarted { session: rec.sid.clone(), turn: 1, call_id: c.id.clone(), tool: c.name.clone(), summary: "manage_project create".into() });
    }
    s.now_ms = t3.at_ms + 3_000;
    let buf = shot("work_parallel", &s);
    assert!(contains(&buf, "2 running · manage_project ×2"), "{}", dump(&buf));
    assert_eq!(ink_at(&buf, "2 running"), Ink::Info.color(&theme()));
    // The first answer: one is left running, and the row goes back to naming it.
    let Part::ToolResult(r) = &rec.turns[2].content[0] else { unreachable!() };
    at(&mut s, rec.turns[2].at_ms, Event::ToolFinished { session: rec.sid.clone(), turn: 1, call_id: r.call_id.clone(), ok: !r.is_error, summary: "plan rejected".into() });
    s.now_ms = t3.at_ms + 3_000;
    let again = render(&s, 120, 30);
    assert!(contains(&again, "└ manage_project  create   running"), "{}", dump(&again));
}

#[test]
fn a_settled_turn_is_two_rows_and_opens_to_the_whole_narrative() {
    let rec = recorded("r5");
    let mut s = client(&rec);
    replay(&mut s, &rec, 0, Until::End);
    let buf = shot("work_settled", &s);
    assert!(contains(&buf, "▸ thought 7.3s · 2 tools"), "{}", dump(&buf));
    assert!(contains(&buf, "└ manage_project  create   ✓ Project p_"), "{}", dump(&buf));
    // The answer stays below the block; the reasoning is folded.
    assert!(contains(&buf, "Project created to compile a comprehensive report"));
    assert!(!contains(&buf, "The user is asking for a comprehensive web research project"), "the reasoning is folded");

    // A click on the header opens it; the narrative is in order: thought, call, thought, call.
    click_on(&mut s, &buf, "▸ thought");
    let tall = render(&s, 120, 60);
    assert_palette_only(&tall);
    let text = dump(&tall);
    let first = text.find("The user is asking for a comprehensive web research project").expect(&text);
    let call1 = text.find("› manage_project  create   ✗ plan.fields[0]").expect(&text);
    let second = text.find("Need to reissue manage_project").expect(&text);
    let call2 = text.find("› manage_project  create   ✓ Project p_").expect(&text);
    assert!(first < call1 && call1 < second && second < call2, "{text}");
    assert!(text.contains("▏ 4.6s · 93 tok · 20 tok/s"), "each thought keeps its numbers\n{text}");
    assert!(text.contains("▾ thought 7.3s · 2 tools"));
    shot("work_expanded", &s);

    // Again, and it folds; ^t opens every block at once.
    let tall = render(&s, 120, 60);
    click_on(&mut s, &tall, "▾ thought");
    assert!(contains(&render(&s, 120, 30), "▸ thought 7.3s"));
    s.on_key(ctrl('t'));
    assert!(contains(&render(&s, 120, 60), "▾ thought 7.3s"));
}

#[test]
fn the_pointer_lights_a_call_and_a_click_opens_its_output() {
    let rec = recorded("r5");
    let mut s = client(&rec);
    replay(&mut s, &rec, 0, Until::End);
    s.on_key(ctrl('t'));
    let area = Rect::new(0, 0, 120, 60);
    let buf = render(&s, 120, 60);
    let th = theme();
    // Hovering a call lights its row; the marker goes to the hover colour.
    let (x, y) = find(&buf, "› manage_project  create   ✓ Project p_");
    assert_eq!(ink_at(&buf, "› manage_project  create   ✓ Project p_"), Ink::Accent.color(&th));
    s.on_mouse(mouse(MouseEventKind::Moved, x + 3, y), area);
    let lit = render(&s, 120, 60);
    assert_eq!(ink_at(&lit, "› manage_project  create   ✓ Project p_"), Ink::Hover.color(&th));
    assert_palette_only(&lit);
    s.on_mouse(mouse(MouseEventKind::Moved, 2, 0), area);
    // A click opens the call in full: its arguments, then its output.
    s.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), x + 3, y), area);
    let Some(Panel::Call(c)) = s.code.panel.clone() else { panic!("a call panel: {:?}", s.code.panel) };
    let Part::ToolCall(call) = rec.turns[3].content.iter().find(|p| matches!(p, Part::ToolCall(_))).unwrap() else { unreachable!() };
    assert_eq!(c.call_id, call.id);
    let shown = dump(&render(&s, 120, 40));
    assert!(shown.contains("call · manage_project create") && shown.contains("Project p_"), "{shown}");
}

#[test]
fn an_empty_call_id_from_the_provider_still_finds_its_line_when_the_call_starts() {
    let rec = recorded("r5");
    let mut s = client(&rec);
    at(&mut s, 1, Event::TurnStarted { session: rec.sid.clone(), turn: 1, model: sonnet_ref(), user: "go".into() });
    at(&mut s, 2, Event::ToolPreparing { session: rec.sid.clone(), turn: 1, call_id: String::new(), tool: "bash".into() });
    at(&mut s, 3, Event::ToolPreparing { session: rec.sid.clone(), turn: 1, call_id: String::new(), tool: "read".into() });
    let calls = |s: &State| s.entries.iter().filter(|e| matches!(e, Entry::Tool { .. })).count();
    assert_eq!(calls(&s), 2);
    at(&mut s, 4, Event::ToolStarted { session: rec.sid.clone(), turn: 1, call_id: "c9".into(), tool: "read".into(), summary: "read a.rs".into() });
    at(&mut s, 5, Event::ToolStarted { session: rec.sid.clone(), turn: 1, call_id: "c8".into(), tool: "bash".into(), summary: "bash ls".into() });
    assert_eq!(calls(&s), 2, "the named calls took the places of the unnamed ones");
    assert!(s.code.preparing.is_empty());
    let summaries: Vec<String> = s.entries.iter().filter_map(|e| if let Entry::Tool { summary, .. } = e { Some(summary.clone()) } else { None }).collect();
    assert_eq!(summaries, ["bash ls", "read a.rs"]);
    // A call that never starts (the turn ends first) does not stay amber.
    at(&mut s, 6, Event::ToolPreparing { session: rec.sid.clone(), turn: 1, call_id: "c7".into(), tool: "grep".into() });
    at(&mut s, 7, Event::TurnFinished { session: rec.sid.clone(), turn: 1, usage: Usage::default(), stop: TurnStop::Interrupted });
    assert!(s.code.preparing.is_empty());
}

#[test]
fn the_block_and_the_file_row_fit_every_width_in_the_palette() {
    let mut states = vec![r5_at(Until::Streaming(3), 1_000), r5_at(Until::Started(3), 2_000), r5_at(Until::End, 0), r1_sent().0];
    let mut open = r5_at(Until::End, 0);
    open.on_key(ctrl('t'));
    states.push(open);
    for s in &states {
        for (w, h) in [(40u16, 14u16), (60, 20), (80, 24), (120, 30), (200, 50)] {
            let buf = render(s, w, h);
            assert_palette_only(&buf);
        }
    }
}

// ---- outside the block --------------------------------------------------------------------------------

/// r1's closing turn: the report is sent. Live, the call's line becomes the file's.
fn r1_sent() -> (State, Rec) {
    let rec = recorded("r1");
    let mut s = client(&rec);
    replay(&mut s, &rec, 12, Until::End);
    (s, rec)
}

#[test]
fn a_sent_file_is_one_row_the_name_in_the_accent_and_the_path_at_the_right() {
    let (s, _) = r1_sent();
    let buf = shot("work_file_line", &s);
    let text = dump(&buf);
    let row = text.lines().find(|l| l.contains("report.md")).expect(&text);
    let pane: String = row.chars().skip(1).take(78).collect();
    assert!(pane.trim_start().starts_with("report.md "), "{pane:?}");
    assert!(pane.trim_end().ends_with("report.md)") && pane.contains("(…"), "the path is trimmed from the left to fit: {pane:?}");
    assert_eq!(ink_at(&buf, "report.md"), Ink::Accent.color(&theme()));
    assert_eq!(ink_at(&buf, "(…"), Ink::Dim.color(&theme()), "the path is grey");
    // The tool line and the description are gone; the file is counted once.
    assert!(!text.contains("send_file") && !text.contains("sent report.md") && !text.contains("13 KB") && !text.contains("/files ·"), "{text}");
    assert_eq!(s.entries.iter().filter(|e| matches!(e, Entry::File { .. })).count(), 1);
    assert_eq!(s.entries.iter().filter(|e| matches!(e, Entry::Tool { .. })).count(), 0);
}

#[test]
fn a_reopened_session_shows_the_sent_file_the_same_way() {
    let (live, rec) = r1_sent();
    let mut s = client(&rec);
    feed(&mut s, 5, Event::Transcript { session: rec.sid.clone(), turns: rec.turns.clone() });
    let file = |s: &State| s.entries.iter().find_map(|e| if let Entry::File { file } = e { Some((file.name.clone(), file.path.clone(), file.bytes)) } else { None });
    assert_eq!(file(&s), file(&live), "the transcript names the file the daemon announced");
    let row = |s: &State| dump(&render(s, 120, 30)).lines().find(|l| l.contains("report.md")).map(|l| l.chars().skip(1).take(78).collect::<String>());
    assert_eq!(row(&s), row(&live));
    assert!(!dump(&render(&s, 120, 30)).contains("send_file"));
}

#[test]
fn a_click_opens_a_markdown_file_in_the_reader_and_anything_else_with_the_system() {
    let (mut s, _) = r1_sent();
    // The recorded outbox is not on this machine: the reader needs the file, so this one is a copy.
    let dir = std::env::temp_dir().join(format!("marlowe-work-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let md = dir.join("report.md");
    std::fs::copy(fixture_dir().join("r1-report.md"), &md).unwrap();
    let sent = |name: &str, path: &std::path::Path| SentFile { name: name.into(), path: path.display().to_string(), bytes: 13_396, digest: "00".repeat(32), note: String::new() };
    s.entries.retain(|e| !matches!(e, Entry::File { .. }));
    let session = s.session.clone().unwrap();
    feed(&mut s, 900, Event::FileSent { session: session.clone(), file: sent("report.md", &md) });
    let buf = render(&s, 120, 30);
    let out = click_on(&mut s, &buf, "report.md");
    assert!(out.is_empty(), "{out:?}");
    let Some(Panel::Report(r)) = s.code.panel.clone() else { panic!("the reader: {:?}", s.code.panel) };
    assert_eq!(r.file.as_ref().map(|f| f.0.as_str()), Some(md.display().to_string().as_str()));
    let shown = render(&s, 120, 40);
    assert_palette_only(&shown);
    assert!(contains(&shown, "report.md") && contains(&shown, "Kimi"), "{}", dump(&shown));
    s.on_key(key(KeyCode::Esc));

    let png = dir.join("loss.png");
    feed(&mut s, 901, Event::FileSent { session, file: sent("loss.png", &png) });
    let buf = render(&s, 120, 30);
    let out = click_on(&mut s, &buf, "loss.png");
    assert_eq!(out, vec![Request::OpenFile { path: png.display().to_string(), reveal: false }]);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_file_row_has_the_keys_when_the_box_has_the_cursor() {
    let (mut s, _) = r1_sent();
    let path = s.entries.iter().find_map(|e| if let Entry::File { file } = e { Some(file.path.clone()) } else { None }).unwrap();
    s.on_key(key(KeyCode::Esc));
    // The border reads the marks of the frame before it.
    render(&s, 120, 30);
    let buf = render(&s, 120, 30);
    // The box says what the keys do, and the row they act on is lit.
    assert!(contains(&buf, "↵ open") && contains(&buf, "r reveal") && contains(&buf, "y copy path"), "{}", dump(&buf));
    assert_eq!(ink_at(&buf, "report.md"), Ink::Hover.color(&theme()));
    assert_palette_only(&buf);
    assert_eq!(s.on_key(key(KeyCode::Char('r'))), vec![Request::OpenFile { path: path.clone(), reveal: true }]);
    assert!(s.on_key(key(KeyCode::Char('y'))).is_empty());
    assert_eq!(s.take_effects(), vec![Effect::CopyText { text: path.clone() }]);
    // Enter opens a markdown file in the reader; when the file cannot be read here, the system gets it.
    let out = s.on_key(key(KeyCode::Enter));
    assert!(out == vec![Request::OpenFile { path, reveal: false }] || matches!(s.code.panel, Some(Panel::Report(_))), "{out:?}");
}

// ---- the end of a project ------------------------------------------------------------------------------

/// r1's project, as the daemon's state event gives it, with its report beside it.
fn give_project(s: &mut State, seq: u64) {
    let state: Value = serde_json::from_str(&std::fs::read_to_string(fixture_dir().join("r1-project.json")).unwrap()).unwrap();
    let id = state["id"].as_str().unwrap().to_string();
    let done = state["finished_ms"].as_i64().unwrap();
    s.projects.clock = Some(done as u64 + 2_000);
    feed(s, seq, Event::ProjectState { project: id.clone(), rev: 900, state });
    let report = std::fs::read_to_string(fixture_dir().join("r1-report.md")).unwrap();
    if let Some(l) = s.projects.loaded.iter_mut().find(|l| l.state.id == id) {
        if let Some(r) = &mut l.report {
            r.text = Some(report);
        }
    }
}

#[test]
fn a_project_that_ended_is_its_entry_and_one_file_row_and_no_relay_line() {
    let rec = recorded("r1");
    let mut s = client(&rec);
    feed(&mut s, 5, Event::Transcript { session: rec.sid.clone(), turns: rec.turns.clone() });
    s.now_ms = rec.turns[12].at_ms + 3_000;
    give_project(&mut s, 6);
    assert_eq!(s.entries.iter().filter(|e| matches!(e, Entry::Project { .. })).count(), 1);
    let buf = shot("work_project_end", &s);
    let text = dump(&buf);
    assert!(text.contains("✓ p_1ed1613c · research · finished"), "{text}");
    assert!(text.contains("read report"), "{text}");
    assert!(!text.contains("↳ project p_1ed1613c · finished"), "the entry says it already:\n{text}");
    // The agent's turn after it: its thought, the file, its one line. No second copy of the report.
    assert!(text.contains("report.md") && text.contains("Sent to you."), "{text}");
    // The button still opens the reader.
    let out = click_on(&mut s, &buf, "read report");
    assert!(out.is_empty());
    assert!(matches!(s.code.panel, Some(Panel::Report(_))));
}

// ---- a turn in the order it happened -------------------------------------------------------------

/// r5 with the text the model wrote before it thought and called, put first in its first step.
/// Everything else is the recording; only this line is composed.
fn r5_saying(first: &str) -> Rec {
    let mut rec = recorded("r5");
    rec.turns[1].content.insert(0, Part::Text { text: first.into() });
    rec
}

const LONG_START: &str = "I will set this up as a project: a plan card with the questions about Kimi, DeepSeek and Qwen4 comes first, and nothing is read until you approve it.";

fn row_of(buf: &Buffer, needle: &str) -> u16 {
    find(buf, needle).1
}

fn replayed(rec: &Rec) -> State {
    let mut s = client(rec);
    replay(&mut s, rec, 0, Until::End);
    s
}

#[test]
fn text_then_work_then_text_draws_in_that_order_and_the_later_text_is_its_own_paragraph() {
    let rec = r5_saying(LONG_START);
    let s = replayed(&rec);
    let buf = shot("work_text_block_text", &s);
    let text = dump(&buf);
    let (start, block, answer) = (row_of(&buf, "I will set this up as a project"), row_of(&buf, "▸ thought"), row_of(&buf, "Project created to compile"));
    assert!(start < block && block < answer, "text, then the block, then the answer:\n{text}");
    // A long first line is text, never a title; one blank row on each side of the block.
    assert!(!text.contains("▸ I will set this up"), "{text}");
    // The transcript's own pane: the left 80 columns.
    let rows: Vec<String> = text.lines().map(|l| l.chars().skip(1).take(78).collect::<String>()).collect();
    assert!(rows[(block - 1) as usize].trim().is_empty(), "a blank row above the block:
{text}");
    assert!(rows[(answer - 1) as usize].trim().is_empty() && answer == block + 3, "the answer is its own paragraph under the block:
{text}");
    // Both texts are Marlowe's prose, and neither carries a label of its own.
    let th = theme();
    assert_eq!(ink_at(&buf, "I will set this up"), Ink::Speech.color(&th));
    assert_eq!(ink_at(&buf, "Project created to compile"), Ink::Speech.color(&th));
}

#[test]
fn a_short_sentence_before_the_calls_is_the_blocks_title() {
    let rec = r5_saying("Gathering recent changes…");
    let s = replayed(&rec);
    let buf = shot("work_title", &s);
    let text = dump(&buf);
    assert!(text.contains("▸ Gathering recent changes · 7.3s · 2 tools"), "{text}");
    assert!(text.contains("└ manage_project  create   ✓ Project p_"), "the last call under it:\n{text}");
    assert_eq!(text.matches("Gathering recent changes").count(), 1, "once, as the title:\n{text}");
    let th = theme();
    assert_eq!(ink_at(&buf, "Gathering recent changes"), Ink::Speech.color(&th));
    assert_eq!(ink_at(&buf, "7.3s"), Ink::Dim.color(&th));
    // The answer is below the block.
    assert!(row_of(&buf, "Project created to compile") > row_of(&buf, "▸ Gathering"), "{text}");

    // Opened, the narrative is under the same title.
    let mut s = s;
    click_on(&mut s, &buf, "▸ Gathering");
    let open = render(&s, 120, 60);
    assert_palette_only(&open);
    assert!(contains(&open, "▾ Gathering recent changes · 7.3s · 2 tools"), "{}", dump(&open));
}

#[test]
fn a_title_ticks_live_and_the_answer_streams_under_the_block() {
    let rec = r5_saying("Gathering recent changes…");
    let mut s = client(&rec);
    let clock = replay(&mut s, &rec, 0, Until::Started(3));
    s.now_ms = clock + 2_000;
    let buf = shot("work_title_live", &s);
    let text = dump(&buf);
    assert!(text.contains("▸ Gathering recent changes · ") && text.contains("2 tools"), "{text}");
    assert!(text.contains("└ manage_project  create   running"), "{text}");
    assert!(!text.contains("▸ thinking"), "{text}");
}

#[test]
fn a_question_a_long_line_or_two_sentences_stay_text_and_so_does_text_with_no_call_after_it() {
    for (said, title) in [
        ("Shall I start with the Kimi models?", false),
        ("Gathering recent changes. Then I will look at the diff.", false),
        (LONG_START, false),
        ("Gathering recent changes", true),
        ("Looking at the open issues.", true),
    ] {
        let s = replayed(&r5_saying(said));
        let text = dump(&render(&s, 120, 30));
        let titled = text.contains(&format!("▸ {}", said.trim_end_matches('.')));
        assert_eq!(titled, title, "{said:?}\n{text}");
        assert!(title || text.contains(said.split_whitespace().take(5).collect::<Vec<_>>().join(" ").as_str()), "{said:?} stays on screen as text\n{text}");
    }
    // Text at the end of the turn, after a block, is the answer: never a title.
    let s = replayed(&recorded("r5"));
    assert!(dump(&render(&s, 120, 30)).contains("▸ thought 7.3s"));
}

#[test]
fn a_reopened_session_draws_the_turn_as_the_live_one_did() {
    for said in ["Gathering recent changes…", LONG_START] {
        let rec = r5_saying(said);
        let live = replayed(&rec);
        let mut reopened = client(&rec);
        // The turn replayed is the first six rows of the recording.
        feed(&mut reopened, 5, Event::Transcript { session: rec.sid.clone(), turns: rec.turns[..6].to_vec() });
        // The same rows, whichever way the entries came (the numbers differ: a stored turn has no clock).
        let rows = |s: &State| dump(&render(s, 120, 30)).lines().filter(|l| l.contains("Gathering") || l.contains("I will set") || l.contains("thought") || l.contains("manage_project") || l.contains("Project created")).map(|l| l.chars().take(12).collect::<String>()).collect::<Vec<_>>();
        assert_eq!(rows(&reopened), rows(&live), "{said:?}
{}
{}", dump(&render(&reopened, 120, 30)), dump(&render(&live, 120, 30)));
        let buf = render(&reopened, 120, 30);
        assert_palette_only(&buf);
        if said.len() < 40 {
            snapshot("work_title_reopened_120x30", &buf);
        }
    }
}
