//! Rendered screens, snapshotted with `TestBackend`, and the no-fill palette rule on every one.

mod common;

use common::*;
use crossterm::event::KeyCode;
use marlowe_model::ProviderKind;
use marlowe_proto::{ApprovalId, Event, LibraryModel, Request, Secret, Slot, TurnStop};
use marlowe_tui::library::LibStep;
use marlowe_tui::{State, View};

fn first_run() -> State {
    let mut s = state();
    welcome(&mut s, 0);
    feed(&mut s, 1, empty_library());
    s
}

#[test]
fn empty_first_run_screen() {
    let s = first_run();
    let buf = render(&s, W, H);
    assert!(contains(&buf, "You have no models. Add one?"));
    assert!(contains(&buf, "add a model ▾"), "the Model box points at the missing model");
    for section in ["Crew", "Watch", "Threads", "Memory", "no runs", "nothing watched", "no open threads"] {
        assert!(contains(&buf, section), "board is missing {section}");
    }
    assert!(contains(&buf, "press ↵ to add a model"));
    assert_palette_only(&buf);
    snapshot("first_run", &buf);
}

#[test]
fn greeting_once_a_model_exists() {
    let mut s = state();
    welcome(&mut s, 0);
    feed(&mut s, 1, library_with_sonnet());
    let buf = render(&s, W, H);
    assert!(contains(&buf, "Good evening."));
    assert!(contains(&buf, "Claude Sonnet 4.5 ▾"));
    assert_palette_only(&buf);
    snapshot("greeting", &buf);
}

#[test]
fn library_onboarding_flow() {
    let mut s = first_run();

    // Enter on an empty composer with no models opens the provider list.
    assert!(s.on_key(key(KeyCode::Enter)).is_empty());
    assert_eq!(s.view, View::Library);
    assert_eq!(s.library.step, LibStep::Providers { sel: 0 });
    let buf = render(&s, W, H);
    assert_eq!(dump(&buf).matches("coming soon").count(), 3);
    assert!(contains(&buf, "running at 127.0.0.1:11434"));
    assert!(contains(&buf, "needs an API key"));
    assert_palette_only(&buf);
    snapshot("library_providers", &buf);

    // Down skips Local GGUF (coming soon) and lands on OpenRouter, which has no key yet.
    s.on_key(key(KeyCode::Down));
    assert_eq!(s.library.step, LibStep::Providers { sel: 2 });
    s.on_key(key(KeyCode::Enter));
    assert!(matches!(s.library.step, LibStep::Key { provider: ProviderKind::OpenRouter, .. }));

    let secret = "sk-or-v1-0123456789secret";
    type_str(&mut s, secret);
    let buf = render(&s, W, H);
    assert!(!contains(&buf, "secret"), "the key must never render in clear");
    assert!(!format!("{s:?}").contains("secret"), "the key must never reach Debug output");
    assert!(contains(&buf, &"•".repeat(secret.len())));
    assert_palette_only(&buf);
    snapshot("library_key", &buf);

    let reqs = s.on_key(key(KeyCode::Enter));
    assert_eq!(
        reqs,
        vec![
            Request::SetCredential { provider: ProviderKind::OpenRouter, secret: Secret(secret.into()) },
            Request::LibraryState,
            Request::ListCatalog { provider: ProviderKind::OpenRouter },
        ]
    );
    let buf = render(&s, W, H);
    assert!(contains(&buf, "loading the catalog…"));

    feed(&mut s, 2, Event::Catalog { provider: ProviderKind::OpenRouter, models: catalog_models() });
    type_str(&mut s, "son");
    let buf = render(&s, W, H);
    assert!(contains(&buf, "1 of 3 models"));
    assert!(contains(&buf, "anthropic/claude-sonnet-4.5"));
    assert!(!contains(&buf, "qwen3-coder"));
    assert_palette_only(&buf);
    snapshot("library_catalog", &buf);

    let reqs = s.on_key(key(KeyCode::Enter));
    assert_eq!(
        reqs,
        vec![
            Request::AddModel { provider: ProviderKind::OpenRouter, model_id: "anthropic/claude-sonnet-4.5".into() },
            Request::LibraryState,
        ]
    );
    assert_eq!(s.library.step, LibStep::Overview);
    feed(&mut s, 3, library_with_sonnet());
    let buf = render(&s, W, H);
    for row in ["Conductor", "Crew · general", "Quick", "$3 / $15", "200k"] {
        assert!(contains(&buf, row), "overview is missing {row}");
    }
    assert!(!contains(&buf, "Reflection"), "memory runs no model: the reflection slot is hidden");
    assert_palette_only(&buf);
    snapshot("library_overview", &buf);

    // Esc returns to the conversation, which now greets.
    s.on_key(key(KeyCode::Esc));
    assert_eq!(s.view, View::Conversation);
}

/// A full turn: thinking, two tools (one failed), markdown prose, usage.
fn streamed_turn() -> State {
    let mut s = state();
    welcome(&mut s, 0);
    feed(&mut s, 1, library_with_sonnet());
    type_str(&mut s, "what changed in the gate overnight?");
    let reqs = s.on_key(key(KeyCode::Enter));
    assert!(matches!(&reqs[..], [Request::Ask { session: None, .. }]));
    feed(&mut s, 0, Event::SessionOpened { session: sid(), title: "what changed in the gate overnight?".into(), workspace: Some("C:/work/valiance".into()) });
    feed(&mut s, 2, Event::SessionOpened { session: sid(), title: "what changed in the gate overnight?".into(), workspace: Some("C:/work/valiance".into()) });
    feed(&mut s, 3, Event::TurnStarted { session: sid(), turn: 1, model: sonnet_ref(), user: "what changed in the gate overnight?".into() });
    feed(&mut s, 4, Event::ReasoningDelta { session: sid(), turn: 1, text: "The user wants the overnight diff. ".into(), tokens: 600 });
    feed(&mut s, 5, Event::ReasoningDelta { session: sid(), turn: 1, text: "Check the log and the gate module.".into(), tokens: 604 });
    s
}

/// ui-preview5: "a tool call directly under thinking (when thinking is expanded) reads as thinking
/// (same colour)". Expanded, the reasoning is the dim weight behind a dimmer rule and ends with a
/// blank row; the call under it has the accent's marker and the body weight.
#[test]
fn a_tool_call_under_expanded_thinking_has_its_own_ink_and_a_boundary() {
    use marlowe_surface::chrome::Ink;
    let mut s = streamed_turn();
    feed(&mut s, 6, Event::ToolStarted { session: sid(), turn: 1, call_id: "c1".into(), tool: "bash".into(), summary: "bash git log --since=yesterday".into() });
    feed(&mut s, 7, Event::ToolFinished { session: sid(), turn: 1, call_id: "c1".into(), ok: true, summary: "3 commits".into() });
    s.on_key(ctrl('t'));
    let buf = render(&s, W, H);
    let text = dump(&buf);
    let rows: Vec<&str> = text.lines().collect();
    let think = rows.iter().position(|r| r.contains("Check the log and the gate module.")).expect(&text);
    let tool = rows.iter().position(|r| r.contains("⋯ bash git log --since=yesterday")).expect(&text);
    assert_eq!(tool, think + 2, "one blank row between the reasoning and the call:
{text}");
    assert!(!rows[think + 1].contains('▏') && !rows[think + 1].contains('⋯'), "{text}");
    let th = theme();
    let cell_of = |row: usize, needle: &str| {
        let x = (0..buf.area.width)
            .find(|&x| {
                let mut l = String::new();
                for k in x..buf.area.width {
                    l.push_str(buf.cell((k, row as u16)).unwrap().symbol());
                }
                l.starts_with(needle)
            })
            .unwrap_or_else(|| panic!("{needle:?} not on row {row}"));
        buf.cell((x, row as u16)).unwrap().clone()
    };
    assert_eq!(cell_of(think, "Check").fg, Ink::Dim.color(&th), "reasoning is the dim weight");
    assert_eq!(cell_of(think, "▏").fg, Ink::Dimmer.color(&th));
    assert_eq!(cell_of(tool, "⋯").fg, Ink::Accent.color(&th), "the call's marker is the accent");
    assert_eq!(cell_of(tool, "bash git").fg, Ink::Body.color(&th), "the call is the body weight");
    assert_ne!(cell_of(tool, "bash git").fg, cell_of(think, "Check").fg);
    assert_palette_only(&buf);

    // Folded, the call sits straight under the marker as before.
    s.on_key(ctrl('t'));
    let text = dump(&render(&s, W, H));
    let rows: Vec<&str> = text.lines().collect();
    let mark = rows.iter().position(|r| r.contains("▸ thought")).expect(&text);
    assert!(rows[mark + 1].contains("⋯ bash git log"), "{text}");
}

#[test]
fn streamed_turn_with_thinking_tools_and_markdown() {
    let mut s = streamed_turn();
    let buf = render(&s, W, H);
    assert!(contains(&buf, "▸ thinking… 1,204 tokens"), "{}", dump(&buf));
    assert!(contains(&buf, "thinking"), "title bar activity");

    feed(&mut s, 6, Event::ToolStarted { session: sid(), turn: 1, call_id: "c1".into(), tool: "bash".into(), summary: "bash git log --since=yesterday".into() });
    feed(&mut s, 7, Event::ToolStarted { session: sid(), turn: 1, call_id: "c2".into(), tool: "read".into(), summary: "read src/gate.rs".into() });
    let buf = render(&s, W, H);
    assert!(contains(&buf, "▸ thought · 1,204 tokens"), "thinking closes when a tool starts");
    assert!(contains(&buf, "⋯ read src/gate.rs …"));
    assert!(contains(&buf, "working"));
    snapshot("streamed_turn_running", &buf);

    feed(&mut s, 8, Event::ToolFinished { session: sid(), turn: 1, call_id: "c1".into(), ok: true, summary: "3 commits".into() });
    feed(&mut s, 9, Event::ToolFinished { session: sid(), turn: 1, call_id: "c2".into(), ok: false, summary: "file not found".into() });
    for chunk in [
        "## Overnight\n\nTwo commits touched the **gate**:\n\n",
        "- `gate.rs` moved the threshold to *0.42*\n",
        "- the eval harness now pins the tokenizer\n\n",
        "The read of `src/gate.rs` failed — the file moved to `crates/gate/src/lib.rs`.",
    ] {
        let n = s.last_seq + 1;
        feed(&mut s, n, Event::TextDelta { session: sid(), turn: 1, text: chunk.into() });
    }
    let n = s.last_seq + 1;
    feed(&mut s, n, Event::TurnFinished { session: sid(), turn: 1, usage: usage(), stop: TurnStop::Done });

    let buf = render(&s, W, H);
    let text = dump(&buf);
    assert!(text.contains("you  what changed in the gate overnight?"));
    assert!(text.contains("⋯ bash git log --since=yesterday · 3 commits"));
    assert!(text.contains("⋯ read src/gate.rs · file not found"));
    assert!(text.contains("Overnight"), "markdown heading");
    assert!(!text.contains("## Overnight"), "markdown is rendered, not shown raw");
    assert!(text.contains("12.4k in · 830 out · 1.2k reasoning · $0.0496"));
    assert!(text.contains("idle"));
    assert_palette_only(&buf);
    snapshot("streamed_turn_done", &buf);

    // ^t opens the thinking.
    s.on_key(ctrl('t'));
    let buf = render(&s, W, H);
    assert!(contains(&buf, "▾ thought · 1,204 tokens"));
    assert!(contains(&buf, "Check the log and the gate module."));
}

#[test]
fn approval_modal() {
    let mut s = streamed_turn();
    feed(&mut s, 6, Event::ToolStarted { session: sid(), turn: 1, call_id: "c1".into(), tool: "powershell".into(), summary: "powershell Remove-Item build".into() });
    feed(&mut s, 7, Event::ApprovalRequested {
        approval: ApprovalId(7),
        session: sid(),
        tool: "powershell".into(),
        summary: "Remove-Item -Recurse build".into(),
        detail: "{\n  \"command\": \"Remove-Item -Recurse build\"\n}".into(),
        run: None,
        proposed_rule: None,
    });
    let buf = render(&s, W, H);
    for want in ["approval", "powershell", "wants to act", "Remove-Item -Recurse build", "allow", "allow for session", "deny", "needs you"] {
        assert!(contains(&buf, want), "approval modal is missing {want}");
    }
    assert_palette_only(&buf);
    snapshot("approval", &buf);

    // Keys go to the modal, not the composer; type-ahead right as it opens is ignored.
    assert!(s.on_key(key(KeyCode::Char('a'))).is_empty());
    s.now_ms += 1_000;
    let reqs = s.on_key(key(KeyCode::Char('a')));
    assert_eq!(
        reqs,
        vec![Request::Approve { approval: ApprovalId(7), decision: marlowe_proto::ApprovalDecision::AllowForSession }]
    );
    assert!(s.composer.text().is_empty());
    assert!(s.approvals.is_empty());
}

#[test]
fn narrow_terminal_notice() {
    let s = first_run();
    let buf = render(&s, 100, 24);
    assert!(contains(&buf, "needs a window of at least 120 × 30"));
    assert!(contains(&buf, "this one is 100 × 24 — resize to continue"));
    assert!(!contains(&buf, "Crew"));
    assert_palette_only(&buf);
    snapshot("narrow", &buf);
}

#[test]
fn palette_and_sessions_overlays() {
    let mut s = first_run();
    s.on_key(ctrl('k'));
    let buf = render(&s, W, H);
    assert!(contains(&buf, "New session"));
    assert!(contains(&buf, "Quit"));
    assert_palette_only(&buf);
    snapshot("palette", &buf);

    type_str(&mut s, "sess");
    let reqs = s.on_key(key(KeyCode::Enter));
    assert_eq!(reqs, vec![Request::ListSessions { limit: 30 }]);
    let now = s.now_ms;
    feed(&mut s, 0, Event::SessionList {
        sessions: vec![marlowe_proto::SessionSummary {
            id: sid(),
            title: "gate refactor review".into(),
            workspace: Some("C:/work/valiance".into()),
            created_ms: now - 7_200_000,
            updated_ms: now - 3_600_000,
            turns: 12,
        }],
    });
    let buf = render(&s, W, H);
    assert!(contains(&buf, "gate refactor review"));
    assert!(contains(&buf, "1h ago"));
    assert_palette_only(&buf);
    let reqs = s.on_key(key(KeyCode::Enter));
    assert_eq!(reqs, vec![Request::OpenSession { session: sid() }]);
}

#[test]
fn model_picker_from_the_model_box() {
    let mut s = state();
    welcome(&mut s, 0);
    let mut second = sonnet();
    second.provider = ProviderKind::Ollama;
    second.id = "qwen3:8b".into();
    second.display = "qwen3:8b".into();
    second.pricing = None;
    second.local = true;
    let lib = match library_with_sonnet() {
        Event::LibraryState { mut models, slots, providers } => {
            models.push(LibraryModel { model: second.model_ref(), info: second.clone(), added_ms: 0 });
            Event::LibraryState { models, slots, providers }
        }
        _ => unreachable!(),
    };
    feed(&mut s, 1, lib);
    let g = marlowe_tui::render::geometry(ratatui::layout::Rect::new(0, 0, W, H), &s);
    let m = g.model.expect("model picker hit box");
    let click = crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column: m.x + 1,
        row: m.y,
        modifiers: crossterm::event::KeyModifiers::NONE,
    };
    s.on_mouse(click, ratatui::layout::Rect::new(0, 0, W, H));
    let buf = render(&s, W, H);
    assert!(contains(&buf, "conductor"));
    assert!(contains(&buf, "qwen3:8b"));
    assert!(contains(&buf, "open the library…"));
    assert_palette_only(&buf);
    snapshot("model_picker", &buf);

    s.on_key(key(KeyCode::Down));
    let reqs = s.on_key(key(KeyCode::Enter));
    assert_eq!(
        reqs[0],
        Request::SetSlot { slot: Slot::Conductor, model: Some(second.model_ref()), effort: marlowe_model::Effort::Medium }
    );
    let buf = render(&s, W, H);
    assert!(contains(&buf, "qwen3:8b ▾"));
}

#[test]
fn offline_state_is_honest() {
    let mut s = state();
    s.on_client(marlowe_tui::ClientEvent::Disconnected { reason: "cannot reach the daemon".into(), attempt: 3 });
    let buf = render(&s, W, H);
    assert!(contains(&buf, "the daemon is not answering"));
    assert!(contains(&buf, "offline"));
    assert_palette_only(&buf);
}
