//! The main screen's layout at the three sizes it is designed for, the focus model that drives the
//! boxes' hotkeys, and the workspace picker. Every frame keeps the no-background palette rule.

mod common;

use common::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use marlowe_proto::{
    CrewBudget, CrewRunInfo, CrewRunStatus, CrewSpent, Event, MemoryStatsReport, QuietHours, Request, RunId,
    RunInfo, RunStatus, TurnStop, WatchRoot, WatchState,
};
use marlowe_tui::render::{geometry, BoardMode};
use marlowe_tui::state::{BoardTab, Focus, Overlay};
use marlowe_tui::{State, View};
use ratatui::layout::Rect;

const NOW: i64 = 1_800_000_000_000;
const ROOT: &str = "D:/lab/PruningResearch/valiance";

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
        workflow: None,
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
        last_line: Some("torch.OutOfMemoryError: CUDA out of memory.".into()),
        step: None,
        loss: None,
        since_ms: NOW - 3_600_000,
    }
}

/// A working session: a model, a finished turn with a recall, two crew runs, two watched runs.
fn working() -> State {
    let mut s = state();
    s.date = "Friday 9 October".into();
    welcome(&mut s, 0);
    feed(&mut s, 1, library_with_sonnet());
    feed(
        &mut s,
        0,
        Event::MemoryStats(MemoryStatsReport {
            beliefs: 390,
            episodes: 41,
            vectors: 388,
            models_loaded: vec![],
            degraded: None,
            journal_seq: 1_204,
            ..Default::default()
        }),
    );
    feed(
        &mut s,
        0,
        Event::WatchState(WatchState {
            roots: vec![WatchRoot { path: ROOT.into(), added_ms: NOW - 86_400_000, runs: 2, git: None, problem: None }],
            runs: vec![watch_run("marlowe-valiance-22b-wnr5", RunStatus::Alive), watch_run("llama-8b-sweep-b", RunStatus::Failed)],
            quiet: QuietHours::default(),
            ..Default::default()
        }),
    );
    feed(
        &mut s,
        0,
        Event::Runs {
            list: vec![
                crew_run("aa11bb22", "researcher", "LoRA rank ablations for 22B gates", CrewRunStatus::Running),
                crew_run("ee55ff66", "engineer", "fix the gate threshold off-by-one", CrewRunStatus::NeedsYou),
            ],
        },
    );
    type_str(&mut s, "what changed in the gate overnight?");
    s.on_key(key(KeyCode::Enter));
    let ws = Some("C:/work/valiance".to_string());
    feed(&mut s, 0, Event::SessionOpened { session: sid(), title: "what changed in the gate overnight?".into(), workspace: ws.clone() });
    feed(&mut s, 2, Event::TurnStarted { session: sid(), turn: 1, model: sonnet_ref(), user: "what changed in the gate overnight?".into() });
    feed(&mut s, 3, Event::Recalled { session: sid(), turn: 1, count: 3, summary: "valiance gate threshold history".into(), total_ms: 0, stages: Vec::new(), abstained: false });
    feed(&mut s, 4, Event::ToolStarted { session: sid(), turn: 1, call_id: "c1".into(), tool: "bash".into(), summary: "bash git log --since=yesterday".into() });
    feed(&mut s, 5, Event::ToolFinished { session: sid(), turn: 1, call_id: "c1".into(), ok: true, summary: "3 commits".into() });
    feed(&mut s, 6, Event::TextDelta { session: sid(), turn: 1, text: "Two commits touched the **gate**: the threshold moved to *0.42*, and the eval harness now pins the tokenizer.".into() });
    feed(&mut s, 7, Event::TurnFinished { session: sid(), turn: 1, usage: usage(), stop: TurnStop::Done });
    s
}

fn check(s: &State, w: u16, h: u16, name: &str) -> ratatui::buffer::Buffer {
    let buf = render(s, w, h);
    assert_palette_only(&buf);
    snapshot(name, &buf);
    buf
}

#[test]
fn main_screen_at_the_minimum_size() {
    let s = working();
    let g = geometry(Rect::new(0, 0, 120, 30), &s);
    assert_eq!(g.board_mode, BoardMode::Tabbed, "30 rows is too short to stack four sections");
    let buf = check(&s, 120, 30, "main_120x30");
    for want in [
        "Conversation · what changed in the gate overnight?",
        "Model",
        "Claude Sonnet 4.5 ▾",
        "medium",
        "^g Memory",
        "^l Library",
        "Workspace",
        "C:/work/valiance ▾",
        "1 Crew",
        "(1-4)",
        "Message",
        "(c)",
        "(i)",
        "(m)",
        "(w)",
        "^k palette",
        "21:40",
        "◐ engineer",
    ] {
        assert!(contains(&buf, want), "missing {want:?}\n{}", dump(&buf));
    }
}

#[test]
fn main_screen_stacks_the_board_when_there_is_height() {
    let s = working();
    for (w, h, name) in [(160, 45, "main_160x45"), (240, 60, "main_240x60")] {
        let g = geometry(Rect::new(0, 0, w, h), &s);
        assert_eq!(g.board_mode, BoardMode::Stacked, "{w}x{h}");
        assert!(g.board.width > 38, "a wider window gives the board more room");
        let buf = check(&s, w, h, name);
        for want in ["Crew", "(1)", "Watch", "(2)", "1 alive", "Threads", "(3)", "no open threads", "Memory", "(4)",
            "390 memories · 41 episodes", "3 recalled over 1 turn", "Friday 9 October · quiet 23:00–07:00 · 21:40"]
        {
            assert!(contains(&buf, want), "{w}x{h} missing {want:?}\n{}", dump(&buf));
        }
    }
    // Taller windows give the rows to the conversation.
    let a = geometry(Rect::new(0, 0, 160, 45), &s).conversation.height;
    let b = geometry(Rect::new(0, 0, 240, 60), &s).conversation.height;
    assert_eq!(b - a, 15);
}

#[test]
fn below_the_minimum_says_so() {
    let s = working();
    let buf = render(&s, 119, 30);
    assert!(contains(&buf, "needs a window of at least 120 × 30"));
    assert!(!contains(&buf, "Conversation"));
    assert_palette_only(&buf);
}

#[test]
fn esc_hands_the_keys_to_the_boxes_and_letters_come_back_as_text() {
    let mut s = working();
    assert_eq!(s.focus, Focus::Message);
    s.on_key(key(KeyCode::Esc));
    assert_eq!(s.focus, Focus::Conversation);
    check(&s, 120, 30, "focus_conversation");

    // Board sections by number; arrows move between them.
    s.on_key(key(KeyCode::Char('2')));
    assert_eq!((s.focus, s.board_tab), (Focus::Board, BoardTab::Watch));
    let buf = check(&s, 120, 30, "focus_board_watch");
    assert!(contains(&buf, "marlowe-valiance-22b-wnr5"));
    assert!(contains(&buf, "↵ open"));
    s.on_key(key(KeyCode::Right));
    assert_eq!(s.board_tab, BoardTab::Threads);
    // Enter opens the section's full view.
    s.on_key(key(KeyCode::Left));
    let reqs = s.on_key(key(KeyCode::Enter));
    assert_eq!(s.view, View::Watch);
    assert!(reqs.contains(&Request::WatchList));
    s.on_key(key(KeyCode::Esc));
    assert_eq!(s.view, View::Conversation);

    // A letter that is not a hotkey goes to the Message box and is typed.
    s.focus = Focus::Board;
    s.on_key(key(KeyCode::Char('h')));
    assert_eq!(s.focus, Focus::Message);
    assert_eq!(s.composer.text(), "h");

    // Tab walks the boxes in reading order.
    s.composer.clear();
    let mut seen = vec![s.focus];
    for _ in 0..5 {
        s.on_key(key(KeyCode::Tab));
        seen.push(s.focus);
    }
    assert_eq!(seen, [Focus::Message, Focus::Model, Focus::Workspace, Focus::Board, Focus::Conversation, Focus::Message]);

    // Alt-digit picks a section without leaving the Message box.
    s.on_key(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::ALT));
    assert_eq!((s.focus, s.board_tab), (Focus::Message, BoardTab::Memory));
    let buf = check(&s, 120, 30, "board_memory_tab");
    assert!(contains(&buf, "4 Memory"));
    assert!(contains(&buf, "390 memories · 41 episodes"));
}

#[test]
fn esc_still_interrupts_a_running_turn() {
    let mut s = working();
    feed(&mut s, 8, Event::TurnStarted { session: sid(), turn: 2, model: sonnet_ref(), user: "and the eval?".into() });
    s.now_ms += 12_000;
    let buf = check(&s, 120, 30, "busy_turn");
    assert!(contains(&buf, "esc Interrupt"), "the footer leads with the interrupt");
    assert!(contains(&buf, "thinking"));
    assert_eq!(s.on_key(key(KeyCode::Esc)), vec![Request::Interrupt { session: sid() }]);
    assert_eq!(s.focus, Focus::Message);
}

#[test]
fn m_opens_the_model_picker_under_its_box() {
    let mut s = working();
    s.on_key(key(KeyCode::Esc));
    s.on_key(key(KeyCode::Char('m')));
    assert!(matches!(s.overlay, Some(Overlay::ModelPicker { .. })));
    let g = geometry(Rect::new(0, 0, 160, 45), &s);
    let (m, p) = (g.model.unwrap(), g.picker.unwrap());
    assert_eq!(p.y, m.bottom(), "the dropdown hangs from the Model box");
    let buf = check(&s, 160, 45, "focus_model_picker");
    assert!(contains(&buf, "open the library…"));
    s.on_key(key(KeyCode::Esc));
    assert_eq!((s.overlay.is_none(), s.focus), (true, Focus::Message));
}

#[test]
fn the_workspace_picker_starts_new_sessions_elsewhere() {
    let mut s = working();
    // These paths are the test's own, not folders on this machine.
    s.wspick.check = |p| Ok(p.to_string());
    s.on_key(key(KeyCode::Esc));
    let reqs = s.on_key(key(KeyCode::Char('w')));
    assert_eq!(reqs, vec![Request::ListSessions { limit: 30 }]);
    assert_eq!(s.focus, Focus::Workspace);
    let buf = check(&s, 160, 45, "workspace_picker");
    assert!(contains(&buf, "C:/work/valiance  current"));
    assert!(contains(&buf, "D:/lab/PruningResearch/valiance  watched"));

    // Picking the watched root: a session is open, so the next message starts a new one there.
    type_str(&mut s, "pruning");
    s.on_key(key(KeyCode::Enter));
    assert!(s.overlay.is_none());
    assert_eq!(s.focus, Focus::Message);
    assert_eq!(s.default_workspace.as_deref(), Some(ROOT));
    assert!(s.session.is_none(), "a session keeps its workspace; a new one starts");
    assert!(contains(&render(&s, 160, 45), "…/PruningResearch/valiance ▾") || contains(&render(&s, 160, 45), "D:/lab/PruningResearch/valiance ▾"));
    type_str(&mut s, "hello");
    let reqs = s.on_key(key(KeyCode::Enter));
    assert!(matches!(&reqs[..], [Request::Ask { session: None, workspace: Some(w), .. }] if w == ROOT));

    // A typed path is offered first.
    s.on_key(key(KeyCode::Esc));
    s.on_key(key(KeyCode::Char('w')));
    s.on_paste("E:/scratch/new");
    assert_eq!(s.workspace_choices("E:/scratch/new")[0], "E:/scratch/new");
}

#[test]
fn clicks_focus_boxes_and_pick_sections() {
    let mut s = working();
    let area = Rect::new(0, 0, 120, 30);
    let click = |x, y| MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: x, row: y, modifiers: KeyModifiers::NONE };
    let g = geometry(area, &s);
    let t = g.tabs[BoardTab::Memory.index()];
    s.on_mouse(click(t.x, t.y), area);
    assert_eq!((s.focus, s.board_tab), (Focus::Board, BoardTab::Memory));
    s.on_mouse(click(g.conversation.x + 3, g.conversation.y + 3), area);
    assert_eq!(s.focus, Focus::Conversation);
    let c = g.composer.unwrap();
    s.on_mouse(click(c.x + 5, c.y + 1), area);
    assert_eq!(s.focus, Focus::Message);
    let w = g.workspace.unwrap();
    s.on_mouse(click(w.x + 3, w.y + 1), area);
    assert!(matches!(s.overlay, Some(Overlay::WorkspacePicker { .. })));
}

#[test]
fn every_view_keeps_the_footer_and_its_own_box() {
    let mut s = working();
    // alt-r opens the run that needs the user straight away; its box is titled with the run.
    for (chord, title) in [('l', "Library"), ('g', "Memory"), ('w', "Watch"), ('r', "Run · fix the gate")] {
        let press = |s: &mut marlowe_tui::State| s.on_key(if chord == 'r' { alt('r') } else { ctrl(chord) });
        press(&mut s);
        let buf = render(&s, 160, 45);
        assert!(contains(&buf, &format!("╭ {title}")), "{title}\n{}", dump(&buf));
        assert!(contains(&buf, "^k palette"), "{title} lost the footer");
        assert_palette_only(&buf);
        press(&mut s);
        assert_eq!(s.view, View::Conversation);
    }
}
