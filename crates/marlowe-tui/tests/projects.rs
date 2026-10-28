//! Projects in the client: the inline entry, the plan and checkin cards, the board rows and the
//! report, replayed from the real recorded runs (`tests/fixtures/projects`: m6 and m9 research
//! prose and records, m1 with failing sections, a2 a short run) at several moments.

mod common;

use common::*;
use crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind, KeyModifiers};
use marlowe_proto::{Event, ProjectPatch, ProjectSummary, Request, SessionId};
use marlowe_tui::projects::ReportDoc;
use marlowe_tui::state::{user_entry, Entry};
use marlowe_tui::{State, View};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use serde_json::Value;

struct Fx {
    state: Value,
    now_ms: i64,
    conversation: Vec<Value>,
    session: String,
    report: Option<String>,
}

fn fixture(name: &str) -> Fx {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join("projects");
    let text = std::fs::read_to_string(dir.join(format!("{name}.json"))).expect("fixture");
    let v: Value = serde_json::from_str(&text).expect("fixture json");
    let run = name.split('-').next().unwrap();
    let report = std::fs::read_to_string(dir.join(format!("{run}-report.md"))).ok();
    let state = v["state"].clone();
    Fx {
        session: state["session"].as_str().unwrap().to_string(),
        now_ms: v["now_ms"].as_i64().unwrap(),
        conversation: v["conversation"].as_array().cloned().unwrap_or_default(),
        report,
        state,
    }
}

fn id_of(fx: &Fx) -> String {
    fx.state["id"].as_str().unwrap().to_string()
}

/// A client attached to the fixture's conversation, its transcript as recorded up to the moment.
fn client(fx: &Fx) -> State {
    let sid = SessionId::from(fx.session.as_str());
    let mut s = state();
    s.date = "Friday 9 October".into();
    welcome(&mut s, 0);
    feed(&mut s, 1, library_with_sonnet());
    feed(&mut s, 0, Event::SessionOpened { session: sid.clone(), title: "ai models".into(), workspace: Some("C:/work/valiance".into()) });
    s.session = Some(sid);
    s.title = Some("ai models".into());
    s.now_ms = fx.now_ms;
    s.projects.clock = Some(fx.now_ms as u64);
    for c in &fx.conversation {
        if c["at_ms"].as_i64().unwrap_or(i64::MAX) > fx.now_ms + 1_000 {
            continue;
        }
        let text = c["text"].as_str().unwrap_or("").to_string();
        match (c["role"].as_str(), c["origin"].as_str().unwrap_or("")) {
            (Some("user"), _) => s.entries.push(user_entry(text)),
            (Some("assistant"), _) => s.entries.push(Entry::Prose { text }),
            _ => {}
        }
    }
    s
}

fn give_state(s: &mut State, fx: &Fx, rev: u64) -> Vec<Request> {
    give_state_at(s, fx, rev, 5)
}

/// The state again, as a later event of the stream (`seq` later than any before).
fn give_state_at(s: &mut State, fx: &Fx, rev: u64, seq: u64) -> Vec<Request> {
    let out = feed(s, seq, Event::ProjectState { project: id_of(fx), rev, state: fx.state.clone() });
    // The report file is a fixture beside the states, not at the path the run recorded.
    if let Some(text) = &fx.report {
        let id = id_of(fx);
        if let Some(l) = s.projects.loaded.iter_mut().find(|l| l.state.id == id) {
            if let Some(r) = &mut l.report {
                *r = ReportDoc { path: r.path.clone(), text: Some(text.clone()) };
            }
        }
    }
    out
}

fn at(name: &str) -> (State, Fx) {
    let fx = fixture(name);
    let mut s = client(&fx);
    give_state(&mut s, &fx, 100);
    (s, fx)
}

fn click(x: u16, y: u16) -> MouseEvent {
    MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: x, row: y, modifiers: KeyModifiers::NONE }
}

fn find(buf: &Buffer, needle: &str) -> (u16, u16) {
    for y in buf.area.top()..buf.area.bottom() {
        let mut line = String::new();
        let mut starts = Vec::new();
        for x in buf.area.left()..buf.area.right() {
            starts.push(line.chars().count());
            line.push_str(buf.cell((x, y)).map_or(" ", |c| c.symbol()));
        }
        if let Some(i) = line.find(needle) {
            let col = line[..i].chars().count();
            let x = starts.iter().position(|s| *s >= col).unwrap_or(0) as u16;
            return (x, y);
        }
    }
    panic!("{needle:?} is not on screen:\n{}", dump(buf));
}

fn shot(name: &str, s: &State, w: u16, h: u16) -> Buffer {
    let buf = render(s, w, h);
    assert_palette_only(&buf);
    snapshot(&format!("{name}_{w}x{h}"), &buf);
    buf
}

// ---- the moments ---------------------------------------------------------------------------------

#[test]
fn m6_plan_awaiting_shows_the_card_under_the_question() {
    let (s, _) = at("m6-01-plan-awaiting");
    let buf = shot("projects_m6_plan", &s, 120, 30);
    assert!(contains(&buf, "Approve") && contains(&buf, "revise"), "{}", dump(&buf));
    shot("projects_m6_plan", &s, 200, 50);
}

#[test]
fn m6_mid_collection_narrates_progress() {
    let (s, _) = at("m6-03-mid-collection");
    let buf = shot("projects_m6_mid", &s, 120, 30);
    assert!(contains(&buf, "Projects"), "the board section is the project's: {}", dump(&buf));
    shot("projects_m6_mid", &s, 200, 50);
}

#[test]
fn m6_checkin_shows_the_leads_words_and_options() {
    let (s, _) = at("m6-05-checkin-1");
    let buf = shot("projects_m6_checkin", &s, 120, 30);
    assert!(contains(&buf, "Keep researching"), "{}", dump(&buf));
    shot("projects_m6_checkin", &s, 200, 50);
}

#[test]
fn m6_done_settles_into_a_result_entry() {
    let (s, _) = at("m6-09-done");
    let buf = shot("projects_m6_done", &s, 120, 30);
    assert!(contains(&buf, "finished"), "{}", dump(&buf));
    shot("projects_m6_done", &s, 200, 50);
}

#[test]
fn m9_done_shows_the_records() {
    let (s, _) = at("m9-09-done");
    shot("projects_m9_done", &s, 120, 30);
    shot("projects_m9_done", &s, 200, 50);
}

#[test]
fn a2_done_and_m1_failing_sections() {
    let (s, _) = at("a2-05-done");
    shot("projects_a2_done", &s, 120, 30);
    let (s, _) = at("m1-04-report-sections-failing");
    shot("projects_m1_failing", &s, 120, 30);
    let (s, _) = at("m1-05-done");
    shot("projects_m1_done", &s, 120, 30);
    // Another run's sections failing while the report is written, and another mid-verification.
    let (s, _) = at("a3-05-report-failing");
    shot("projects_a3_failing", &s, 120, 30);
    let (s, _) = at("m5-04-condense-verify");
    shot("projects_m5_verify", &s, 120, 30);
}

#[test]
fn employees_held_back_are_drawn_waiting_with_the_reason_and_not_counted_as_in_flight() {
    let mut fx = fixture("m6-03-mid-collection");
    let mut held = 0;
    for a in fx.state["agents"].as_object_mut().unwrap().values_mut() {
        if a["status"] == "running" {
            a["status"] = "waiting".into();
            a["note"] = "waiting for the GPU".into();
            held += 1;
        }
    }
    assert!(held >= 2, "the recorded moment has employees in calls");
    let mut s = client(&fx);
    give_state(&mut s, &fx, 100);
    let buf = render(&s, 200, 50);
    assert_palette_only(&buf);
    assert!(contains(&buf, &format!("{held} waiting")) && contains(&buf, "waiting for the GPU"), "{}", dump(&buf));
    assert!(!contains(&buf, "in flight"), "none is in a call: {}", dump(&buf));
    // The steps' briefs count them apart from the ones in a call (analyze had one, condense two).
    assert!(contains(&buf, "analyze 15 ◌1") && contains(&buf, "condense 3 ◌2"), "{}", dump(&buf));
}

fn project_entries(s: &State) -> usize {
    s.entries.iter().filter(|e| matches!(e, Entry::Project { .. })).count()
}

#[test]
fn a_reopened_project_takes_its_old_result_out_while_it_runs_and_settles_again_when_it_ends() {
    let running = fixture("m6-03-mid-collection");
    let done = fixture("m6-09-done");
    let mut s = client(&done);
    give_state_at(&mut s, &running, 10, 10);
    assert_eq!(project_entries(&s), 0, "a running project is drawn after the conversation, not in it");
    give_state_at(&mut s, &done, 20, 20);
    assert_eq!(project_entries(&s), 1);
    assert!(matches!(s.entries.last(), Some(Entry::Project { .. })));
    // More talk, then the project is reopened and runs again.
    s.entries.push(user_entry("and what about Qwen?".into()));
    s.entries.push(Entry::Prose { text: "Let me look.".into() });
    give_state_at(&mut s, &running, 30, 30);
    assert_eq!(project_entries(&s), 0, "the old result leaves the transcript while the project runs");
    let buf = render(&s, 200, 50);
    assert_palette_only(&buf);
    // When it ends again the result settles at the end, not where the first one stood.
    give_state_at(&mut s, &done, 40, 40);
    assert_eq!(project_entries(&s), 1);
    assert!(matches!(s.entries.last(), Some(Entry::Project { .. })), "it settles after the talk that came since");
}

#[test]
fn a_project_that_ends_while_a_turn_streams_settles_after_the_turn_and_never_splits_its_reply() {
    let running = fixture("m6-03-mid-collection");
    let done = fixture("m6-09-done");
    let mut s = client(&done);
    give_state_at(&mut s, &running, 10, 10);
    let sid = SessionId::from(done.session.as_str());
    feed(&mut s, 11, Event::TurnStarted { session: sid.clone(), turn: 7, model: sonnet_ref(), user: "how is the weather".into() });
    feed(&mut s, 12, Event::TextDelta { session: sid.clone(), turn: 7, text: "It is ".into() });
    give_state_at(&mut s, &done, 20, 13);
    assert_eq!(project_entries(&s), 0, "nothing is put into the transcript under a turn that is being written");
    feed(&mut s, 14, Event::TextDelta { session: sid.clone(), turn: 7, text: "sunny.".into() });
    let prose: Vec<&str> = s.entries.iter().filter_map(|e| if let Entry::Prose { text } = e { text.as_str().into() } else { None }).collect();
    assert!(prose.iter().any(|t| t.contains("It is sunny.")), "the reply is one entry: {prose:?}");
    feed(&mut s, 15, Event::TurnFinished { session: sid, turn: 7, usage: Default::default(), stop: marlowe_proto::TurnStop::Done });
    assert_eq!(project_entries(&s), 1, "settled when the turn ended");
    assert!(s.entries.iter().rposition(|e| matches!(e, Entry::Project { .. })) > s.entries.iter().position(|e| matches!(e, Entry::Prose { text } if text.contains("It is sunny."))));
}

#[test]
fn a_daemon_without_projects_is_asked_once_and_its_answer_is_never_flashed() {
    let mut s = state();
    welcome(&mut s, 0);
    feed(&mut s, 1, library_with_sonnet());
    feed(&mut s, 0, Event::Error { session: None, message: marlowe_tui::projects::NO_PROJECTS.into() });
    assert!(s.flash.is_none());
    // The conversation opens, and the list is not asked for again.
    let reqs = feed(&mut s, 0, Event::SessionOpened { session: SessionId::from("5e55"), title: "t".into(), workspace: None });
    assert!(!reqs.contains(&Request::ListProjects), "{reqs:?}");
    // An answer to a request sent before the absence was known is not flashed either.
    feed(&mut s, 0, Event::Error { session: None, message: marlowe_tui::projects::NO_PROJECTS.into() });
    assert!(s.flash.is_none());
}

// ---- the protocol -----------------------------------------------------------------------------------

#[test]
fn a_project_of_another_conversation_is_not_drawn() {
    let fx = fixture("m6-03-mid-collection");
    let mut s = client(&fx);
    s.session = Some(SessionId::from("someone-else"));
    give_state(&mut s, &fx, 100);
    assert!(s.projects.loaded.is_empty());
}

#[test]
fn the_list_asks_for_projects_of_this_conversation_only() {
    let fx = fixture("m6-03-mid-collection");
    let mut s = client(&fx);
    let summary = |id: &str, session: &str| ProjectSummary {
        id: id.into(),
        session: Some(session.into()),
        kind: "research".into(),
        title: "t".into(),
        status: "running".into(),
        phase: "DataCollection".into(),
        rev: 5,
        updated_ms: 0,
    };
    let out = feed(&mut s, 3, Event::ProjectList { projects: vec![summary("p_mine", &fx.session), summary("p_other", "zzz")] });
    assert_eq!(out, vec![Request::GetProject { project: "p_mine".into() }]);
}

#[test]
fn a_patch_gap_asks_for_the_state_again() {
    let (mut s, fx) = at("m6-03-mid-collection");
    let id = id_of(&fx);
    // base 99 is not the rev the client holds (100): a gap.
    let out = feed(&mut s, 6, Event::ProjectUpdated { project: id.clone(), base: 99, rev: 101, patches: vec![ProjectPatch::set("/title", serde_json::json!("x"))] });
    assert_eq!(out, vec![Request::GetProject { project: id.clone() }]);
    // In sequence, it applies.
    let out = feed(&mut s, 7, Event::ProjectUpdated { project: id.clone(), base: 100, rev: 101, patches: vec![ProjectPatch::set("/title", serde_json::json!("A new title"))] });
    assert!(out.is_empty());
    s.now_ms += 1_000;
    s.on_tick();
    assert_eq!(s.projects.get(&id).unwrap().state.title, "A new title");
}

// ---- the cards ------------------------------------------------------------------------------------------

fn gate_requests(out: &[Request]) -> Vec<&Request> {
    out.iter().filter(|r| matches!(r, Request::AnswerProjectGate { .. })).collect()
}

#[test]
fn alt_p_gives_the_plan_card_the_keys_and_a_approves() {
    let (mut s, fx) = at("m6-01-plan-awaiting");
    s.on_key(alt('p'));
    assert_eq!(s.projects.focus.as_deref(), Some(id_of(&fx).as_str()));
    let out = s.on_key(key(KeyCode::Char('a')));
    let reqs = gate_requests(&out);
    assert_eq!(reqs.len(), 1, "{out:?}");
    match reqs[0] {
        Request::AnswerProjectGate { project, choice, .. } => assert_eq!((project.as_str(), choice.as_str()), (id_of(&fx).as_str(), "approve")),
        _ => unreachable!(),
    }
    // Asked once: the card shows it is answering and does not send again.
    assert!(gate_requests(&s.on_key(key(KeyCode::Char('a')))).is_empty());
    let buf = render(&s, 120, 30);
    assert!(contains(&buf, "sending your answer"), "{}", dump(&buf));
}

#[test]
fn clicking_approve_answers_and_clicking_revise_opens_the_composer() {
    let (mut s, fx) = at("m6-01-plan-awaiting");
    let buf = render(&s, 120, 30);
    let (x, y) = find(&buf, "revise");
    let out = s.on_mouse(click(x, y), Rect::new(0, 0, 120, 30));
    assert!(out.is_empty());
    assert!(s.composer.text().starts_with(&format!("revise the plan of {}", id_of(&fx))), "{:?}", s.composer.text());

    let buf = render(&s, 120, 30);
    let (x, y) = find(&buf, "Approve");
    let out = s.on_mouse(click(x, y), Rect::new(0, 0, 120, 30));
    assert_eq!(gate_requests(&out).len(), 1, "{out:?}");
}

#[test]
fn a_card_answered_elsewhere_stops_offering_itself() {
    let (mut s, fx) = at("m6-05-checkin-1");
    let id = id_of(&fx);
    let gate = s.projects.get(&id).unwrap().open_gate().unwrap().id.clone();
    feed(&mut s, 8, Event::ProjectGateAnswered { project: id.clone(), gate, choice: "approve".into() });
    s.on_key(alt('p'));
    let out = s.on_key(key(KeyCode::Char('a')));
    assert!(gate_requests(&out).is_empty(), "{out:?}");
}

#[test]
fn checkin_options_answer_by_their_letters_and_steer_asks_for_words() {
    let (mut s, fx) = at("m6-05-checkin-1");
    s.on_key(alt('p'));
    let out = s.on_key(key(KeyCode::Char('s')));
    assert!(gate_requests(&out).is_empty(), "steer asks for its text first");
    let buf = render(&s, 120, 30);
    assert!(contains(&buf, "steer"), "{}", dump(&buf));
    for c in "look at benchmarks".chars() {
        s.on_key(key(KeyCode::Char(c)));
    }
    let out = s.on_key(key(KeyCode::Enter));
    match gate_requests(&out)[..] {
        [Request::AnswerProjectGate { project, choice, text, .. }] => {
            assert_eq!((project.as_str(), choice.as_str(), text.as_deref()), (id_of(&fx).as_str(), "steer", Some("look at benchmarks")));
        }
        _ => panic!("{out:?}"),
    }
}

#[test]
fn cancel_asks_first() {
    let (mut s, _) = at("m6-05-checkin-1");
    s.on_key(alt('p'));
    let out = s.on_key(key(KeyCode::Char('x')));
    assert!(gate_requests(&out).is_empty());
    let buf = render(&s, 120, 30);
    assert!(contains(&buf, "cancel"), "{}", dump(&buf));
    let out = s.on_key(key(KeyCode::Char('y')));
    assert_eq!(gate_requests(&out).len(), 1, "{out:?}");
}

#[test]
fn a_running_project_can_be_cancelled_and_checked_in_from_its_entry() {
    let (mut s, fx) = at("m6-03-mid-collection");
    s.on_key(alt('p'));
    let out = s.on_key(key(KeyCode::Char('c')));
    assert_eq!(out, vec![Request::CheckinProject { project: id_of(&fx) }]);
    assert!(s.on_key(key(KeyCode::Char('x'))).is_empty());
    let out = s.on_key(key(KeyCode::Char('y')));
    assert_eq!(out, vec![Request::CancelProject { project: id_of(&fx) }]);
}

// ---- the report and the board -------------------------------------------------------------------------

#[test]
fn enter_on_a_finished_entry_opens_the_report() {
    let (mut s, _) = at("m6-09-done");
    s.on_key(alt('p'));
    // Nothing is running: alt-p has nothing to go to, the click on the board does.
    let buf = render(&s, 120, 30);
    let (x, y) = find(&buf, "Compile a compre");
    s.on_mouse(click(x, y), Rect::new(0, 0, 120, 30));
    s.on_key(key(KeyCode::Enter));
    let buf = render(&s, 120, 30);
    assert!(contains(&buf, "report"), "{}", dump(&buf));
    assert!(contains(&buf, "Confidence"), "{}", dump(&buf));
    assert_palette_only(&buf);
    snapshot("projects_m6_report_120x30", &buf);
    s.on_key(key(KeyCode::Esc));
}

#[test]
fn a_click_on_the_board_row_goes_to_the_entry() {
    let (mut s, fx) = at("m6-03-mid-collection");
    let buf = render(&s, 120, 30);
    let (x, y) = find(&buf, "Compile a compre");
    s.on_mouse(click(x, y), Rect::new(0, 0, 120, 30));
    assert_eq!(s.projects.focus.as_deref(), Some(id_of(&fx).as_str()));
    assert_eq!(s.view, View::Conversation);
}

#[test]
fn the_project_never_shows_twice() {
    let (s, _) = at("m6-09-done");
    let text = dump(&render(&s, 200, 50));
    assert_eq!(text.matches("Project p_fac325c3 finished").count(), 0, "the relay's body is folded into the entry:\n{text}");
}

#[test]
fn the_rule_is_amber_while_a_card_waits_and_dimmer_once_it_settled() {
    let th = theme();
    let rule_fg = |s: &State| {
        let buf = render(s, 120, 30);
        let (x, y) = find(&buf, "▏");
        buf[(x, y)].fg
    };
    let (waiting, _) = at("m6-01-plan-awaiting");
    assert_eq!(rule_fg(&waiting), marlowe_surface::chrome::Ink::Amber.color(&th));
    let (done, _) = at("m6-09-done");
    assert_ne!(rule_fg(&done), marlowe_surface::chrome::Ink::Amber.color(&th));
}
