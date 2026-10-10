//! The reducer: events in, state and requests out.

mod common;

use common::*;
use crossterm::event::KeyCode;
use marlowe_model::{Effort, Part, Role, ToolCall, ToolResult};
use marlowe_proto::{ApprovalDecision, ApprovalId, Event, Request, SessionId, Slot, StoredTurn, TurnStop};
use marlowe_tui::library::{LibFocus, LibStep};
use marlowe_tui::state::{Activity, ToolStatus};
use marlowe_tui::{ClientEvent, Conn, Entry, State, View};

fn in_session() -> State {
    let mut s = state();
    welcome(&mut s, 0);
    feed(&mut s, 1, library_with_sonnet());
    type_str(&mut s, "hello");
    s.on_key(key(KeyCode::Enter));
    // The daemon broadcasts the new session, then confirms it to the asking client with seq 0.
    feed(&mut s, 2, Event::SessionOpened { session: sid(), title: String::new(), workspace: None });
    feed(&mut s, 0, Event::SessionOpened { session: sid(), title: String::new(), workspace: None });
    feed(&mut s, 3, Event::TurnStarted { session: sid(), turn: 1, model: sonnet_ref(), user: "hello".into() });
    s
}

#[test]
fn welcome_requests_the_library_and_goes_online() {
    let mut s = state();
    assert_eq!(s.conn, Conn::Connecting);
    let reqs = welcome(&mut s, 0);
    assert_eq!(reqs, welcome_requests());
    assert!(s.conn.is_online());
}

#[test]
fn enter_sends_shift_enter_breaks_the_line() {
    let mut s = state();
    welcome(&mut s, 0);
    feed(&mut s, 1, library_with_sonnet());
    type_str(&mut s, "line one");
    assert!(s.on_key(shift(KeyCode::Enter)).is_empty());
    type_str(&mut s, "line two");
    let reqs = s.on_key(key(KeyCode::Enter));
    assert_eq!(
        reqs,
        vec![Request::Ask {
            session: None,
            text: "line one\nline two".into(),
            workspace: Some("C:/work/valiance".into()),
            images: Vec::new(),
            options: None,
        }]
    );
    assert!(s.composer.text().is_empty());
    assert_eq!(s.pending_user.as_deref(), Some("line one\nline two"));
}

#[test]
fn the_new_session_is_adopted_and_the_turn_streams() {
    let mut s = in_session();
    assert_eq!(s.session, Some(sid()));
    assert!(s.pending_user.is_none());
    assert_eq!(s.entries, vec![Entry::User { text: "hello".into(), steer: false }]);
    assert_eq!(s.activity(), Activity::Thinking);

    feed(&mut s, 4, Event::TextDelta { session: sid(), turn: 1, text: "Hi".into() });
    feed(&mut s, 5, Event::TextDelta { session: sid(), turn: 1, text: " there.".into() });
    assert_eq!(s.entries.last(), Some(&Entry::Prose { text: "Hi there.".into() }));
    assert_eq!(s.activity(), Activity::Writing);

    // A later message in a busy session is held, then goes in as a steer at the next step boundary.
    type_str(&mut s, "also check CI");
    assert!(s.on_key(key(KeyCode::Enter)).is_empty());
    feed(&mut s, 6, Event::ToolStarted { session: sid(), turn: 1, call_id: "c1".into(), tool: "read".into(), summary: "read a".into() });
    let reqs = feed(&mut s, 7, Event::ToolFinished { session: sid(), turn: 1, call_id: "c1".into(), ok: true, summary: "ok".into() });
    assert_eq!(reqs, vec![Request::Ask { session: Some(sid()), text: "also check CI".into(), workspace: None, images: Vec::new(), options: None }]);
    assert_eq!(s.entries.last(), Some(&Entry::User { text: "also check CI".into(), steer: true }));
}

#[test]
fn replayed_events_are_applied_once() {
    let mut s = in_session();
    feed(&mut s, 4, Event::TextDelta { session: sid(), turn: 1, text: "a".into() });
    feed(&mut s, 4, Event::TextDelta { session: sid(), turn: 1, text: "a".into() });
    feed(&mut s, 3, Event::TextDelta { session: sid(), turn: 1, text: "old".into() });
    assert_eq!(s.entries.last(), Some(&Entry::Prose { text: "a".into() }));
    assert_eq!(s.last_seq, 4);
}

#[test]
fn other_sessions_do_not_leak_into_this_one() {
    let mut s = in_session();
    let other = SessionId::from("other");
    feed(&mut s, 4, Event::SessionOpened { session: other.clone(), title: "x".into(), workspace: None });
    feed(&mut s, 5, Event::TextDelta { session: other, turn: 1, text: "nope".into() });
    assert_eq!(s.session, Some(sid()));
    assert!(!s.entries.iter().any(|e| matches!(e, Entry::Prose { text } if text == "nope")));
}

#[test]
fn another_clients_new_session_is_not_adopted_while_ours_is_pending() {
    let mut s = state();
    welcome(&mut s, 0);
    feed(&mut s, 1, library_with_sonnet());
    type_str(&mut s, "hello");
    s.on_key(key(KeyCode::Enter));
    let other = SessionId::from("other");
    feed(&mut s, 2, Event::SessionOpened { session: other.clone(), title: String::new(), workspace: None });
    feed(&mut s, 3, Event::TurnStarted { session: other.clone(), turn: 0, model: sonnet_ref(), user: "theirs".into() });
    assert_eq!(s.session, None, "broadcasts for another session are not ours");
    feed(&mut s, 0, Event::SessionOpened { session: sid(), title: String::new(), workspace: None });
    assert_eq!(s.session, Some(sid()));
}

#[test]
fn a_replay_gap_error_reloads_the_open_session() {
    let mut s = in_session();
    welcome(&mut s, 50);
    let reqs = feed(&mut s, 0, Event::Error { session: None, message: "replay gap: events 4..40 are no longer buffered".into() });
    assert_eq!(reqs, vec![Request::OpenSession { session: sid() }]);
    assert!(s.flash.is_none());
}

#[test]
fn retracted_text_becomes_thinking() {
    let mut s = in_session();
    feed(&mut s, 4, Event::TextDelta { session: sid(), turn: 1, text: "<think>hmm".into() });
    feed(&mut s, 5, Event::TextRetracted { session: sid(), turn: 1, tokens: 3 });
    assert_eq!(s.entries.last(), Some(&Entry::Thinking { text: "<think>hmm".into(), tokens: 3, live: false }));
}

#[test]
fn reasoning_tokens_sum_per_delta_and_a_late_settlement_lands_on_the_thinking_line() {
    let mut s = in_session();
    feed(&mut s, 4, Event::ReasoningDelta { session: sid(), turn: 1, text: "a".into(), tokens: 1 });
    feed(&mut s, 5, Event::ReasoningDelta { session: sid(), turn: 1, text: "b".into(), tokens: 1 });
    feed(&mut s, 6, Event::TextDelta { session: sid(), turn: 1, text: "ready".into() });
    feed(&mut s, 7, Event::ReasoningDelta { session: sid(), turn: 1, text: String::new(), tokens: 10 });
    let thinking: Vec<u64> = s
        .entries
        .iter()
        .filter_map(|e| match e {
            Entry::Thinking { tokens, .. } => Some(*tokens),
            _ => None,
        })
        .collect();
    assert_eq!(thinking, vec![12]);
    assert_eq!(s.entries.last(), Some(&Entry::Prose { text: "ready".into() }));
}

#[test]
fn escape_interrupts_a_running_turn() {
    let mut s = in_session();
    assert_eq!(s.on_key(key(KeyCode::Esc)), vec![Request::Interrupt { session: sid() }]);
    feed(&mut s, 4, Event::TurnFinished { session: sid(), turn: 1, usage: Default::default(), stop: TurnStop::Interrupted });
    assert!(!s.is_busy());
    assert!(matches!(s.entries.last(), Some(Entry::Note { text, .. }) if text == "interrupted"));
    assert!(s.on_key(key(KeyCode::Esc)).is_empty(), "nothing to interrupt");
}

#[test]
fn a_finished_turn_closes_dangling_tools() {
    let mut s = in_session();
    feed(&mut s, 4, Event::ToolStarted { session: sid(), turn: 1, call_id: "c".into(), tool: "read".into(), summary: "read a".into() });
    feed(&mut s, 5, Event::TurnFinished { session: sid(), turn: 1, usage: Default::default(), stop: TurnStop::Error });
    assert!(matches!(&s.entries[1], Entry::Tool { status: ToolStatus::Failed(_), .. }));
}

#[test]
fn reconnect_with_a_gap_reloads_the_session() {
    let mut s = in_session();
    s.on_client(ClientEvent::Disconnected { reason: "lost".into(), attempt: 1 });
    assert!(!s.conn.is_online());
    assert_eq!(s.activity(), Activity::Offline);
    let reqs = welcome(&mut s, 9);
    assert_eq!(reqs, welcome_requests());
    // seq 4..8 fell out of the ring buffer: the first replayed event is 9.
    let reqs = feed(&mut s, 9, Event::TextDelta { session: sid(), turn: 1, text: "x".into() });
    assert_eq!(reqs, vec![Request::OpenSession { session: sid() }]);
}

#[test]
fn reconnect_without_a_gap_just_continues() {
    let mut s = in_session();
    welcome(&mut s, 4);
    let reqs = feed(&mut s, 4, Event::TextDelta { session: sid(), turn: 1, text: "x".into() });
    assert!(reqs.is_empty());
    assert_eq!(s.entries.last(), Some(&Entry::Prose { text: "x".into() }));
}

#[test]
fn a_restarted_daemon_reloads_and_drops_the_lost_turn() {
    let mut s = in_session();
    let reqs = welcome(&mut s, 0);
    assert!(reqs.contains(&Request::OpenSession { session: sid() }));
    assert!(!s.is_busy());
    assert_eq!(s.last_seq, 0);
    // The transcript replaces the view.
    let turns = vec![
        StoredTurn { idx: 0, role: Role::User, content: vec![Part::Text { text: "hello".into() }], at_ms: 0 },
        StoredTurn {
            idx: 1,
            role: Role::Assistant,
            content: vec![
                Part::Text { text: "Reading.".into() },
                Part::ToolCall(ToolCall { id: "c1".into(), name: "read".into(), args: serde_json::json!({"path": "a.rs"}) }),
            ],
            at_ms: 0,
        },
        StoredTurn {
            idx: 2,
            role: Role::Tool,
            content: vec![Part::ToolResult(ToolResult { call_id: "c1".into(), content: "x\ny".into(), is_error: false, parts: Vec::new() })],
            at_ms: 0,
        },
    ];
    feed(&mut s, 0, Event::Transcript { session: sid(), turns });
    assert_eq!(
        s.entries,
        vec![
            Entry::User { text: "hello".into(), steer: false },
            Entry::Prose { text: "Reading.".into() },
            Entry::Tool { call_id: "c1".into(), summary: "read a.rs".into(), status: ToolStatus::Done("2 lines".into()) },
        ]
    );
}

#[test]
fn offline_keeps_the_message() {
    let mut s = state();
    s.on_client(ClientEvent::Disconnected { reason: "down".into(), attempt: 1 });
    type_str(&mut s, "hold this");
    assert!(s.on_key(key(KeyCode::Enter)).is_empty());
    assert_eq!(s.composer.text(), "hold this");
    assert!(s.flash.is_some());
}

#[test]
fn a_failed_first_ask_gives_the_text_back() {
    let mut s = state();
    welcome(&mut s, 0);
    feed(&mut s, 1, library_with_sonnet());
    type_str(&mut s, "hello");
    s.on_key(key(KeyCode::Enter));
    feed(&mut s, 0, Event::Error { session: None, message: "no model for the conductor".into() });
    assert_eq!(s.composer.text(), "hello");
    assert!(s.pending_user.is_none());
}

#[test]
fn approvals_queue_and_resolve_from_any_client() {
    let mut s = in_session();
    for id in [1, 2] {
        feed(&mut s, 3 + id, Event::ApprovalRequested {
            approval: ApprovalId(id),
            session: sid(),
            tool: "bash".into(),
            summary: format!("cmd {id}"),
            detail: String::new(),
            run: None,
            proposed_rule: None,
        });
    }
    assert_eq!(s.activity(), Activity::Awaiting);
    // Another client answered the first.
    feed(&mut s, 6, Event::ApprovalResolved { approval: ApprovalId(1), decision: ApprovalDecision::Deny });
    assert_eq!(s.current_approval().map(|a| a.id), Some(ApprovalId(2)));
    // Deny is the default; Enter confirms it once the type-ahead guard has passed.
    s.now_ms += 1_000;
    let reqs = s.on_key(key(KeyCode::Enter));
    assert_eq!(reqs, vec![Request::Approve { approval: ApprovalId(2), decision: ApprovalDecision::Deny }]);
    assert!(s.approvals.is_empty());
}

fn ask_approval(s: &mut State, id: u64, detail: &str) {
    feed(s, 3 + id, Event::ApprovalRequested {
        approval: ApprovalId(id),
        session: sid(),
        tool: "bash".into(),
        summary: "bash: echo hi".into(),
        detail: detail.into(),
        run: None,
        proposed_rule: None,
    });
}

#[test]
fn type_ahead_cannot_answer_an_approval() {
    let mut s = in_session();
    type_str(&mut s, "have a l");
    ask_approval(&mut s, 1, "{\"command\": \"echo hi\"}");
    for k in [key(KeyCode::Char('o')), key(KeyCode::Char('a')), key(KeyCode::Char('y')), key(KeyCode::Enter)] {
        assert!(s.on_key(k).is_empty(), "a key typed as the modal opened answered it");
    }
    assert_eq!(s.approvals.len(), 1);
    // Later, a bare Enter denies: nothing moved the selection off Deny.
    s.now_ms += 1_000;
    assert_eq!(s.on_key(key(KeyCode::Enter)), vec![Request::Approve { approval: ApprovalId(1), decision: ApprovalDecision::Deny }]);
}

#[test]
fn allow_needs_the_whole_detail_to_have_been_shown() {
    let mut s = in_session();
    let script: Vec<String> = (0..30).map(|i| format!("echo {i}")).collect();
    let detail = serde_json::json!({ "command": script.join("\n") }).to_string();
    ask_approval(&mut s, 1, &detail);
    assert!(!s.approval_read);
    s.now_ms += 1_000;
    assert!(s.on_key(key(KeyCode::Char('y'))).is_empty(), "allow refused before reading");
    s.on_key(key(KeyCode::Left));
    s.on_key(key(KeyCode::Left));
    assert!(s.on_key(key(KeyCode::Enter)).is_empty());
    s.on_key(key(KeyCode::PageDown));
    s.on_key(key(KeyCode::PageDown));
    assert!(s.approval_read);
    assert!(contains(&render(&s, W, H), "echo 29"));
    assert_eq!(s.on_key(key(KeyCode::Char('y'))), vec![Request::Approve { approval: ApprovalId(1), decision: ApprovalDecision::Allow }]);
}

#[test]
fn palette_commands() {
    let mut s = in_session();
    s.on_key(ctrl('k'));
    type_str(&mut s, "new");
    assert!(s.on_key(key(KeyCode::Enter)).is_empty());
    assert!(s.session.is_none());
    assert!(s.entries.is_empty());
    assert!(s.overlay.is_none());

    s.on_key(ctrl('l'));
    assert_eq!(s.view, View::Library);
    s.on_key(ctrl('q'));
    assert!(s.quit);
}

#[test]
fn ctrl_c_clears_then_quits() {
    let mut s = state();
    type_str(&mut s, "abc");
    s.on_key(ctrl('c'));
    assert!(s.composer.text().is_empty());
    assert!(!s.quit);
    s.on_key(ctrl('c'));
    assert!(s.quit);
}

#[test]
fn slots_cycle_model_and_effort() {
    let mut s = in_session();
    s.on_key(ctrl('l'));
    assert_eq!(s.library.step, LibStep::Overview);
    assert_eq!(s.library.focus, LibFocus::Slots);
    // Conductor: effort Medium -> High.
    let reqs = s.on_key(key(KeyCode::Char('e')));
    assert_eq!(reqs, vec![Request::SetSlot { slot: Slot::Conductor, model: None, effort: Effort::High }]);
    // Conductor: inherit -> the one library model, explicitly.
    let reqs = s.on_key(key(KeyCode::Right));
    assert_eq!(reqs, vec![Request::SetSlot { slot: Slot::Conductor, model: Some(sonnet_ref()), effort: Effort::High }]);
    assert!(!s.library.slot_rows()[0].inherited);
    // The crew row is synthesised and settable.
    s.on_key(key(KeyCode::Down));
    let reqs = s.on_key(key(KeyCode::Right));
    assert_eq!(
        reqs,
        vec![Request::SetSlot { slot: Slot::Crew("general".into()), model: Some(sonnet_ref()), effort: Effort::Medium }]
    );
    // Removing a model from the models list.
    s.on_key(key(KeyCode::Tab));
    let reqs = s.on_key(key(KeyCode::Char('d')));
    assert_eq!(reqs, vec![Request::RemoveModel { model: sonnet_ref() }, Request::LibraryState]);
}

#[test]
fn catalog_errors_show_in_the_catalog() {
    let mut s = state();
    welcome(&mut s, 0);
    feed(&mut s, 1, empty_library());
    s.on_key(key(KeyCode::Enter));
    let reqs = s.on_key(key(KeyCode::Enter));
    assert_eq!(reqs, vec![Request::ListCatalog { provider: marlowe_model::ProviderKind::Ollama }]);
    feed(&mut s, 0, Event::Error { session: None, message: "Ollama is not running".into() });
    assert_eq!(s.library.catalog.error.as_deref(), Some("Ollama is not running"));
    let buf = render(&s, W, H);
    assert!(contains(&buf, "Ollama is not running"));
    assert_palette_only(&buf);
}

#[test]
fn paste_goes_to_the_composer_with_newlines_intact() {
    let mut s = state();
    s.on_paste("one\r\ntwo");
    assert_eq!(s.composer.text(), "one\ntwo");
}

#[test]
fn hostile_text_cannot_reach_a_cell_raw() {
    let mut s = in_session();
    feed(&mut s, 4, Event::ToolStarted {
        session: sid(),
        turn: 1,
        call_id: "c".into(),
        tool: "bash".into(),
        summary: "bash echo \u{1b}[2Kpwned".into(),
    });
    feed(&mut s, 5, Event::TextDelta { session: sid(), turn: 1, text: "ok \u{1b}]0;title\u{7}".into() });
    let buf = render(&s, W, H);
    let text = dump(&buf);
    assert!(!text.contains('\u{1b}'), "an escape reached the buffer");
    assert!(text.contains("<U+001B>"));
}
