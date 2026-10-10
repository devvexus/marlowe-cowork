//! Pointer and keys across every view: the one-model Library, going back from anywhere, hover
//! lighting and click targets, the header, and the Run view's research stage strip. Every frame
//! keeps the no-background palette rule.

mod common;

use common::*;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use marlowe_proto::{
    ApprovalDecision, ApprovalId, CrewBudget, CrewRunInfo, CrewRunStatus, CrewSpent, Event, Playbook, QuietHours, Request,
    ResearchCounts, ResearchProgress, ResearchStage, RunId, RunInfo, RunStatus, StageState, WatchRoot, WatchState,
};
use marlowe_tui::library::{LibFocus, LibStep};
use marlowe_tui::render::geometry;
use marlowe_tui::state::{Command, Focus, Overlay, Target};
use marlowe_tui::{State, View};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};

const NOW: i64 = 1_800_000_000_000;
const ROOT: &str = "D:/lab/PruningResearch/valiance";

fn area() -> Rect {
    Rect::new(0, 0, W, H)
}

fn click(x: u16, y: u16) -> MouseEvent {
    MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: x, row: y, modifiers: KeyModifiers::NONE }
}

fn moved(x: u16, y: u16) -> MouseEvent {
    MouseEvent { kind: MouseEventKind::Moved, column: x, row: y, modifiers: KeyModifiers::NONE }
}

/// Where `needle` starts on screen (column, row), counting cells.
fn find(buf: &Buffer, needle: &str) -> (u16, u16) {
    for (y, line) in dump(buf).lines().enumerate() {
        if let Some(i) = line.find(needle) {
            return (line[..i].chars().count() as u16, y as u16);
        }
    }
    panic!("{needle:?} is not on screen\n{}", dump(buf));
}

/// Renders (which records where everything is) and checks the palette.
fn frame(s: &State) -> Buffer {
    let buf = render(s, W, H);
    assert_palette_only(&buf);
    buf
}

fn crew_run(id: &str, role: &str, title: &str, status: CrewRunStatus) -> CrewRunInfo {
    CrewRunInfo {
        id: RunId::from(id),
        session: Some(sid()),
        role: role.into(),
        title: title.into(),
        goal: title.into(),
        playbook: None,
        status,
        workspace: Some("C:/work/valiance".into()),
        sandbox: None,
        budget: CrewBudget { tokens: Some(400_000), steps: Some(40), usd: Some(0.5) },
        spent: CrewSpent { tokens: 41_200, steps: 6, usd: 0.0213 },
        step: 6,
        current_action: Some("web_search \"lora rank ablation\"".into()),
        note: None,
        tainted: false,
        created_ms: NOW - 20 * 60_000,
        updated_ms: NOW - 60_000,
        result: None,
    }
}

fn watch_run(name: &str, status: RunStatus) -> RunInfo {
    RunInfo {
        dir: format!("{ROOT}/runs/{name}"),
        name: name.into(),
        status,
        stage: Some("wnr-p5".into()),
        layer: Some(9),
        pid: Some(48_440),
        heartbeat_ms: Some(NOW - 40_000),
        last_line: None,
        step: None,
        loss: None,
        since_ms: NOW - 3_600_000,
    }
}

/// A model, a watched root with a live run, two crew runs (one deep research), a session.
fn busy() -> State {
    let mut s = state();
    s.date = "Friday 9 October".into();
    welcome(&mut s, 0);
    feed(&mut s, 1, library_with_sonnet());
    feed(
        &mut s,
        0,
        Event::WatchState(WatchState {
            roots: vec![WatchRoot { path: ROOT.into(), added_ms: NOW - 86_400_000, runs: 1, git: None, problem: None }],
            runs: vec![watch_run("marlowe-valiance-22b-wnr5", RunStatus::Alive)],
            quiet: QuietHours::default(),
            ..Default::default()
        }),
    );
    let mut research = crew_run("aa11bb22", "researcher", "LoRA rank ablations for 22B gates", CrewRunStatus::Running);
    research.playbook = Some(Playbook::DeepResearch);
    feed(
        &mut s,
        0,
        Event::Runs {
            list: vec![research, crew_run("ee55ff66", "engineer", "fix the gate threshold", CrewRunStatus::NeedsYou)],
        },
    );
    feed(&mut s, 0, Event::SessionOpened { session: sid(), title: "gate work".into(), workspace: Some("C:/work/valiance".into()) });
    s.session = Some(sid());
    s.title = Some("gate work".into());
    s
}

// ---- 1. the one-model Library ---------------------------------------------------------------------

/// Matthew's first session: one model in the library, the Model box clicked, "open the library…"
/// chosen. Nothing said what ↵ would do or how to leave. The keys must go to the Library (not the
/// Message box), the row under the cursor must be visible, the hints must name ↵'s action and the
/// way back, and "add another model" must be a row of its own.
#[test]
fn one_model_library_from_the_model_box_takes_the_keys_and_says_what_they_do() {
    let mut s = state();
    welcome(&mut s, 0);
    feed(&mut s, 1, library_with_sonnet());
    frame(&s);
    let m = geometry(area(), &s).model.unwrap();
    s.on_mouse(click(m.x + 3, m.y + 1), area());
    assert!(matches!(s.overlay, Some(Overlay::ModelPicker { sel: 0 })));
    // The picker's last row opens the library.
    s.on_key(key(KeyCode::Char('j')));
    s.on_key(key(KeyCode::Enter));
    assert_eq!((s.view, s.overlay.clone()), (View::Library, None));

    let buf = frame(&s);
    for want in ["› Conductor", "↵ effort medium → high", "←→ model", "esc/⌫ back", "+ add another model",
        "1 in the library · the default for every slot"]
    {
        assert!(contains(&buf, want), "missing {want:?}\n{}", dump(&buf));
    }

    // ↓ (or j) walks the slots, then the one model, then "add another model". Nothing is typed.
    for _ in 0..3 {
        s.on_key(key(KeyCode::Down));
    }
    assert_eq!((s.library.focus, s.library.slot_sel), (LibFocus::Slots, 3));
    s.on_key(key(KeyCode::Char('j')));
    assert_eq!((s.library.focus, s.library.model_sel), (LibFocus::Models, 0));
    let buf = frame(&s);
    assert!(contains(&buf, "› Claude Sonnet 4.5"), "the one model is visibly selected\n{}", dump(&buf));
    assert!(contains(&buf, "↵ keep as conductor"), "{}", dump(&buf));
    assert!(contains(&buf, "d remove"));
    s.on_key(key(KeyCode::Char('j')));
    let buf = frame(&s);
    assert!(contains(&buf, "› + add another model"), "{}", dump(&buf));
    assert!(contains(&buf, "↵ add another model"));
    s.on_key(key(KeyCode::Char('k')));
    assert_eq!(s.library.model_sel, 0, "k goes back up");
    s.on_key(key(KeyCode::Char('j')));

    // ↵ on the add row opens the providers; Backspace goes back one level at a time.
    s.on_key(key(KeyCode::Enter));
    assert!(matches!(s.library.step, LibStep::Providers { .. }));
    s.on_key(key(KeyCode::Backspace));
    assert_eq!(s.library.step, LibStep::Overview);
    s.on_key(key(KeyCode::Backspace));
    assert_eq!(s.view, View::Conversation);
    assert!(s.composer.text().is_empty(), "no library key reached the Message box");
}

#[test]
fn enter_on_the_model_row_makes_it_the_conductor_and_clicks_select_then_act() {
    let mut s = state();
    welcome(&mut s, 0);
    feed(&mut s, 1, library_with_sonnet());
    s.on_key(ctrl('l'));
    let buf = frame(&s);
    // A click on the model row selects it; a second click acts like ↵.
    let (x, y) = find(&buf, "Claude Sonnet 4.5                 OpenRouter    200k");
    let out = s.on_mouse(click(x, y), area());
    assert!(out.is_empty());
    assert_eq!((s.library.focus, s.library.model_sel), (LibFocus::Models, 0));
    frame(&s);
    let out = s.on_mouse(click(x, y), area());
    assert!(
        matches!(&out[..], [Request::SetSlot { slot: marlowe_proto::Slot::Conductor, model: Some(m), .. }] if *m == sonnet_ref()),
        "{out:?}"
    );
    // The add row is a button.
    let buf = frame(&s);
    let (x, y) = find(&buf, "+ add another model");
    s.on_mouse(click(x, y), area());
    assert!(matches!(s.library.step, LibStep::Providers { .. }));
}

// ---- the same audit for every view ----------------------------------------------------------------

#[test]
fn every_view_and_picker_goes_back_on_esc_and_on_backspace() {
    let back = [KeyCode::Esc, KeyCode::Backspace];
    for k in back {
        let mut s = busy();
        // Full-screen views: back to the conversation.
        for chord in ['l', 'g', 'w'] {
            s.on_key(ctrl(chord));
            assert_ne!(s.view, View::Conversation, "^{chord}");
            s.on_key(key(k));
            assert_eq!(s.view, View::Conversation, "^{chord} then {k:?}");
            assert!(s.composer.text().is_empty());
        }
        // The crew: the run view, then the list, then the conversation.
        s.on_key(ctrl('r'));
        assert!(s.crew.open.is_some());
        s.on_key(key(k));
        assert!(s.crew.open.is_none(), "{k:?} leaves the run for the list");
        assert_eq!(s.view, View::Crew);
        s.on_key(key(k));
        assert_eq!(s.view, View::Conversation);
        // Pickers and panels close.
        for open in [ctrl('o'), ctrl('k'), ctrl('d')] {
            s.on_key(open);
            assert!(s.overlay.is_some(), "{open:?}");
            s.on_key(key(k));
            assert_eq!(s.overlay, None, "{open:?} then {k:?}");
        }
        s.on_key(key(KeyCode::Esc));
        for hot in ['m', 'w'] {
            s.focus = Focus::Conversation;
            s.on_key(key(KeyCode::Char(hot)));
            assert!(s.overlay.is_some(), "{hot}");
            s.on_key(key(k));
            assert_eq!((s.overlay.clone(), s.focus), (None, Focus::Message), "{hot} then {k:?}");
        }
    }
}

#[test]
fn backspace_edits_what_is_typed_before_it_goes_back() {
    let mut s = busy();
    s.on_key(ctrl('g'));
    type_str(&mut s, "ab");
    s.on_key(key(KeyCode::Backspace));
    assert_eq!((s.view, s.memory.query.as_str()), (View::Memory, "a"));
    s.on_key(key(KeyCode::Backspace));
    s.on_key(key(KeyCode::Backspace));
    assert_eq!(s.view, View::Conversation);
    // The palette's query, then the palette.
    s.on_key(ctrl('k'));
    type_str(&mut s, "me");
    s.on_key(key(KeyCode::Backspace));
    assert!(matches!(&s.overlay, Some(Overlay::Palette { query, .. }) if query == "m"));
}

#[test]
fn j_and_k_move_in_lists() {
    let mut s = busy();
    s.on_key(ctrl('r'));
    s.on_key(key(KeyCode::Esc));
    assert_eq!(s.crew.sel, 0);
    s.on_key(key(KeyCode::Char('j')));
    assert_eq!(s.crew.sel, 1);
    s.on_key(key(KeyCode::Char('k')));
    assert_eq!(s.crew.sel, 0);
    s.on_key(ctrl('o'));
    feed(&mut s, 0, Event::SessionList { sessions: vec![] });
    s.on_key(key(KeyCode::Char('j')));
    assert!(matches!(s.overlay, Some(Overlay::Sessions { sel: 0 })), "an empty list stays put");
}

// ---- 2. hover and click -------------------------------------------------------------------------

#[test]
fn hover_lights_the_box_under_the_pointer_and_only_changes_when_the_target_does() {
    let th = theme();
    let mut s = busy();
    frame(&s);
    let m = geometry(area(), &s).model.unwrap();
    s.on_mouse(moved(m.x + 4, m.y + 1), area());
    assert_eq!(s.hover, Some(Target::Box(Focus::Model)));
    let buf = frame(&s);
    assert_eq!(buf[(m.x, m.y)].fg, th.hover_color(), "the hovered box's border takes the hover tone");
    let c = geometry(area(), &s).composer.unwrap();
    assert_eq!(buf[(c.x, c.y)].fg, th.accent(), "focus keeps the accent");
    // Motion inside the same box is not a change: the loop does not redraw for it.
    let before = s.hover.clone();
    s.on_mouse(moved(m.x + 9, m.y + 1), area());
    assert_eq!(s.hover, before);
    // Off every target: nothing is lit.
    s.on_mouse(moved(0, 0), area());
    let buf = frame(&s);
    assert_ne!(buf[(m.x, m.y)].fg, th.hover_color());
}

#[test]
fn hovered_rows_and_chords_brighten_their_foreground_only() {
    let th = theme();
    let mut s = busy();
    let buf = frame(&s);
    // A crew run on the board.
    let (x, y) = find(&buf, "LoRA rank ablations");
    s.on_mouse(moved(x, y), area());
    assert_eq!(s.hover, Some(Target::Run(RunId::from("aa11bb22"))));
    let buf = frame(&s);
    assert!(buf[(x, y)].modifier.contains(Modifier::BOLD), "the hovered run's title goes bright");
    // A footer chord.
    let (x, y) = find(&buf, "^g Memory");
    s.on_mouse(moved(x + 4, y), area());
    assert_eq!(s.hover, Some(Target::Chord(Command::Memory)));
    let buf = frame(&s);
    assert_eq!(buf[(x, y)].fg, th.speech(), "the hovered chord's key takes Marlowe's colour");
    assert_eq!(buf[(x + 3, y)].fg, Color::Reset, "its label reads as body text");
    // A board tab (the stacked Memory box at this size is a whole-box target).
    let mem = geometry(area(), &s).sections[3];
    s.on_mouse(moved(mem.x + 2, mem.y + 1), area());
    assert_eq!(s.hover, Some(Target::Tab(marlowe_tui::state::BoardTab::Memory)));
    let buf = frame(&s);
    assert_eq!(buf[(mem.x, mem.y)].fg, th.hover_color());
}

#[test]
fn clicks_open_runs_watch_items_views_and_chords() {
    let mut s = busy();
    let buf = frame(&s);
    // A crew run on the board opens its Run view, watching it live.
    let (x, y) = find(&buf, "fix the gate threshold");
    let out = s.on_mouse(click(x, y), area());
    assert_eq!(s.view, View::Crew);
    assert_eq!(s.crew.open.as_ref().map(|v| v.run.clone()), Some(RunId::from("ee55ff66")));
    assert!(out.contains(&Request::WatchRun { run: RunId::from("ee55ff66") }), "{out:?}");
    // In the Run view the steer line is a button.
    let buf = frame(&s);
    let (x, y) = find(&buf, "s to steer");
    s.on_mouse(click(x, y), area());
    assert!(s.crew.open.as_ref().unwrap().steering);
    s.on_key(key(KeyCode::Esc));
    s.on_key(key(KeyCode::Esc));
    s.on_key(key(KeyCode::Esc));
    assert_eq!(s.view, View::Conversation);

    // A watched run opens Watch at the root that holds it.
    let buf = frame(&s);
    let (x, y) = find(&buf, "marlowe-valiance-22b-wnr5");
    s.on_mouse(click(x, y), area());
    assert_eq!((s.view, s.wake.sel), (View::Watch, 0));
    s.on_key(key(KeyCode::Esc));

    // The footer's chords run their commands; ^k opens the palette.
    let buf = frame(&s);
    let (x, y) = find(&buf, "^g Memory");
    s.on_mouse(click(x, y), area());
    assert_eq!(s.view, View::Memory);
    let buf = frame(&s);
    let (x, y) = find(&buf, "^k palette");
    s.on_mouse(click(x + 3, y), area());
    assert!(matches!(s.overlay, Some(Overlay::Palette { .. })));
    // A palette row runs its command.
    let buf = frame(&s);
    let (x, y) = find(&buf, "Watch — runs, notices, roots");
    s.on_mouse(click(x, y), area());
    assert_eq!((s.view, s.overlay.clone()), (View::Watch, None));
    s.on_key(key(KeyCode::Esc));

    // The header's summary opens the crew.
    let buf = frame(&s);
    let (x, y) = find(&buf, "3 runs · 1 needs you");
    s.on_mouse(click(x, y), area());
    assert_eq!(s.view, View::Crew);
    s.on_key(key(KeyCode::Esc));
    s.on_key(key(KeyCode::Esc));

    // The stacked Memory box opens the Memory view.
    frame(&s);
    let mem = geometry(area(), &s).sections[3];
    s.on_mouse(click(mem.x + 2, mem.y + 1), area());
    assert_eq!(s.view, View::Memory);
}

#[test]
fn the_crew_list_rows_open_their_runs() {
    let mut s = busy();
    s.on_key(ctrl('r'));
    s.on_key(key(KeyCode::Esc));
    let buf = frame(&s);
    let (x, y) = find(&buf, "LoRA rank ablations");
    s.on_mouse(moved(x, y), area());
    let buf = frame(&s);
    assert!(contains(&buf, "› ● researcher"), "the hovered run is marked\n{}", dump(&buf));
    s.on_mouse(click(x, y), area());
    assert_eq!(s.crew.open.as_ref().map(|v| v.run.clone()), Some(RunId::from("aa11bb22")));
}

#[test]
fn approval_buttons_answer_with_a_click() {
    let mut s = busy();
    feed(
        &mut s,
        9,
        Event::ApprovalRequested {
            approval: ApprovalId(7),
            session: sid(),
            tool: "bash".into(),
            summary: "run the eval".into(),
            detail: "python eval.py".into(),
            run: None,
        },
    );
    let buf = frame(&s);
    let (x, y) = find(&buf, "n deny");
    // Inside the type-ahead guard a click does nothing.
    assert!(s.on_mouse(click(x, y), area()).is_empty());
    s.now_ms += 1_000;
    s.on_mouse(moved(x, y), area());
    assert_eq!(s.hover, Some(Target::Button(2)));
    frame(&s);
    let out = s.on_mouse(click(x, y), area());
    assert_eq!(out, vec![Request::Approve { approval: ApprovalId(7), decision: ApprovalDecision::Deny }]);
    assert!(s.approvals.is_empty());
}

// ---- 4. the header ------------------------------------------------------------------------------

#[test]
fn the_header_names_the_session_and_sums_up_the_day_in_one_row() {
    let th = theme();
    let mut s = state();
    welcome(&mut s, 0);
    feed(&mut s, 1, library_with_sonnet());
    let buf = frame(&s);
    let (x, y) = find(&buf, "* marlowe — new session");
    assert_eq!((x, y), (1, 0), "the top row");
    assert_eq!(buf[(x, y)].fg, th.accent(), "the star is the accent");
    assert!(contains(&buf, "0 runs · 0 need you"));
    assert_eq!(s.window_title(), "marlowe — new session");
    let g = geometry(area(), &s);
    assert_eq!((g.header.height, g.main.y, g.main.height), (1, 1, H - 2), "one row, nothing more");

    let mut s = busy();
    let buf = frame(&s);
    assert!(contains(&buf, "* marlowe — gate work"));
    assert!(contains(&buf, "3 runs · 1 needs you"), "{}", dump(&buf));
    assert_eq!(s.window_title(), "marlowe — gate work");
    feed(&mut s, 0, Event::SessionOpened { session: sid(), title: "eval drift".into(), workspace: None });
    assert_eq!(s.window_title(), "marlowe — eval drift", "the title follows the session");
}

// ---- 5. looking in on the crew ------------------------------------------------------------------

fn progress(stage: ResearchStage, state: StageState, done: u32, total: u32, counts: ResearchCounts) -> Event {
    Event::ResearchProgress {
        run: RunId::from("aa11bb22"),
        progress: ResearchProgress { stage, state, done, total, detail: Some("reading arXiv:2401.01234".into()), counts },
    }
}

#[test]
fn a_research_run_shows_its_stages_and_counts_and_degrades_without_them() {
    let mut s = busy();
    let buf = frame(&s);
    let (x, y) = find(&buf, "LoRA rank ablations");
    s.on_mouse(click(x, y), area());
    let buf = frame(&s);
    assert!(!contains(&buf, "Plan →"), "no progress reported: the plain transcript\n{}", dump(&buf));
    assert!(contains(&buf, "TRANSCRIPT"));

    let mut counts = ResearchCounts { sources_found: 42, sources_fetched: 30, ..Default::default() };
    feed(&mut s, 20, progress(ResearchStage::Plan, StageState::Done, 1, 1, counts));
    feed(&mut s, 21, progress(ResearchStage::Explore, StageState::Running, 3, 8, counts));
    let buf = frame(&s);
    assert!(contains(&buf, "✓ Plan → ● Explore 3/8 → ○ Compress → ○ Corroborate → ○ Verify → ○ Synthesise"), "{}", dump(&buf));
    assert!(contains(&buf, "sources 42 found · 30 fetched"));
    assert!(contains(&buf, "reading arXiv:2401.01234"));

    counts.claims = 118;
    counts.independent_roots = 9;
    counts.citations_supported = 24;
    counts.citations_unsupported = 2;
    for (i, st) in [ResearchStage::Explore, ResearchStage::Compress, ResearchStage::Corroborate].into_iter().enumerate() {
        feed(&mut s, 22 + i as u64, progress(st, StageState::Done, 1, 1, counts));
    }
    feed(&mut s, 30, progress(ResearchStage::Verify, StageState::Running, 20, 26, counts));
    let buf = frame(&s);
    assert!(contains(&buf, "✓ Corroborate → ● Verify 20/26 → ○ Synthesise"), "{}", dump(&buf));
    assert!(contains(&buf, "claims 118 · independent roots 9 · citations 24 supported · 2 unsupported"), "{}", dump(&buf));
    snapshot("run_research_progress", &buf);
}

/// A research run proposes its plan: the Run view shows it with its quote, `a` approves it and
/// the plan leaves the screen; a second proposal is rejected with `x` then `y`.
#[test]
fn a_proposed_research_plan_is_shown_with_its_quote_and_answered_with_a_or_x() {
    use marlowe_proto::{ChannelId, CostQuote, PlanDecision, ResearchPlan, SubQuestion};
    let mut s = busy();
    let run = RunId::from("aa11bb22");
    let mut waiting = crew_run("aa11bb22", "researcher", "LoRA rank ablations for 22B gates", CrewRunStatus::NeedsYou);
    waiting.playbook = Some(Playbook::DeepResearch);
    feed(&mut s, 40, Event::RunUpdated(waiting));
    let plan = ResearchPlan {
        goal: "LoRA rank ablations".into(),
        sub_questions: vec![SubQuestion {
            id: 1,
            question: "Which ranks hold accuracy on 7B models?".into(),
            channels: vec![ChannelId::Arxiv, ChannelId::SemanticScholar],
            rationale: None,
        }],
        breadth: 3,
        depth: 1,
        source_budget: 12,
        recalled: vec![],
        notes: None,
    };
    let quote = CostQuote { model_tokens: 96_000, fetches: 12, minutes: 18, usd: None, basis: "local".into() };
    feed(&mut s, 41, Event::PlanProposed { run: run.clone(), plan: plan.clone(), quote: quote.clone() });
    let buf = frame(&s);
    let (x, y) = find(&buf, "LoRA rank ablations");
    s.on_mouse(click(x, y), area());
    let buf = frame(&s);
    assert!(contains(&buf, "Research plan — waiting for you"), "{}", dump(&buf));
    assert!(contains(&buf, "Which ranks hold accuracy on 7B models?"));
    assert!(contains(&buf, "arxiv · semantic_scholar"));
    assert!(contains(&buf, "breadth 3 · depth 1 · up to 12 sources"));
    assert!(contains(&buf, "a approve plan"), "{}", dump(&buf));
    let out = s.on_key(key(KeyCode::Char('a')));
    assert!(out.contains(&Request::ApprovePlan { run: run.clone(), decision: PlanDecision::Approve }), "{out:?}");
    assert!(!contains(&frame(&s), "Research plan — waiting for you"));

    feed(&mut s, 42, Event::PlanProposed { run: run.clone(), plan, quote });
    assert!(s.on_key(key(KeyCode::Char('x'))).is_empty(), "reject asks first");
    let out = s.on_key(key(KeyCode::Char('y')));
    assert!(out.contains(&Request::ApprovePlan { run, decision: PlanDecision::Reject { reason: None } }), "{out:?}");
}
