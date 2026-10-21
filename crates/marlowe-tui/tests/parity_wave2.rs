//! Parity with Claude Code for what wave 2 added to the daemon, in the terminal client: key-event
//! and mouse replay against the requests the daemon would get and the screens the user would see.
//! Rows: T33 `ask_user` previews, T34 the task list, M13 the plan file, G13 findings, WF7 the
//! workflow progress view, BG1 `/tasks`, BG4 monitor events, BG7 notifications, BG8 file cards,
//! BG11 message threads, U17 `/fast`, M14 rule suggestions, and the bench and eval reports. Every
//! frame keeps the palette (no backgrounds, no colour outside it).
#![allow(non_snake_case)]

mod common;

use common::*;
use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use marlowe_proto::{
    CrewBudget, CrewRunInfo, CrewRunStatus, CrewSpent, Event, FastState, Finding, FindingSeverity, NotifyState, PermissionMode,
    PlanFileDecision, Question, QuestionOption, Request, RuleAction, RuleSuggestion, RunId, SavedWorkflow, SentFile,
    SettingsScope, TaskInfo, TaskKind, TaskStatus, TodoItem, TodoStatus, Urgency, WorkflowAgentInfo, WorkflowAgentStatus,
    WorkflowInfo,
};
use marlowe_tui::bg::{ReportFile, ReportKind};
use marlowe_tui::coding::{Effect, LocalEvent};
use marlowe_tui::state::{Overlay, Target};
use marlowe_tui::{ClientEvent, Entry, State};

const NOW: i64 = 1_800_000_000_000;

fn local(s: &mut State, ev: LocalEvent) -> Vec<Request> {
    s.on_client(ClientEvent::Local(ev))
}

/// Clicks the centre-left of the last frame's `target`.
fn click(s: &mut State, target: &Target) -> Vec<Request> {
    let _ = render(s, W, H);
    let rect = s.hits.borrow().iter().rev().find(|(_, t)| t == target).map(|(r, _)| *r).unwrap_or_else(|| panic!("{target:?} is not on screen:\n{}", dump(&render(s, W, H))));
    let m = MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: rect.x + 3.min(rect.width - 1), row: rect.y, modifiers: KeyModifiers::NONE };
    s.on_mouse(m, ratatui::layout::Rect::new(0, 0, W, H))
}

/// A modal's type-ahead guard has passed.
fn settle(s: &mut State) {
    s.now_ms += 1_000;
}

// ---- T33 ----------------------------------------------------------------------------------------

fn layout_question() -> Event {
    Event::QuestionAsked {
        session: sid(),
        id: "q1".into(),
        questions: vec![Question {
            question: "Which layout should the settings screen use?".into(),
            header: "layout".into(),
            options: vec!["Tabs".into(), "Split".into(), "Single page".into()],
            multi_select: false,
            option_details: vec![
                QuestionOption { label: "Tabs".into(), description: "One pane at a time, switched along the top".into(), preview: Some("+-- general --+-- keys --+\n|  theme: dark             |\n+--------------------------+".into()) },
                QuestionOption { label: "Split".into(), description: "A list on the left, the setting on the right".into(), preview: Some("+---------+----------------+\n| general | theme: dark    |\n| keys    |                |\n+---------+----------------+".into()) },
                QuestionOption { label: "Single page".into(), description: String::new(), preview: None },
            ],
        }],
    }
}

#[test]
fn parity_T33_the_focused_options_preview_and_description_are_on_screen() {
    let mut s = coding_session();
    feed(&mut s, 3, layout_question());
    assert_eq!(s.overlay, Some(Overlay::Panel));
    let buf = palette_frame(&s);
    for want in ["Which layout", "Tabs", "One pane at a time", "A list on the left", "preview", "+-- general --+-- keys --+", "theme: dark"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    assert!(!contains(&buf, "| general | theme"), "only the focused option's preview shows:\n{}", dump(&buf));
    snapshot("ask_user_preview", &buf);
    // Moving the focus moves the preview.
    keys(&mut s, "down");
    let buf = palette_frame(&s);
    assert!(contains(&buf, "| general | theme: dark"), "{}", dump(&buf));
    assert!(!contains(&buf, "+-- general --+"), "{}", dump(&buf));
    // An option with no preview shows none.
    keys(&mut s, "down");
    let buf = palette_frame(&s);
    assert!(!contains(&buf, "theme: dark"), "{}", dump(&buf));
    // Narrow windows put the preview under the options instead of beside them.
    keys(&mut s, "up up");
    let narrow = render(&s, 120, 40);
    assert_palette_only(&narrow);
    assert!(contains(&narrow, "+-- general --+-- keys --+"), "{}", dump(&narrow));
}

#[test]
fn parity_T33_multi_select_returns_every_label_and_other_returns_what_was_typed() {
    let mut s = coding_session();
    feed(
        &mut s,
        3,
        Event::QuestionAsked {
            session: sid(),
            id: "q2".into(),
            questions: vec![
                Question {
                    question: "Which checks should run?".into(),
                    header: "checks".into(),
                    options: vec!["lint".into(), "unit".into(), "e2e".into()],
                    multi_select: true,
                    option_details: vec![],
                },
                Question { question: "Anything else?".into(), header: "extra".into(), options: vec!["no".into(), "yes".into()], multi_select: false, option_details: vec![] },
            ],
        },
    );
    settle(&mut s);
    // Space toggles; Enter moves to the next question.
    keys(&mut s, "space down down space enter");
    let buf = palette_frame(&s);
    assert!(contains(&buf, "Anything else?"), "{}", dump(&buf));
    // The free-text row is always offered; typing on it is the answer.
    keys(&mut s, "down down 'run the fuzzers too'");
    let reqs = keys(&mut s, "enter");
    assert_eq!(
        reqs,
        vec![Request::AnswerQuestion { session: sid(), id: "q2".into(), answers: vec!["lint, e2e".into(), "run the fuzzers too".into()] }]
    );
    assert_eq!(s.overlay, None);
}

// ---- T34 ----------------------------------------------------------------------------------------

fn item(id: &str, subject: &str, status: TodoStatus, owner: Option<&str>, blocked_by: &[&str]) -> TodoItem {
    TodoItem {
        content: subject.into(),
        status,
        active_form: None,
        id: Some(id.into()),
        description: String::new(),
        owner: owner.map(str::to_string),
        blocked_by: blocked_by.iter().map(|b| b.to_string()).collect(),
    }
}

#[test]
fn parity_T34_the_task_list_shows_ids_owners_and_what_blocks_an_item() {
    let mut s = coding_session();
    feed(
        &mut s,
        3,
        Event::Todos {
            session: sid(),
            items: vec![
                item("1", "map the gate module", TodoStatus::InProgress, Some("explore-1"), &[]),
                item("2", "write the fix", TodoStatus::Pending, None, &["1"]),
                item("3", "run the suite", TodoStatus::Pending, Some("conductor"), &["2"]),
            ],
        },
    );
    let buf = palette_frame(&s);
    for want in ["todos", "#1 map the gate module", "@explore-1", "#2 write the fix", "needs #1", "needs #2", "@conductor"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    snapshot("task_list_strip", &buf);
    // Once the blocker is completed the item is startable: no "needs".
    feed(
        &mut s,
        4,
        Event::Todos {
            session: sid(),
            items: vec![
                item("1", "map the gate module", TodoStatus::Completed, Some("explore-1"), &[]),
                item("2", "write the fix", TodoStatus::InProgress, None, &["1"]),
                item("3", "run the suite", TodoStatus::Pending, Some("conductor"), &["2"]),
            ],
        },
    );
    let buf = palette_frame(&s);
    assert!(!contains(&buf, "needs #1") && contains(&buf, "needs #2"), "{}", dump(&buf));
}

// ---- M13 ----------------------------------------------------------------------------------------

const PLAN_PATH: &str = "C:/Users/m/AppData/Local/MarloweCowork/plans/5e55-fix-the-gate.md";

fn plan_ready(id: &str, text: &str) -> Event {
    Event::PlanReady { session: sid(), id: id.into(), path: PLAN_PATH.into(), text: text.into() }
}

#[test]
fn parity_M13_the_plan_modal_shows_the_file_and_approval_picks_the_next_mode() {
    let mut s = coding_session();
    feed(&mut s, 3, plan_ready("p1", "# Fix the gate\n\n- read `gate::threshold`\n- include the upper bound\n- add a regression test\n"));
    assert_eq!(s.overlay, Some(Overlay::Panel));
    let buf = palette_frame(&s);
    for want in ["plan", "Fix the gate", "include the upper bound", "Yes, and accept edits", "Yes, and review each edit", "No, keep planning", "e edit the file"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    snapshot("plan_modal", &buf);
    // A key typed as the plan arrived is not an answer.
    assert!(keys(&mut s, "1").is_empty() && s.overlay == Some(Overlay::Panel));
    settle(&mut s);
    let reqs = keys(&mut s, "1");
    assert_eq!(reqs, vec![Request::AnswerPlan { session: sid(), id: "p1".into(), decision: PlanFileDecision::Approve { mode: PermissionMode::AcceptEdits } }]);
    assert_eq!(s.code.mode, PermissionMode::AcceptEdits);
    assert_eq!(s.overlay, None);
    // Option 2: the default mode, every edit reviewed.
    feed(&mut s, 4, plan_ready("p2", "# Second\n- a step"));
    settle(&mut s);
    let reqs = keys(&mut s, "down enter");
    assert_eq!(reqs, vec![Request::AnswerPlan { session: sid(), id: "p2".into(), decision: PlanFileDecision::Approve { mode: PermissionMode::Default } }]);
    assert_eq!(s.code.mode, PermissionMode::Default);
}

#[test]
fn parity_M13_rejecting_keeps_plan_mode_and_sends_the_feedback() {
    let mut s = coding_session();
    s.code.mode = PermissionMode::Plan;
    feed(&mut s, 3, plan_ready("p1", "# Plan\n- step"));
    settle(&mut s);
    keys(&mut s, "3");
    assert!(contains(&render(&s, W, H), "what should change?"));
    let reqs = keys(&mut s, "'use the old parser' enter");
    assert_eq!(reqs, vec![Request::AnswerPlan { session: sid(), id: "p1".into(), decision: PlanFileDecision::Reject { feedback: Some("use the old parser".into()) } }]);
    assert_eq!(s.code.mode, PermissionMode::Plan, "rejecting leaves plan mode on");
    assert_eq!(s.overlay, None);
    // Esc on the plan itself is a rejection without a reason.
    feed(&mut s, 4, plan_ready("p2", "# Plan\n- step"));
    settle(&mut s);
    let reqs = keys(&mut s, "esc");
    assert_eq!(reqs, vec![Request::AnswerPlan { session: sid(), id: "p2".into(), decision: PlanFileDecision::Reject { feedback: None } }]);
    // Esc inside the feedback line only goes back to the choices.
    feed(&mut s, 5, plan_ready("p3", "# Plan\n- step"));
    settle(&mut s);
    keys(&mut s, "n");
    assert!(keys(&mut s, "esc").is_empty());
    assert_eq!(s.overlay, Some(Overlay::Panel));
}

#[test]
fn parity_M13_the_user_edits_the_file_and_the_modal_shows_what_will_be_approved() {
    let mut s = coding_session();
    feed(&mut s, 3, plan_ready("p1", "# Original plan\n- step"));
    keys(&mut s, "e");
    assert_eq!(s.take_effects(), vec![Effect::OpenEditor { path: PLAN_PATH.into() }]);
    // The editor closing on the plan file reads it again; it does not reload instruction files.
    let reqs = local(&mut s, LocalEvent::EditorClosed { path: PLAN_PATH.into(), result: Ok(()) });
    assert!(reqs.is_empty(), "{reqs:?}");
    assert_eq!(s.take_effects(), vec![Effect::ReadFile { path: PLAN_PATH.into() }]);
    local(&mut s, LocalEvent::FileText { path: PLAN_PATH.into(), result: Ok("# Edited plan\n- a better step".into()) });
    let buf = palette_frame(&s);
    assert!(contains(&buf, "Edited plan") && contains(&buf, "a better step") && contains(&buf, "edited"), "{}", dump(&buf));
    assert!(!contains(&buf, "Original plan"));
    // The answer names the plan; the daemon reads the file's text as it stands.
    settle(&mut s);
    let reqs = keys(&mut s, "2");
    assert!(matches!(&reqs[..], [Request::AnswerPlan { id, .. }] if id == "p1"), "{reqs:?}");
}

#[test]
fn parity_M13_plans_and_questions_wait_their_turn_and_a_panel_comes_back() {
    let mut s = coding_session();
    keys(&mut s, "'/config' enter");
    feed(&mut s, 3, plan_ready("p1", "# One"));
    feed(&mut s, 4, plan_ready("p2", "# Two"));
    assert!(contains(&render(&s, W, H), "One") && !contains(&render(&s, W, H), "# Two"));
    settle(&mut s);
    keys(&mut s, "1");
    settle(&mut s);
    assert!(contains(&render(&s, W, H), "Two"), "the second plan is next:\n{}", dump(&render(&s, W, H)));
    keys(&mut s, "1");
    assert!(contains(&render(&s, W, H), "changes are written to the"), "the panel it covered returns");
}

#[test]
fn parity_M13_the_plan_modal_answers_the_mouse() {
    let mut s = coding_session();
    feed(&mut s, 3, plan_ready("p1", "# Plan\n- step"));
    settle(&mut s);
    let reqs = click(&mut s, &Target::Button(1));
    assert_eq!(reqs, vec![Request::AnswerPlan { session: sid(), id: "p1".into(), decision: PlanFileDecision::Approve { mode: PermissionMode::Default } }]);
}

// ---- G13 ----------------------------------------------------------------------------------------

fn finding(sev: FindingSeverity, lens: &str, file: &str, line: Option<u32>, claim: &str) -> Finding {
    Finding {
        lens: lens.into(),
        severity: sev,
        file: file.into(),
        line,
        claim: claim.into(),
        evidence: format!("{file} shows it: the bound is exclusive"),
        fix: Some("use `..=` for the upper bound".into()),
    }
}

fn review() -> Event {
    Event::Findings {
        session: Some(sid()),
        run: Some(RunId::from("wf1")),
        findings: vec![
            finding(FindingSeverity::Low, "reuse", "src/util.rs", Some(7), "a second copy of clamp()"),
            finding(FindingSeverity::High, "correctness", "src/gate/threshold.rs", Some(12), "the sweep stops one short of the upper bound"),
            finding(FindingSeverity::Medium, "tests", "tests/gate.rs", None, "no test covers the bound"),
        ],
    }
}

#[test]
fn parity_G13_findings_are_a_note_then_a_panel_worst_first() {
    let mut s = coding_session();
    feed(&mut s, 5, review());
    let buf = palette_frame(&s);
    assert!(contains(&buf, "review") && contains(&buf, "3 findings (1 high, 1 medium, 1 low)") && contains(&buf, "/findings"), "{}", dump(&buf));
    keys(&mut s, "'/findings' enter");
    assert_eq!(s.overlay, Some(Overlay::Panel));
    let buf = palette_frame(&s);
    let text = dump(&buf);
    let at = |p: &str| text.find(p).unwrap_or_else(|| panic!("{p:?}\n{text}"));
    assert!(at("the sweep stops one short") < at("no test covers the bound") && at("no test covers the bound") < at("a second copy of clamp()"), "{text}");
    for want in ["high", "correctness", "src/gate/threshold.rs:12", "evidence", "src/gate/threshold.rs shows it", "use `..=` for the upper bound"] {
        assert!(text.contains(want), "{want:?}\n{text}");
    }
    snapshot("findings_panel", &buf);
    // The file:line in the panel is a link for the terminal.
    assert!(s.code.link_plan.borrow().iter().any(|l| format!("{l:?}").contains("threshold.rs")), "{:?}", s.code.link_plan.borrow());
    // Down to the medium one; f puts the fix in the Message box for the user to send.
    keys(&mut s, "down f");
    assert_eq!(s.overlay, None);
    assert!(s.composer.text().starts_with("Fix this review finding in tests/gate.rs: no test covers the bound"), "{:?}", s.composer.text());
    // o opens the file in the editor.
    keys(&mut s, "ctrl-u '/findings' enter o");
    assert_eq!(s.take_effects(), vec![Effect::OpenEditor { path: "C:/work/valiance/src/gate/threshold.rs".into() }]);
}

#[test]
fn parity_G13_findings_asked_for_before_any_arrive_open_when_they_do() {
    let mut s = coding_session();
    let reqs = keys(&mut s, "'/findings' enter");
    assert_eq!(reqs, vec![Request::ListFindings { run: None, session: Some(sid()) }]);
    feed(&mut s, 0, review());
    assert_eq!(s.overlay, Some(Overlay::Panel));
    // Nothing found: a line, not an empty panel.
    let mut s = coding_session();
    keys(&mut s, "'/findings' enter");
    feed(&mut s, 0, Event::Findings { session: Some(sid()), run: None, findings: vec![] });
    assert_eq!(s.overlay, None);
    assert!(contains(&render(&s, W, H), "no findings in this session yet"));
}

// ---- WF7 ----------------------------------------------------------------------------------------

fn agent(index: u32, label: &str, phase: &str, status: WorkflowAgentStatus, tokens: u64, ms: u64) -> WorkflowAgentInfo {
    WorkflowAgentInfo { index, label: label.into(), phase: Some(phase.into()), status, tokens, elapsed_ms: ms }
}

fn wf_info() -> WorkflowInfo {
    WorkflowInfo {
        run: RunId::from("wf1"),
        name: "code-review".into(),
        phases: vec!["find".into(), "verify".into()],
        phase: Some("find".into()),
        agents: vec![
            agent(1, "correctness lens", "find", WorkflowAgentStatus::Done, 41_200, 62_000),
            agent(2, "security lens", "find", WorkflowAgentStatus::Running, 12_800, 31_000),
            agent(3, "verify finding 1", "verify", WorkflowAgentStatus::Queued, 0, 0),
        ],
        log: vec!["2 lenses chosen".into(), "correctness: 3 candidates".into()],
        budget_total: Some(500_000),
        budget_spent: 54_000,
    }
}

fn saved() -> Vec<SavedWorkflow> {
    vec![SavedWorkflow { name: "code-review".into(), description: "Review the diff for bugs with verified findings".into(), when_to_use: None, phases: vec![], source: "builtin".into() }]
}

fn run_info(id: &str, status: CrewRunStatus) -> CrewRunInfo {
    CrewRunInfo {
        id: RunId::from(id),
        session: Some(sid()),
        role: "workflow".into(),
        title: "code-review".into(),
        goal: "code-review".into(),
        playbook: None,
        status,
        workspace: Some("C:/work/valiance".into()),
        sandbox: None,
        budget: CrewBudget { tokens: Some(400_000), steps: Some(40), usd: Some(0.5) },
        spent: CrewSpent::default(),
        step: 3,
        current_action: None,
        note: None,
        tainted: false,
        created_ms: NOW - 120_000,
        updated_ms: NOW - 1_000,
        result: None,
        workflow: None,
    }
}

fn workflows_open() -> State {
    let mut s = coding_session();
    feed(&mut s, 0, Event::Runs { list: vec![run_info("wf1", CrewRunStatus::Running)] });
    let reqs = keys(&mut s, "'/workflows' enter");
    assert!(reqs.contains(&Request::ListWorkflows), "{reqs:?}");
    feed(&mut s, 0, Event::WorkflowList { saved: saved(), runs: vec![wf_info()] });
    s
}

#[test]
fn parity_WF7_the_workflows_view_lists_runs_and_saved_scripts() {
    let mut s = workflows_open();
    let buf = palette_frame(&s);
    for want in ["team tasks", "runs", "code-review", "running", "1 of 3 agents", "saved", "Review the diff for bugs", "builtin"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    snapshot("workflows_list", &buf);
    // Enter on the run opens its progress and watches it.
    let reqs = keys(&mut s, "enter");
    assert_eq!(reqs, vec![Request::WatchWorkflow { run: RunId::from("wf1") }]);
    // Enter on a saved script puts its start line in the Message box.
    let mut s = workflows_open();
    keys(&mut s, "down enter");
    assert_eq!(s.overlay, None);
    assert_eq!(s.composer.text(), "/team code-review ");
}

#[test]
fn parity_WF7_progress_is_grouped_by_phase_with_status_tokens_time_and_the_log_in_order() {
    let mut s = workflows_open();
    keys(&mut s, "enter");
    let buf = palette_frame(&s);
    let text = dump(&buf);
    for want in [
        "find",
        "verify",
        "1 of 3 agents done",
        "budget 54k of 500k",
        "#1",
        "correctness lens",
        "done",
        "41.2k tok",
        "1m02s",
        "security lens",
        "running",
        "verify finding 1",
        "queued",
        "log",
        "2 lenses chosen",
    ] {
        assert!(text.contains(want), "{want:?}\n{text}");
    }
    let at = |p: &str| text.find(p).unwrap_or_else(|| panic!("{p:?}\n{text}"));
    assert!(at("correctness lens") < at("security lens") && at("security lens") < at("verify finding 1"), "phases in order");
    snapshot("workflow_progress", &buf);
    // log() lines arrive in order, the newest at the bottom.
    feed(&mut s, 7, Event::WorkflowLog { run: RunId::from("wf1"), line: "security: 1 candidate".into() });
    feed(&mut s, 8, Event::WorkflowLog { run: RunId::from("wf1"), line: "merged 4 candidates".into() });
    let text = dump(&render(&s, W, H));
    let at = |p: &str| text.find(p).unwrap_or_else(|| panic!("{p:?}\n{text}"));
    assert!(at("2 lenses chosen") < at("correctness: 3 candidates") && at("correctness: 3 candidates") < at("security: 1 candidate") && at("security: 1 candidate") < at("merged 4 candidates"), "{text}");
    // An update replaces the agents.
    let mut info = wf_info();
    info.agents[1].status = WorkflowAgentStatus::Done;
    info.phase = Some("verify".into());
    feed(&mut s, 9, Event::WorkflowUpdated(info));
    assert!(contains(&render(&s, W, H), "2 of 3 agents done"));
}

#[test]
fn parity_WF7_skipping_an_agent_sends_its_index_and_shows_it_skipped() {
    let mut s = workflows_open();
    keys(&mut s, "enter");
    // The cursor starts on agent #1 (done); the second row is agent #2.
    let none = keys(&mut s, "s");
    assert!(none.is_empty(), "a finished agent cannot be skipped: {none:?}");
    assert!(contains(&render(&s, W, H), "already finished"));
    let reqs = keys(&mut s, "down s");
    assert_eq!(reqs, vec![Request::SkipWorkflowAgent { run: RunId::from("wf1"), agent: 2 }]);
    let buf = palette_frame(&s);
    assert!(contains(&buf, "skipped"), "{}", dump(&buf));
    // The daemon confirms; the view agrees.
    let mut info = wf_info();
    info.agents[1].status = WorkflowAgentStatus::Skipped;
    feed(&mut s, 9, Event::WorkflowUpdated(info));
    assert!(contains(&render(&s, W, H), "skipped"));
    // A queued agent can be skipped too.
    let reqs = keys(&mut s, "down s");
    assert_eq!(reqs, vec![Request::SkipWorkflowAgent { run: RunId::from("wf1"), agent: 3 }]);
}

#[test]
fn the_workflow_view_has_a_clickable_stop_control_with_the_same_confirm() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let mut s = workflows_open();
    keys(&mut s, "enter");
    let buf = render(&s, W, H);
    assert!(contains(&buf, "\u{25a0} stop run"), "{}", dump(&buf));
    let rect = s.hits.borrow().iter().rev().find(|(_, t)| *t == marlowe_tui::state::Target::StopRun).map(|(r, _)| *r).expect("stop control is registered");
    let m = MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: rect.x + 1, row: rect.y, modifiers: KeyModifiers::NONE };
    assert!(s.on_mouse(m, ratatui::layout::Rect::new(0, 0, W, H)).is_empty());
    let buf = render(&s, W, H);
    assert!(contains(&buf, "Stop this team task? Running agents stop; finished agents\u{2019} results are kept for a resume."), "{}", dump(&buf));
    assert_eq!(keys(&mut s, "y"), vec![Request::StopTask { id: "wf_wf1".into() }]);
}

#[test]
fn parity_WF7_stopping_the_run_asks_first() {
    let mut s = workflows_open();
    keys(&mut s, "enter");
    assert!(keys(&mut s, "x").is_empty());
    assert!(contains(&render(&s, W, H), "Stop this team task?"));
    assert!(keys(&mut s, "n").is_empty(), "anything but y keeps it going");
    keys(&mut s, "x");
    assert_eq!(keys(&mut s, "y"), vec![Request::StopTask { id: "wf_wf1".into() }]);
    // Esc goes back to the list, then closes.
    keys(&mut s, "esc");
    assert!(contains(&render(&s, W, H), "saved"));
    keys(&mut s, "esc");
    assert_eq!(s.overlay, None);
}

#[test]
fn parity_WF7_a_background_completion_that_starts_a_turn_is_news_not_you() {
    let mut s = coding_session();
    feed(
        &mut s,
        5,
        Event::TurnStarted {
            session: sid(),
            turn: 2,
            model: sonnet_ref(),
            user: "[crew \u{b7} run background \u{b7} workflow]\n[workflow wf_wf1 finished: 3 findings]\nreturned: {\"findings\": 3}".into(),
        },
    );
    assert!(matches!(s.entries.last(), Some(Entry::News { label, .. }) if label == "background \u{b7} workflow"), "{:?}", s.entries.last());
    let buf = palette_frame(&s);
    assert!(contains(&buf, "background \u{b7} workflow") && contains(&buf, "workflow wf_wf1 finished: 3 findings"), "{}", dump(&buf));
    assert!(!contains(&buf, "you  [crew"), "the user did not say it:\n{}", dump(&buf));
    // The window title is not the notice either.
    assert!(s.title.as_deref().is_none_or(|t| !t.contains("[crew")));
}

#[test]
fn parity_WF7_the_workflow_command_starts_a_run_and_opens_its_progress() {
    let mut s = coding_session();
    let reqs = keys(&mut s, "'/team code-review {\"level\":\"low\"}' enter");
    assert_eq!(
        reqs,
        vec![Request::RunWorkflow {
            session: Some(sid()),
            name: Some("code-review".into()),
            script_path: None,
            args: serde_json::json!({"level": "low"}),
            resume_from: None,
        }]
    );
    feed(&mut s, 0, Event::WorkflowUpdated(wf_info()));
    assert_eq!(s.overlay, Some(Overlay::Panel));
    assert!(contains(&render(&s, W, H), "security lens"));
    // Bad JSON is the user's to fix, not a request.
    let mut s = coding_session();
    assert!(keys(&mut s, "'/team code-review {oops' enter").is_empty());
    assert!(contains(&render(&s, W, H), "the arguments are JSON"));
}

#[test]
fn parity_WF7_rows_answer_the_mouse() {
    let mut s = workflows_open();
    // A first click on a saved row moves the cursor, a second does what Enter does.
    click(&mut s, &Target::Row(1));
    assert!(s.composer.text().is_empty());
    click(&mut s, &Target::Row(1));
    assert_eq!(s.composer.text(), "/team code-review ");
    let mut s = workflows_open();
    let reqs = click(&mut s, &Target::Row(0));
    assert_eq!(reqs, vec![Request::WatchWorkflow { run: RunId::from("wf1") }]);
}

// ---- BG1 ----------------------------------------------------------------------------------------

fn task(id: &str, kind: TaskKind, status: TaskStatus, title: &str, detail: Option<&str>) -> TaskInfo {
    TaskInfo {
        id: id.into(),
        kind,
        session: Some(sid()),
        title: title.into(),
        status,
        started_ms: NOW - 95_000,
        ended_ms: (status.is_terminal()).then_some(NOW - 5_000),
        output_file: Some(format!("C:/home/jobs/{id}.log")),
        detail: detail.map(str::to_string),
    }
}

fn all_kinds() -> Vec<TaskInfo> {
    let mut v = vec![
        task("job_j1", TaskKind::Job, TaskStatus::Running, "cargo test -p gate", None),
        task("agent_r7", TaskKind::Agent, TaskStatus::Running, "engineer: fix the gate", Some("step 4")),
        task("wf_wf1", TaskKind::Workflow, TaskStatus::Running, "code-review", Some("1 of 3 agents")),
        task("mon_m2", TaskKind::Monitor, TaskStatus::Waiting, "tail -f train.log", Some("armed")),
        task("cron_c1", TaskKind::Cron, TaskStatus::Waiting, "check the build", Some("next 09:00")),
        task("job_j0", TaskKind::Job, TaskStatus::Done, "npm run lint", Some("exit 0")),
    ];
    // They did not all start together: the strip shows the newest.
    for (t, ago) in v.iter_mut().zip([95_000, 80_000, 60_000, 40_000, 30_000, 120_000]) {
        t.started_ms = NOW - ago;
    }
    v
}

fn tasks_open() -> State {
    let mut s = coding_session();
    feed(&mut s, 6, Event::Tasks { list: all_kinds() });
    let reqs = keys(&mut s, "'/tasks' enter");
    assert_eq!(reqs, vec![Request::ListTasks { session: None }]);
    s
}

#[test]
fn parity_BG1_tasks_lists_one_of_each_kind_with_status_and_elapsed() {
    let s = tasks_open();
    let buf = palette_frame(&s);
    for want in ["tasks", "job_j1", "cargo test -p gate", "agent_r7", "team task", "wf_wf1", "monitor", "mon_m2", "cron", "cron_c1", "next 09:00", "running", "waiting", "done", "1m35s", "1m20s", "1m00s", "40s", "1m55s"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    let text = dump(&buf);
    let at = |p: &str| text.find(p).unwrap_or_else(|| panic!("{p:?}\n{text}"));
    assert!(at("agent_r7") < at("npm run lint"), "finished tasks come after the running ones");
    snapshot("tasks_panel", &buf);
}

#[test]
fn parity_BG1_tasks_are_stopped_filtered_and_read_from_the_keyboard() {
    let mut s = tasks_open();
    // Newest running first; x stops the selected live task.
    let reqs = keys(&mut s, "x");
    assert!(matches!(&reqs[..], [Request::StopTask { id }] if TaskKind::split(id).is_some()), "{reqs:?}");
    // The finished one cannot be stopped.
    keys(&mut s, "end");
    assert!(keys(&mut s, "x").is_empty());
    assert!(contains(&render(&s, W, H), "already finished"));
    // h hides the finished; tab narrows to one kind.
    keys(&mut s, "h");
    assert!(!contains(&render(&s, W, H), "npm run lint"));
    keys(&mut s, "tab");
    let buf = render(&s, W, H);
    assert!(contains(&buf, "kind job") && contains(&buf, "cargo test") && !contains(&buf, "agent    agent_r7"), "{}", dump(&buf));
    keys(&mut s, "tab tab tab tab tab");
    assert!(contains(&render(&s, W, H), "kind all"));
    // s: only this session's.
    let mut other = task("job_zz", TaskKind::Job, TaskStatus::Running, "someone else's job", None);
    other.session = Some(marlowe_proto::SessionId::from("other"));
    feed(&mut s, 8, Event::TaskUpdated(other));
    assert!(contains(&render(&s, W, H), "someone else's job") && contains(&render(&s, W, H), "other session"));
    keys(&mut s, "s");
    assert!(!contains(&render(&s, W, H), "someone else's job"));
}

#[test]
fn parity_BG1_enter_reads_a_tasks_output_and_keeps_reading_while_it_runs() {
    let mut s = tasks_open();
    let reqs = keys(&mut s, "down down down enter");
    assert_eq!(reqs, vec![Request::TaskOutput { id: "job_j1".into(), since: None, filter: None }]);
    feed(&mut s, 0, Event::TaskOutput { id: "job_j1".into(), text: "running 4 tests\ntest a ... ok\n".into(), next_since: 31, done: false });
    let buf = palette_frame(&s);
    assert!(contains(&buf, "running 4 tests") && contains(&buf, "test a ... ok") && contains(&buf, "job_j1"), "{}", dump(&buf));
    // Nothing more is asked inside the second, then once a second after.
    assert!(s.on_idle().iter().all(|r| !matches!(r, Request::TaskOutput { .. })));
    s.now_ms += 1_500;
    let reqs = s.on_idle();
    assert!(reqs.contains(&Request::TaskOutput { id: "job_j1".into(), since: Some(31), filter: None }), "{reqs:?}");
    // The answer is appended; a finished task stops the polling.
    feed(&mut s, 0, Event::TaskOutput { id: "job_j1".into(), text: "test b ... ok\n".into(), next_since: 45, done: true });
    assert!(contains(&render(&s, W, H), "test b ... ok") && contains(&render(&s, W, H), "test a ... ok"));
    s.now_ms += 5_000;
    assert!(s.on_idle().iter().all(|r| !matches!(r, Request::TaskOutput { .. })));
    // Esc goes back to the list.
    keys(&mut s, "esc");
    assert!(contains(&render(&s, W, H), "cargo test -p gate") && contains(&render(&s, W, H), "agent_r7"));
}

#[test]
fn parity_BG1_a_workflow_task_opens_its_progress_view() {
    let mut s = tasks_open();
    feed(&mut s, 0, Event::WorkflowUpdated(wf_info()));
    keys(&mut s, "down");
    let reqs = keys(&mut s, "w");
    assert_eq!(reqs, vec![Request::WatchWorkflow { run: RunId::from("wf1") }]);
    assert!(contains(&render(&s, W, H), "security lens"));
}

#[test]
fn parity_BG1_running_tasks_are_a_strip_under_the_conversation_and_a_click_opens_them() {
    let mut s = coding_session();
    feed(&mut s, 6, Event::Tasks { list: all_kinds() });
    let buf = palette_frame(&s);
    for want in ["background", "4 running", "/tasks", "agent_r7 engineer: fix the gate", "wf_wf1 code-review", "mon_m2 tail -f train.log", "1m20s"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    assert!(!contains(&buf, "npm run lint"), "finished work is not in the strip:\n{}", dump(&buf));
    assert!(!contains(&buf, "job_j1 cargo test"), "three at most, the newest:\n{}", dump(&buf));
    assert!(!contains(&buf, "check the build"), "a schedule is not work");
    snapshot("task_strip", &buf);
    // The status line counts what is not a job.
    assert!(contains(&buf, "3 tasks"), "{}", dump(&buf));
    // Clicking it opens /tasks.
    let reqs = click(&mut s, &Target::Chord(marlowe_tui::state::Command::Tasks));
    assert_eq!(s.overlay, Some(Overlay::Panel));
    assert_eq!(reqs, vec![Request::ListTasks { session: None }]);
    // A finished task leaves the strip.
    keys(&mut s, "esc");
    let mut done = task("agent_r7", TaskKind::Agent, TaskStatus::Done, "engineer: fix the gate", Some("done"));
    done.ended_ms = Some(NOW);
    feed(&mut s, 9, Event::TaskUpdated(done));
    let buf = render(&s, W, H);
    assert!(!contains(&buf, "agent_r7 engineer") && contains(&buf, "job_j1 cargo test"), "{}", dump(&buf));
}

#[test]
fn parity_BG1_task_rows_answer_the_mouse() {
    let mut s = tasks_open();
    click(&mut s, &Target::Row(1));
    let reqs = click(&mut s, &Target::Row(1));
    assert_eq!(reqs, vec![Request::TaskOutput { id: "wf_wf1".into(), since: None, filter: None }]);
}

// ---- BG4 ----------------------------------------------------------------------------------------

#[test]
fn parity_BG4_monitor_events_are_news_with_their_source_not_the_users_words() {
    let mut s = coding_session();
    feed(
        &mut s,
        5,
        Event::TurnStarted {
            session: sid(),
            turn: 2,
            model: sonnet_ref(),
            user: "[crew \u{b7} run background \u{b7} monitor]\n[monitor mon_m2 \u{b7} lines 1-3]\nepoch 4 loss 0.81\nepoch 5 loss 0.74\nepoch 6 loss 0.70".into(),
        },
    );
    let buf = palette_frame(&s);
    for want in ["background \u{b7} monitor", "epoch 4 loss 0.81", "epoch 6 loss 0.70"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    snapshot("monitor_news", &buf);
    // A long flood shows its tail and says where the rest is.
    let flood: String = (1..=30).map(|i| format!("line {i}\n")).collect();
    feed(&mut s, 6, Event::TurnFinished { session: sid(), turn: 2, stop: marlowe_proto::TurnStop::Done, usage: usage() });
    feed(&mut s, 7, Event::TurnStarted { session: sid(), turn: 3, model: sonnet_ref(), user: format!("[crew \u{b7} run background \u{b7} monitor]\n[monitor mon_m2 \u{b7} lines 4-33]\n{flood}") });
    assert!(contains(&render(&s, W, H), "more lines"), "{}", dump(&render(&s, W, H)));
    // A reopened session shows batched mid-turn news the same way.
    let turns = vec![marlowe_proto::StoredTurn {
        role: marlowe_model::Role::User,
        content: vec![marlowe_model::Part::Text { text: "[monitor mon_m2 ended: `tail -f train.log` exited with code 0 after 12 min]".into() }],
        at_ms: 1,
        ..stored_default()
    }];
    let mut s = coding_session();
    feed(&mut s, 5, Event::Transcript { session: sid(), turns });
    assert!(matches!(s.entries.first(), Some(Entry::News { label, .. }) if label == "monitor \u{b7} mon_m2"), "{:?}", s.entries);
    // Single-line news keeps its message: the exit code, the end reason, the wake-up reason.
    for (text, label, want) in [
        ("[job job_j1 exited with code 0 after 1.0s: `cargo test`]", "job \u{b7} job_j1", "exited with code 0 after 1.0s"),
        ("[wake-up cron_2: you asked to be woken \u{2014} check the eval]", "wake-up \u{b7} cron_2", "check the eval"),
        ("[monitor mon_a ended: `tail -f x` exited with code 0 after 12 min]", "monitor \u{b7} mon_a", "exited with code 0 after 12 min"),
    ] {
        let mut s = coding_session();
        feed(&mut s, 5, Event::TurnStarted { session: sid(), turn: 2, model: sonnet_ref(), user: text.into() });
        let buf = render(&s, W, H);
        assert!(contains(&buf, label) && contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
}

fn stored_default() -> marlowe_proto::StoredTurn {
    marlowe_proto::StoredTurn { idx: 0, role: marlowe_model::Role::User, content: vec![], at_ms: 0, phases: Vec::new(), numbers: None }
}

// ---- BG7 ----------------------------------------------------------------------------------------

#[test]
fn parity_BG7_the_toast_and_voice_switches_are_in_config_and_go_to_the_daemon() {
    let mut s = coding_session();
    // The connect-time query asks for them.
    let asked = s.on_idle();
    assert!(asked.contains(&Request::NotifySettings { toast: None, voice: None }), "{asked:?}");
    feed(&mut s, 0, Event::NotifyState(NotifyState { toast: true, voice: false }));
    keys(&mut s, "'/config' enter");
    feed(&mut s, 0, settings_event(&[(SettingsScope::User, "editor_url = \"vscode://file/{path}:{line}\"\n")]));
    let buf = palette_frame(&s);
    for want in ["notify.toast", "notify.voice", "a Windows toast", "daemon"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    // Toggle the toast: a NotifySettings request, never a settings write.
    let rows_before = 5; // editor_url, statusline.command, permissions.default_mode, network.allow_hosts, quit.choice
    let downs = "down ".repeat(rows_before);
    let reqs = keys(&mut s, &format!("{downs}enter"));
    assert_eq!(reqs, vec![Request::NotifySettings { toast: Some(false), voice: None }]);
    let reqs = keys(&mut s, "down enter");
    assert_eq!(reqs, vec![Request::NotifySettings { toast: None, voice: Some(true) }]);
    // The daemon's switch cannot be unset from a settings file.
    assert!(keys(&mut s, "x").is_empty());
    // The answer is shown.
    feed(&mut s, 0, Event::NotifyState(NotifyState { toast: false, voice: true }));
    let text = dump(&render(&s, W, H));
    let line = text.lines().find(|l| l.contains("notify.toast")).unwrap();
    assert!(line.contains("off"), "{line}");
}

#[test]
fn parity_BG7_a_notification_is_a_line_in_the_conversation_urgent_ones_in_amber() {
    let mut s = coding_session();
    feed(&mut s, 5, Event::Notified { session: Some(sid()), title: "Training done".into(), body: "eval loss 0.61".into(), urgency: Urgency::Normal });
    feed(&mut s, 6, Event::Notified { session: Some(sid()), title: "Disk almost full".into(), body: String::new(), urgency: Urgency::Urgent });
    let buf = palette_frame(&s);
    assert!(contains(&buf, "notification \u{b7} Training done \u{b7} eval loss 0.61") && contains(&buf, "notification \u{b7} Disk almost full"), "{}", dump(&buf));
    assert!(matches!(&s.entries[s.entries.len() - 1], Entry::Note { tone: marlowe_tui::state::NoteTone::Amber, .. }));
    // Another session's notification is a flash, not a line in this conversation.
    let before = s.entries.len();
    feed(&mut s, 7, Event::Notified { session: Some(marlowe_proto::SessionId::from("other")), title: "Elsewhere".into(), body: String::new(), urgency: Urgency::Normal });
    assert_eq!(s.entries.len(), before);
    assert!(s.flash.as_ref().is_some_and(|f| f.text.contains("Elsewhere")));
}

// ---- BG8 ----------------------------------------------------------------------------------------

fn sent(name: &str) -> SentFile {
    SentFile { name: name.into(), path: format!("C:/Users/m/AppData/Local/MarloweCowork/outbox/5e55/{name}"), bytes: 12_700, digest: "ab12".into(), note: "the loss curves for the last run".into() }
}

#[test]
fn parity_BG8_a_sent_file_is_a_card_in_the_conversation_and_in_files() {
    let mut s = coding_session();
    feed(&mut s, 5, Event::FileSent { session: sid(), file: sent("loss.html") });
    let buf = palette_frame(&s);
    for want in ["loss.html", "12 KB", "the loss curves for the last run", "outbox", "/files"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    assert!(matches!(s.entries.last(), Some(Entry::File { .. })));
    snapshot("file_card", &buf);
    // A page is opened by the daemon's OpenFile (outbox only), never by a link the terminal would
    // hand to a browser; text the terminal may show is an OSC 8 link, as the links policy says.
    assert!(!s.code.link_plan.borrow().iter().any(|l| l.url.contains("loss.html")), "{:?}", s.code.link_plan.borrow());

    // /files lists what was sent; Enter opens it, r shows it in Explorer.
    feed(&mut s, 6, Event::FileSent { session: sid(), file: sent("curves.png") });
    keys(&mut s, "'/files' enter");
    let buf = palette_frame(&s);
    assert!(contains(&buf, "curves.png") && contains(&buf, "loss.html"), "{}", dump(&buf));
    let reqs = keys(&mut s, "enter");
    assert_eq!(reqs, vec![Request::OpenFile { path: sent("curves.png").path, reveal: false }], "the newest is first");
    let reqs = keys(&mut s, "down r");
    assert_eq!(reqs, vec![Request::OpenFile { path: sent("loss.html").path, reveal: true }]);
    // Another session's file is not in this session's list.
    feed(&mut s, 7, Event::FileSent { session: marlowe_proto::SessionId::from("other"), file: sent("theirs.txt") });
    assert!(!contains(&render(&s, W, H), "theirs.txt"));
}

#[test]
fn parity_BG8_files_answer_the_mouse() {
    let mut s = coding_session();
    feed(&mut s, 5, Event::FileSent { session: sid(), file: sent("loss.html") });
    keys(&mut s, "'/files' enter");
    click(&mut s, &Target::Row(0));
    let reqs = click(&mut s, &Target::Row(0));
    assert_eq!(reqs, vec![Request::OpenFile { path: sent("loss.html").path, reveal: false }]);
}

// ---- U17 ----------------------------------------------------------------------------------------

#[test]
fn parity_U17_fast_toggles_the_quick_model_and_the_model_box_says_so() {
    let mut s = coding_session();
    assert_eq!(keys(&mut s, "'/fast' enter"), vec![Request::SetFast { session: sid(), on: true }]);
    let quick = catalog_models()[1].model_ref();
    feed(&mut s, 7, Event::FastChanged { session: sid(), state: FastState { on: true, model: Some(quick.clone()) } });
    let buf = palette_frame(&s);
    assert!(contains(&buf, "fast"), "{}", dump(&buf));
    assert!(s.flash.as_ref().is_some_and(|f| f.text.contains("fast on")), "{:?}", s.flash);
    // The indicator sits in the Model box, beside the effort.
    let row = model_row(&buf);
    assert!(row.contains("fast") && row.contains("medium") && row.contains("gpt-5-mini"), "{row}");
    // Toggling again turns it off; on and off are also words.
    assert_eq!(keys(&mut s, "'/fast' enter"), vec![Request::SetFast { session: sid(), on: false }]);
    feed(&mut s, 8, Event::FastChanged { session: sid(), state: FastState { on: false, model: Some(sonnet_ref()) } });
    assert!(!model_row(&render(&s, W, H)).contains("fast"));
    assert_eq!(keys(&mut s, "'/fast on' enter"), vec![Request::SetFast { session: sid(), on: true }]);
    assert!(keys(&mut s, "'/fast sideways' enter").is_empty());
    // Another session's change does not light this one's box.
    feed(&mut s, 9, Event::FastChanged { session: marlowe_proto::SessionId::from("other"), state: FastState { on: true, model: None } });
    assert!(!model_row(&render(&s, W, H)).contains("fast"));
}

/// The Model box's first row: the conductor, then what is true of it.
fn model_row(buf: &ratatui::buffer::Buffer) -> String {
    dump(buf).lines().find(|l| l.contains('\u{25be}') && l.contains("medium")).map(str::to_string).unwrap_or_default()
}

// ---- BG11 ---------------------------------------------------------------------------------------

#[test]
fn parity_BG11_messages_to_a_crew_run_are_a_thread_you_and_the_conductor_write_to() {
    let mut s = coding_session();
    feed(&mut s, 0, Event::Runs { list: vec![run_info("aa11bb22cc33dd44", CrewRunStatus::Running)] });
    // The conductor's send_message call, as the live events and then the stored transcript say it.
    feed(&mut s, 5, tool_started("c1", "send_message", "send_message stop at the unit tests"));
    feed(&mut s, 6, tool_finished("c1", true, "sent to aa11bb22cc33dd44"));
    let turns = vec![
        marlowe_proto::StoredTurn {
            role: marlowe_model::Role::Assistant,
            content: vec![marlowe_model::Part::ToolCall(marlowe_model::ToolCall {
                id: "c1".into(),
                name: "send_message".into(),
                args: serde_json::json!({"to": "aa11bb22cc33dd44", "text": "stop at the unit tests"}),
            })],
            ..stored_default()
        },
        marlowe_proto::StoredTurn {
            role: marlowe_model::Role::Tool,
            content: vec![marlowe_model::Part::ToolResult(marlowe_model::ToolResult { call_id: "c1".into(), content: "sent to aa11bb22cc33dd44".into(), is_error: false, parts: vec![] })],
            ..stored_default()
        },
    ];
    s.code.detail_refresh = Some(sid());
    feed(&mut s, 7, Event::Transcript { session: sid(), turns });
    // The Run view: t opens the thread; the steer line writes to it.
    keys(&mut s, "alt-r");
    let buf = render(&s, W, H);
    assert!(contains(&buf, "messages"), "the Run view offers the thread:\n{}", dump(&buf));
    keys(&mut s, "t");
    let buf = palette_frame(&s);
    for want in ["messages", "aa11bb22", "conductor", "stop at the unit tests", "delivered"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    // Enter writes to the run: it goes out as a steer and joins the thread.
    keys(&mut s, "enter");
    let reqs = keys(&mut s, "'skip the e2e suite' enter");
    assert_eq!(reqs, vec![Request::Steer { run: RunId::from("aa11bb22cc33dd44"), text: "skip the e2e suite".into() }]);
    let buf = palette_frame(&s);
    assert!(contains(&buf, "you") && contains(&buf, "skip the e2e suite"), "{}", dump(&buf));
    snapshot("message_thread", &buf);
    // A steer typed in the Run view is in the thread too.
    keys(&mut s, "esc");
    keys(&mut s, "s");
    keys(&mut s, "'also cargo fmt' enter");
    keys(&mut s, "t");
    assert!(contains(&render(&s, W, H), "also cargo fmt"));
}

// ---- M14 ----------------------------------------------------------------------------------------

#[test]
fn parity_M14_suggested_rules_are_picked_and_added_to_the_chosen_layer() {
    let mut s = coding_session();
    let reqs = keys(&mut s, "'/fewer-permission-prompts' enter");
    assert_eq!(reqs, vec![Request::SuggestRules { workspace: Some("C:/work/valiance".into()) }]);
    feed(
        &mut s,
        0,
        Event::RuleSuggestions {
            list: vec![
                RuleSuggestion { pattern: "bash(git status*)".into(), count: 5, example: "git status --short".into() },
                RuleSuggestion { pattern: "bash(rg *)".into(), count: 4, example: "rg -n gate src".into() },
            ],
        },
    );
    let buf = palette_frame(&s);
    for want in ["fewer permission prompts", "bash(git status*)", "5\u{d7} approved", "git status --short", "bash(rg *)", "project settings"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    // Pick the second, move the layer to local, add.
    keys(&mut s, "down space tab");
    let reqs = keys(&mut s, "enter");
    assert_eq!(reqs, vec![Request::AddRule { action: RuleAction::Allow, pattern: "bash(rg *)".into(), scope: SettingsScope::Local, workspace: Some("C:/work/valiance".into()) }]);
    assert_eq!(s.overlay, None);
    // Nothing to suggest is a line.
    let mut s = coding_session();
    keys(&mut s, "'/fewer-permission-prompts' enter");
    feed(&mut s, 0, Event::RuleSuggestions { list: vec![] });
    assert_eq!(s.overlay, None);
    assert!(contains(&render(&s, W, H), "nothing to suggest"));
}

// ---- bench and eval reports --------------------------------------------------------------------

const CARD: &str = r#"{"model":"ollama/marlowe-dusk:27b-super","local":true,"started_unix":1,
 "tools":{"prompts":200,"called":198,"selected":190,"args_ok":186,"schema_valid":196,"valid_rate":0.985,"repair_rate":0.04,"mean_ms":812.0,"misses":[]},
 "edits":{"formats":{"edit":{"cases":40,"first_applied":38,"succeeded":40,"first_rate":0.95,"success_rate":1.0,"mean_calls":1.1}},"recommended":["edit"],"retries":2},
 "memory":null,"research":null,"coding":{"runs":10,"passed":7,"pass_rate":0.7,"error":null},"skipped":[],
 "gates":[{"id":"A4","pass":true,"detail":"valid after repair 98.5%"},{"id":"A5","pass":false,"detail":"first attempt 88%"}]}"#;

#[test]
fn parity_bench_lists_cards_and_eval_reports_and_reads_one() {
    let mut s = coding_session();
    keys(&mut s, "'/bench' enter");
    assert_eq!(s.take_effects(), vec![Effect::ListReports]);
    let card = ReportFile { kind: ReportKind::Bench, path: "C:/home/bench/dusk-1.json".into(), name: "ollama-marlowe-dusk-1".into(), modified_ms: NOW - 3_600_000 };
    let eval = ReportFile { kind: ReportKind::Eval, path: "C:/home/eval/20261011-0100/report.md".into(), name: "20261011-0100".into(), modified_ms: NOW - 60_000 };
    local(&mut s, LocalEvent::Reports(vec![card.clone(), eval.clone()]));
    let buf = palette_frame(&s);
    let text = dump(&buf);
    for want in ["bench and eval", "ollama-marlowe-dusk-1", "20261011-0100", "1h ago", "1m ago"] {
        assert!(text.contains(want), "{want:?}\n{text}");
    }
    assert!(text.find("20261011-0100").unwrap() < text.find("ollama-marlowe-dusk-1").unwrap(), "newest first");
    // Enter on the (newer) eval report reads it.
    keys(&mut s, "enter");
    assert_eq!(s.take_effects(), vec![Effect::ReadReport { path: eval.path.clone() }]);
    local(&mut s, LocalEvent::ReportText { path: eval.path.clone(), result: Ok("# Coding eval: micro on dusk\n| arm | pass |\n|---|---|\n| marlowe-full | 7/10 |\nA1 PASS\nA3 FAIL\n".into()) });
    let buf = palette_frame(&s);
    for want in ["Coding eval: micro on dusk", "marlowe-full", "A1 PASS", "A3 FAIL"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    // Back, then the bench card: the numbers and the gates.
    keys(&mut s, "esc down enter");
    assert_eq!(s.take_effects(), vec![Effect::ReadReport { path: card.path.clone() }]);
    local(&mut s, LocalEvent::ReportText { path: card.path.clone(), result: Ok(CARD.into()) });
    let buf = palette_frame(&s);
    for want in ["ollama/marlowe-dusk:27b-super (local)", "200 prompts", "valid after repair 98.5%", "first attempt 95.0% (38/40)", "micro tasks passed 7/10", "A4   PASS", "A5   FAIL"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    snapshot("bench_report", &buf);
    keys(&mut s, "esc esc");
    assert_eq!(s.overlay, None);
    // A report that cannot be read says why.
    keys(&mut s, "'/bench' enter");
    local(&mut s, LocalEvent::Reports(vec![card.clone()]));
    keys(&mut s, "enter");
    local(&mut s, LocalEvent::ReportText { path: card.path, result: Err("access denied".into()) });
    assert!(contains(&render(&s, W, H), "could not read it: access denied"));
}

#[test]
fn parity_bench_with_no_reports_says_how_to_make_one() {
    let mut s = coding_session();
    keys(&mut s, "'/bench' enter");
    local(&mut s, LocalEvent::Reports(vec![]));
    assert!(contains(&render(&s, W, H), "marlowe bench <model>"));
}

// ---- the palette and /help -----------------------------------------------------------------------

#[test]
fn parity_u19_the_palette_and_help_list_what_wave_two_added() {
    let mut s = coding_session();
    keys(&mut s, "ctrl-k 'tasks' enter");
    assert_eq!(s.overlay, Some(Overlay::Panel));
    keys(&mut s, "esc");
    for (query, want) in [("teams", "loading"), ("files", "nothing sent yet"), ("bench", "looking"), ("findings", "")] {
        keys(&mut s, &format!("ctrl-k '{query}' enter"));
        if !want.is_empty() {
            assert!(contains(&render(&s, W, H), want), "{query}: {}", dump(&render(&s, W, H)));
        }
        keys(&mut s, "esc");
    }
    // A saved workflow and a project command appear in /help (U19).
    feed(&mut s, 0, Event::WorkflowList { saved: saved(), runs: vec![] });
    feed(&mut s, 0, Event::Commands { list: vec![marlowe_proto::CommandInfo { name: "deploy".into(), description: "ship it to staging".into(), argument_hint: Some("<env>".into()), source: ".marlowe/commands/deploy.md".into() }] });
    keys(&mut s, "'/help' enter");
    keys(&mut s, "end");
    let buf = render(&s, W, H);
    assert!(contains(&buf, "/team code-review") && contains(&buf, "/deploy <env>") && contains(&buf, "ship it to staging"), "{}", dump(&buf));
}

// ---- the palette, at every size -------------------------------------------------------------------

fn sweep_states() -> Vec<(&'static str, State)> {
    let mut out: Vec<(&'static str, State)> = Vec::new();
    out.push(("tasks", tasks_open()));
    let mut s = tasks_open();
    keys(&mut s, "down down down enter");
    feed(&mut s, 0, Event::TaskOutput { id: "job_j1".into(), text: (1..=80).map(|i| format!("line {i}\n")).collect(), next_since: 400, done: false });
    out.push(("task output", s));
    out.push(("workflows", workflows_open()));
    let mut s = workflows_open();
    keys(&mut s, "enter");
    out.push(("workflow run", s));
    let mut s = workflows_open();
    keys(&mut s, "enter x");
    out.push(("workflow stop", s));
    let mut s = coding_session();
    feed(&mut s, 5, review());
    keys(&mut s, "'/findings' enter");
    out.push(("findings", s));
    let mut s = coding_session();
    feed(&mut s, 5, Event::FileSent { session: sid(), file: sent("loss.html") });
    out.push(("file card", s.clone()));
    keys(&mut s, "'/files' enter");
    out.push(("files", s));
    let mut s = coding_session();
    let long: String = (1..=120).map(|i| format!("- step {i}\n")).collect();
    feed(&mut s, 3, plan_ready("p1", &format!("# A long plan\n{long}")));
    out.push(("plan", s.clone()));
    settle(&mut s);
    keys(&mut s, "pgdn pgdn 3 'why'");
    out.push(("plan feedback", s));
    let mut s = coding_session();
    keys(&mut s, "'/bench' enter");
    local(&mut s, LocalEvent::Reports(vec![ReportFile { kind: ReportKind::Bench, path: "C:/b/x.json".into(), name: "x".into(), modified_ms: NOW }]));
    out.push(("bench", s.clone()));
    keys(&mut s, "enter");
    local(&mut s, LocalEvent::ReportText { path: "C:/b/x.json".into(), result: Ok(CARD.into()) });
    out.push(("bench card", s));
    let mut s = coding_session();
    keys(&mut s, "'/fewer-permission-prompts' enter");
    feed(&mut s, 0, Event::RuleSuggestions { list: vec![RuleSuggestion { pattern: "bash(rg *)".into(), count: 4, example: "rg -n gate src".into() }] });
    out.push(("suggestions", s));
    let mut s = coding_session();
    feed(&mut s, 0, Event::Runs { list: vec![run_info("aa11bb22cc33dd44", CrewRunStatus::Running)] });
    s.bg.thread.push(marlowe_tui::bg::ThreadMsg {
        target: "aa11bb22cc33dd44".into(),
        from: marlowe_tui::bg::ThreadFrom::Conductor,
        text: "stop at the unit tests ".repeat(12),
        call_id: None,
        at_ms: NOW,
        delivery: marlowe_tui::bg::Delivery::Sent,
    });
    keys(&mut s, "'/messages' enter enter 'hello'");
    out.push(("thread", s));
    let mut s = coding_session();
    feed(&mut s, 3, layout_question());
    out.push(("question", s));
    let mut s = coding_session();
    keys(&mut s, "ctrl-q");
    out.push(("quit", s));
    let mut s = coding_session();
    feed(&mut s, 6, Event::Tasks { list: all_kinds() });
    feed(&mut s, 3, Event::Todos { session: sid(), items: vec![item("1", "a", TodoStatus::InProgress, Some("x"), &[]), item("2", "b", TodoStatus::Pending, None, &["1"])] });
    feed(&mut s, 5, Event::TurnStarted { session: sid(), turn: 2, model: sonnet_ref(), user: "[monitor mon_m2 \u{b7} lines 1-2]\nepoch 1\nepoch 2".into() });
    feed(&mut s, 6, Event::FileSent { session: sid(), file: sent("a.txt") });
    out.push(("conversation", s));
    out
}

#[test]
fn every_wave2_surface_draws_at_every_size_inside_the_palette() {
    for (name, s) in sweep_states() {
        for (w, h) in [(120, 30), (132, 36), (140, 40), (200, 60)] {
            let buf = render(&s, w, h);
            assert_palette_only(&buf);
            assert!(!dump(&buf).contains("needs a window"), "{name} at {w}x{h}");
        }
    }
}

#[test]
fn parity_G13_a_session_wide_list_and_a_runs_report_do_not_double_the_findings() {
    let mut s = coding_session();
    feed(&mut s, 5, review());
    // /findings asks for the session's: the answer replaces what was held.
    feed(&mut s, 0, Event::Findings { session: Some(sid()), run: None, findings: vec![finding(FindingSeverity::High, "correctness", "src/a.rs", Some(1), "one")] });
    assert_eq!(s.bg.findings.len(), 1);
    assert_eq!(s.bg.findings[0].findings.len(), 1);
    // A later review of a run supersedes the session-wide list.
    feed(&mut s, 6, review());
    assert_eq!(s.bg.findings.len(), 1);
    assert_eq!(s.bg.findings[0].findings.len(), 3);
    // Two different runs are two reports, and Tab walks between them.
    feed(&mut s, 7, Event::Findings { session: Some(sid()), run: Some(RunId::from("wf2")), findings: vec![finding(FindingSeverity::Low, "reuse", "src/b.rs", None, "two")] });
    assert_eq!(s.bg.findings.len(), 2);
    keys(&mut s, "'/findings' enter");
    assert!(contains(&render(&s, W, H), "1 finding (1 low)"));
    keys(&mut s, "tab");
    assert!(contains(&render(&s, W, H), "3 findings"));
}

#[test]
fn parity_BG8_a_sent_text_file_is_an_osc8_link_on_its_card() {
    let mut s = coding_session();
    feed(&mut s, 5, Event::FileSent { session: sid(), file: sent("notes.txt") });
    let _ = render(&s, W, H);
    assert!(s.code.link_plan.borrow().iter().any(|l| l.url.contains("notes.txt")), "{:?}", s.code.link_plan.borrow());
}
