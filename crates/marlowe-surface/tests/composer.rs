//! **The composer: it grows, it scrolls, and a pasted wall is represented rather than expanded.**
//! §B10's *"multiline by default"*, which had a key binding and no rendering behind it until M3 F2.
//!
//! # Why these assertions are on a drawn `Buffer`
//!
//! The defect was never in the state. `app.input` held the whole message the entire time — every
//! character typed went in, `Shift-Enter` inserted its newline, and a test on `app.input` would
//! have passed on the broken build. What was wrong is that the field rendered **one `Line` into a
//! region one row tall**, so everything past the width was clipped and everything past the first
//! line was invisible. That is a fact about cells, so it is asserted on cells.

mod common;

use marlowe_surface::app::Key;
use marlowe_surface::commands::paste_marker;
use marlowe_surface::window::WindowApp;
use marlowe_surface::{render, window};

/// Rows of the message region that are inside its border.
fn message_rows(app: &marlowe_surface::app::App, w: u16, h: u16) -> Vec<String> {
    let area = ratatui::layout::Rect::new(0, 0, w, h);
    let c = render::chrome_for(app, area);
    let buf = common::frame(app, w, h);
    (c.message.y + 1..c.message.y + c.message.height - 1)
        .map(|y| common::row_text(&buf, y).trim_end().to_string())
        .collect()
}

/// Type into the message field.
///
/// **The focus has to be set first, and that is §B10 working rather than a nuisance.** The default
/// focus is a region where letters are hotkeys — never a text input — so a test that just pressed
/// keys had its `i` swallowed as the Message region's own hotkey and typed `rst` instead of
/// `first`. The first draft of this file did exactly that and read as a rendering bug.
fn type_str(app: &mut marlowe_surface::app::App, s: &str) {
    app.focus = marlowe_surface::region::RegionId::Message;
    for ch in s.chars() {
        app.on_key(Key::Char(ch));
    }
}

// ─── growth ───────────────────────────────────────────────────────────────────────────────────

#[test]
fn a_message_longer_than_the_field_wraps_instead_of_vanishing_off_the_edge() {
    let mut app = common::app();
    // Long enough to wrap several times at any supported width, and made of distinct words so the
    // assertion can name the part that used to be lost.
    let words: Vec<String> = (0..40).map(|i| format!("word{i:02}")).collect();
    type_str(&mut app, &words.join(" "));

    let rows = message_rows(&app, 120, 30);
    let text = rows.join("\n");

    assert!(text.contains("word00"), "the start of the message is gone:\n{text}");
    // **The control, and the whole point.** On the broken build `word39` was in `app.input` and on
    // no cell of the screen — one `Line`, clipped at the region's width.
    assert!(
        text.contains("word39"),
        "the end of the message never reached a cell — this is the defect this file exists for:\n{text}"
    );
    assert!(rows.len() > 1, "the field did not grow: {rows:?}");
}

#[test]
fn the_field_grows_only_to_the_cap_and_then_scrolls_to_what_is_being_typed() {
    let mut app = common::app();
    let words: Vec<String> = (0..200).map(|i| format!("w{i:03}")).collect();
    type_str(&mut app, &words.join(" "));

    let rows = message_rows(&app, 120, 30);
    assert_eq!(
        rows.len() as u16,
        render::INPUT_ROWS_MAX,
        "the composer must stop growing at the cap, or a long message eats the conversation it is \
         about to be sent into"
    );
    let text = rows.join("\n");
    // Pinned to the END. Scrolling to the top would show the user the beginning of a message they
    // finished writing a paragraph ago.
    assert!(text.contains("w199"), "the field is not scrolled to what is being typed:\n{text}");
    assert!(!text.contains("w000"), "the field is still showing the start:\n{text}");
}

#[test]
fn an_empty_composer_is_one_row_so_the_resting_layout_is_unchanged() {
    // The control for both tests above, and the reason `layout(area)` kept its one-argument shape:
    // sixteen call sites assert where regions sit, and every one of them means a resting frame.
    let app = common::app();
    let area = ratatui::layout::Rect::new(0, 0, 120, 30);
    assert_eq!(render::chrome_for(&app, area), render::layout(area));
}

#[test]
fn a_shift_enter_newline_is_a_line_break_rather_than_a_glyph() {
    // §B10 binds Shift-Enter to a newline. It used to render as an inline `⏎` because the field
    // could not break a line; a newline that draws as a character is not a newline.
    let mut app = common::app();
    type_str(&mut app, "first");
    app.on_key(Key::ShiftEnter);
    type_str(&mut app, "second");

    let rows = message_rows(&app, 120, 30);
    assert!(rows.len() >= 2, "a newline did not break the line: {rows:?}");
    assert!(rows[0].contains("first") && !rows[0].contains("second"), "{rows:?}");
    assert!(rows[1].contains("second"), "{rows:?}");
}

// ─── paste ────────────────────────────────────────────────────────────────────────────────────

#[test]
fn a_pasted_wall_is_represented_in_the_composer_and_sent_in_full() {
    let mut app = common::app();
    let wall: String = (0..340).map(|i| format!("line {i}\n")).collect();
    type_str(&mut app, "review this: ");
    app.paste(wall.clone());

    // The composer shows a chip, not 340 rows.
    let rows = message_rows(&app, 120, 30);
    assert_eq!(rows.len(), 1, "a chip must not grow the field: {rows:?}");
    assert!(rows[0].contains("+340 lines"), "no chip in the composer: {rows:?}");
    assert!(!rows[0].contains("line 200"), "the wall was inlined: {rows:?}");

    // ...and what is SENT is the whole thing. A composer affordance that survived into the
    // transcript would make the record disagree with what the model received.
    app.on_key(Key::Enter);
    let sent = app
        .drain_intents()
        .into_iter()
        .find_map(|i| match i {
            marlowe_view::Intent::Send(t) => Some(t),
            _ => None,
        })
        .expect("Enter sent nothing");
    assert!(sent.contains("review this:"), "the typed part was lost");
    assert!(sent.contains("line 339"), "the paste was NOT expanded — the chip was sent instead");
    assert!(!sent.contains("+340 lines"), "the chip survived into the sent text: {sent}");
}

#[test]
fn a_short_paste_goes_in_literally_because_a_chip_would_be_longer_than_it() {
    let mut app = common::app();
    app.focus = marlowe_surface::region::RegionId::Message;
    app.paste("two words".into());
    let rows = message_rows(&app, 120, 30);
    assert!(rows[0].contains("two words"), "{rows:?}");
    assert!(!rows[0].contains("Pasted"), "a two-word paste became a chip: {rows:?}");
}

#[test]
fn backspace_deletes_a_whole_chip_rather_than_breaking_it() {
    // Half a marker no longer matches, so the paste it stood for would be dropped and the literal
    // wreckage sent in its place.
    let mut app = common::app();
    let wall: String = (0..12).map(|i| format!("l{i}\n")).collect();
    app.paste(wall);
    assert!(message_rows(&app, 120, 30)[0].contains("Pasted"));

    app.on_key(Key::Backspace);
    let rows = message_rows(&app, 120, 30);
    assert!(!rows[0].contains("Pasted"), "one backspace left a broken chip: {rows:?}");
    assert!(!rows[0].contains('['), "wreckage left in the composer: {rows:?}");
}

#[test]
fn a_marker_the_user_typed_before_any_paste_is_left_alone() {
    // The forgeable claim, bounded. Expansion only matches markers this session minted, so with no
    // pastes there is nothing to substitute and the text is the user's own.
    let mut app = common::app();
    type_str(&mut app, &format!("look at {}", paste_marker(0, "a
b
c")));
    app.on_key(Key::Enter);
    let sent = app
        .drain_intents()
        .into_iter()
        .find_map(|i| match i {
            marlowe_view::Intent::Send(t) => Some(t),
            _ => None,
        })
        .expect("Enter sent nothing");
    assert_eq!(sent, format!("look at {}", paste_marker(0, "a
b
c")));
}

// ─── the steer field is the same composer ─────────────────────────────────────────────────────

#[test]
fn the_run_windows_steer_field_grows_and_scrolls_exactly_as_the_message_field_does() {
    // §B10 is a property of both composers or of neither. This is the cross-surface half: the
    // window had the same one-row clip, and a steer is the one place a person types a paragraph
    // into a run that is already going.
    let mut app = WindowApp::new(common::run_view());
    app.focus = marlowe_surface::region::RegionId::RunSteer;
    let words: Vec<String> = (0..40).map(|i| format!("word{i:02}")).collect();
    for ch in words.join(" ").chars() {
        app.on_key(Key::Char(ch));
    }

    let area = ratatui::layout::Rect::new(0, 0, 120, 30);
    let c = window::chrome_for(&app, area);
    let buf = common::window_frame(&app, 120, 30);
    let text: String = (c.steer.y + 1..c.steer.y + c.steer.height - 1)
        .map(|y| common::row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(text.contains("word00"), "the start of the steer is gone:\n{text}");
    assert!(text.contains("word39"), "the end of the steer never reached a cell:\n{text}");
    assert!(c.steer.height > 3, "the steer field did not grow: {:?}", c.steer);
}

#[test]
fn a_steer_field_that_grew_leaves_the_output_panel_smaller_rather_than_overlapping_it() {
    // The rows have to come from somewhere, and §6's window has exactly one region that can give
    // them up. An overlap would be two regions drawing the same cells, which no per-region
    // assertion would catch.
    let mut app = WindowApp::new(common::run_view());
    app.focus = marlowe_surface::region::RegionId::RunSteer;
    for ch in "x".repeat(600).chars() {
        app.on_key(Key::Char(ch));
    }
    let area = ratatui::layout::Rect::new(0, 0, 120, 30);
    let grown = window::chrome_for(&app, area);
    let resting = window::layout(area);

    assert!(grown.steer.height > resting.steer.height, "the field did not grow");
    assert!(
        grown.output.height < resting.output.height,
        "the field grew and nothing gave up the rows — the regions must be overlapping"
    );
    assert_eq!(
        grown.output.y + grown.output.height,
        grown.steer.y,
        "the output panel and the steer field are not flush; one is drawing over the other"
    );
}

// ─── editing chords ───────────────────────────────────────────────────────────────────────────

#[test]
fn ctrl_backspace_deletes_a_word_and_the_space_before_it() {
    let mut app = common::app();
    type_str(&mut app, "delete the last word ");
    app.on_key(Key::CtrlBackspace);
    // Trailing whitespace first, then the word — `hello world ` leaves `hello `, not `hello world`.
    assert!(message_rows(&app, 120, 30)[0].contains("delete the last"));
    assert!(!message_rows(&app, 120, 30)[0].contains("word"));
}

#[test]
fn ctrl_backspace_takes_a_whole_paste_chip_because_a_chip_is_one_word_to_the_eye() {
    let mut app = common::app();
    let wall: String = (0..12).map(|i| format!("l{i}\n")).collect();
    app.paste(wall);
    app.on_key(Key::CtrlBackspace);
    assert!(!message_rows(&app, 120, 30)[0].contains("Pasted"));
}

#[test]
fn ctrl_a_then_typing_replaces_the_whole_message() {
    let mut app = common::app();
    type_str(&mut app, "the old message");
    app.on_key(Key::Ctrl('a'));
    type_str(&mut app, "new");

    let row = message_rows(&app, 120, 30)[0].clone();
    assert!(row.contains("new"), "{row}");
    assert!(!row.contains("old"), "the selection did not replace: {row}");
}

#[test]
fn ctrl_a_does_not_survive_the_keystroke_that_acted_on_it() {
    // The control for the test above. A selection that outlived its own keystroke would make the
    // SECOND character wipe the buffer again, so `new` would render as `w`.
    let mut app = common::app();
    type_str(&mut app, "old");
    app.on_key(Key::Ctrl('a'));
    type_str(&mut app, "new");
    assert!(message_rows(&app, 120, 30)[0].contains("new"), "{:?}", message_rows(&app, 120, 30));
}

#[test]
fn esc_lets_go_of_a_selection_rather_than_backing_out_of_the_field() {
    let mut app = common::app();
    type_str(&mut app, "keep me");
    app.on_key(Key::Ctrl('a'));
    app.on_key(Key::Esc);
    type_str(&mut app, "!");
    // Esc released the selection, so the `!` appended instead of replacing.
    assert!(message_rows(&app, 120, 30)[0].contains("keep me!"), "{:?}", message_rows(&app, 120, 30));
}

#[test]
fn the_steer_field_answers_the_same_three_chords() {
    // §B10 is a property of both composers or of neither — the same rule the growth tests apply.
    let mut app = WindowApp::new(common::run_view());
    app.focus = marlowe_surface::region::RegionId::RunSteer;
    for ch in "stop and reconsider ".chars() {
        app.on_key(Key::Char(ch));
    }
    app.on_key(Key::CtrlBackspace);

    let area = ratatui::layout::Rect::new(0, 0, 120, 30);
    let c = window::chrome_for(&app, area);
    let buf = common::window_frame(&app, 120, 30);
    let row = common::row_text(&buf, c.steer.y + 1);
    assert!(row.contains("stop and"), "{row}");
    assert!(!row.contains("reconsider"), "ctrl-backspace did not delete the word: {row}");

    app.on_key(Key::Ctrl('a'));
    app.on_key(Key::Char('n'));
    let buf = common::window_frame(&app, 120, 30);
    let row = common::row_text(&buf, c.steer.y + 1);
    assert!(!row.contains("stop"), "ctrl-a did not replace the draft: {row}");
}

// ─── ^c: copy, else confirm ───────────────────────────────────────────────────────────────────

#[test]
fn ctrl_c_does_not_quit_on_one_press() {
    // **The regression that matters most in this file.** `^c` was `Action::Quit` — immediate,
    // advertised on no footer, asserted by no test — and `Ctrl-A` landing in the same session made
    // its worst case the ordinary one: select all, reach for copy, lose the draft.
    let mut app = common::app();
    assert_ne!(app.on_key(Key::Ctrl('c')), marlowe_surface::app::Action::Quit);
}

#[test]
fn two_consecutive_ctrl_c_presses_quit() {
    let mut app = common::app();
    app.on_key(Key::Ctrl('c'));
    assert_eq!(app.on_key(Key::Ctrl('c')), marlowe_surface::app::Action::Quit);
}

#[test]
fn a_keystroke_between_them_disarms_the_quit() {
    // A confirmation is a pair of CONSECUTIVE presses. One that survived an intervening keystroke
    // would turn a `^c` from five minutes ago into half of a quit.
    let mut app = common::app();
    app.on_key(Key::Ctrl('c'));
    app.on_key(Key::Tab);
    assert_ne!(
        app.on_key(Key::Ctrl('c')),
        marlowe_surface::app::Action::Quit,
        "an intervening key did not disarm the quit"
    );
}

#[test]
fn ctrl_c_over_a_draft_copies_it_and_does_not_arm_a_quit() {
    let mut app = common::app();
    type_str(&mut app, "a message worth keeping");
    assert_ne!(app.on_key(Key::Ctrl('c')), marlowe_surface::app::Action::Quit);
    assert_eq!(
        app.pending_copy.take().as_deref(),
        Some("a message worth keeping"),
        "the draft was not copied"
    );
    // ...and a second press still does not quit, because there is still something to copy.
    assert_ne!(app.on_key(Key::Ctrl('c')), marlowe_surface::app::Action::Quit);
}

#[test]
fn copying_a_draft_puts_the_pasted_text_on_the_clipboard_and_not_the_chip() {
    // A clipboard holding `[Pasted #1 +12 lines]` hands the user a label instead of their data.
    let mut app = common::app();
    let wall: String = (0..12).map(|i| format!("l{i}\n")).collect();
    app.paste(wall);
    app.on_key(Key::Ctrl('c'));
    let copied = app.pending_copy.take().expect("nothing was copied");
    assert!(copied.contains("l11"), "the chip was copied instead of the text: {copied}");
    assert!(!copied.contains("Pasted #"), "the chip reached the clipboard: {copied}");
}

#[test]
fn the_run_window_answers_ctrl_c_too_rather_than_ignoring_it() {
    // Same chord, two surfaces. It terminated one and did nothing at all in the other.
    let mut app = WindowApp::new(common::run_view());
    app.focus = marlowe_surface::region::RegionId::RunSteer;
    for ch in "hold on".chars() {
        app.on_key(Key::Char(ch));
    }
    app.on_key(Key::Ctrl('c'));
    assert_eq!(app.pending_copy.take().as_deref(), Some("hold on"));
}

#[test]
fn the_windows_ctrl_c_detaches_only_on_the_second_press_and_never_cancels() {
    // §6.5: closing a window detaches and never cancels. The confirmation is about the draft, not
    // about the run — but a window vanishing under a copy attempt is still the wrong outcome.
    let mut app = WindowApp::new(common::run_view());
    assert_ne!(app.on_key(Key::Ctrl('c')), marlowe_surface::window::Action::Close);
    assert_eq!(app.on_key(Key::Ctrl('c')), marlowe_surface::window::Action::Close);
    let asked = app.drain_requests();
    assert!(
        asked.iter().all(|r| *r != marlowe_surface::window::WindowRequest::Cancel),
        "^c asked the run to cancel: {asked:?}"
    );
}

// ─── ADR-056: Ctrl-V is paste ─────────────────────────────────────────────────────────────────

#[test]
fn ctrl_v_no_longer_cycles_the_status_band() {
    // The reason the chord had to move. Firing a §B5 demo because somebody pressed the paste
    // chord is the worst available outcome: the screen changes, nothing is pasted, and the change
    // reads as though the paste worked.
    let mut app = common::app();
    let before = app.view().status.state;
    app.on_key(Key::Ctrl('v'));
    assert_eq!(app.view().status.state, before, "Ctrl-V still cycles the band");
    assert!(app.drain_intents().is_empty(), "Ctrl-V still asks the producer for something");
}

#[test]
fn ctrl_v_reaching_the_app_names_the_fallback_rather_than_doing_nothing() {
    // It only arrives on a terminal that did NOT paste — Windows Terminal binds the chord and
    // delivers `Event::Paste`. There is no portable clipboard read from a TUI, so the honest
    // answer is to say so.
    let mut app = common::app();
    app.on_key(Key::Ctrl('v'));
    let notice = app.notice.clone().expect("Ctrl-V said nothing at all");
    assert!(notice.contains("did not send a paste"), "{notice}");
    assert!(notice.contains("Shift-Insert"), "a refusal that names no fallback is an obstacle: {notice}");
}

#[test]
fn alt_v_carries_the_state_cycle_that_ctrl_v_used_to() {
    // The capability did not disappear with the chord. `b13_keyboard.rs` asserts the full
    // seven-state sweep from inside a text field; this is the narrower "it moved and still works".
    let mut app = common::app();
    let before = app.view().status.state;
    app.on_key(Key::Alt('v'));
    let asked: Vec<_> = app.drain_intents();
    assert!(
        asked.iter().any(|i| matches!(i, marlowe_view::Intent::ForceState(s) if *s != before)),
        "alt-v did not ask for a state change: {asked:?}"
    );
}

#[test]
fn the_footer_advertises_the_chord_that_actually_works() {
    // A footer is a claim about the keyboard. `^v` sat there for two milestones while Windows
    // Terminal ate the chord, and no test could see it because the dispatch tests never cross a
    // terminal.
    let footer: Vec<&str> = marlowe_surface::app::FOOTER_KEYS.iter().map(|(k, _)| *k).collect();
    assert!(footer.contains(&"alt-v"), "the footer does not advertise alt-v: {footer:?}");
    assert!(!footer.contains(&"^v"), "the footer still advertises ^v: {footer:?}");
}

// ─── the composer scrolls ─────────────────────────────────────────────────────────────────────

#[test]
fn up_walks_back_through_a_paste_too_long_to_show() {
    // **Growing a composer to a cap without giving it a scroll just moves the cliff.** The field
    // stops at INPUT_ROWS_MAX and pins to its last line, so before this there was no key that
    // reached line 1 of a long paste: the text was in the buffer and unreachable on screen.
    // **Typed, not pasted.** A paste this size becomes a chip, and a chip is one row that needs no
    // scrolling — the field overflows when somebody *writes* a long message, which is the case
    // this has to cover.
    let mut app = common::app();
    let wall: String = (0..200).map(|i| format!("line{i:03} ")).collect();
    type_str(&mut app, &wall);

    let at_end = message_rows(&app, 120, 30).join("\n");
    assert!(at_end.contains("line199"), "the field does not start pinned to the end:\n{at_end}");

    for _ in 0..20 {
        app.on_key(Key::Up);
    }
    let scrolled = message_rows(&app, 120, 30).join("\n");
    assert!(scrolled.contains("line000"), "Up never reached the start of the paste:\n{scrolled}");
    assert_ne!(at_end, scrolled, "Up changed nothing");
}

#[test]
fn typing_snaps_the_composer_back_to_what_is_being_typed() {
    // The control. A composer that stayed scrolled back while characters landed off-screen would
    // be worse than one that never moved at all.
    let mut app = common::app();
    let wall: String = (0..200).map(|i| format!("line{i:03} ")).collect();
    type_str(&mut app, &wall);
    for _ in 0..20 {
        app.on_key(Key::Up);
    }
    assert!(message_rows(&app, 120, 30).join("\n").contains("line000"));

    app.on_key(Key::Char('!'));
    let after = message_rows(&app, 120, 30).join("\n");
    assert!(after.contains('!'), "the typed character is not visible:\n{after}");
    assert!(!after.contains("line000"), "the field stayed scrolled back:\n{after}");
}

#[test]
fn scrolling_past_the_top_clamps_rather_than_rendering_out_of_range() {
    // The offset is a distance from the bottom and is deliberately NOT clamped where it is set —
    // `draw` clamps against the height it actually has, so this asserts the clamp is really there.
    let mut app = common::app();
    type_str(&mut app, &(0..200).map(|i| format!("line{i:03} ")).collect::<String>());
    for _ in 0..5_000 {
        app.on_key(Key::Up);
    }
    let rows = message_rows(&app, 120, 30);
    assert!(rows.join("\n").contains("line000"), "{rows:?}");
    assert_eq!(rows.len() as u16, render::INPUT_ROWS_MAX, "the field changed size: {rows:?}");
}
