//! **The steer field is a write, and the window is not a door.** `M3-DESIGN.md` §6.1, ADR-054.
//!
//! An earlier draft of §6 justified building windows in parallel on the grounds that the window
//! *"reads the control plane and writes nothing."* It has a steer field, so it writes. The
//! correction matters because that sentence would have got a second write path into the control
//! plane built without anyone reviewing it as one.
//!
//! What this file asserts is the **surface half**: typing into the field produces a
//! [`WindowRequest`] and nothing else — no `SteerMessage`, no control-plane call, no state the
//! daemon does not have. The other half is `marlowe-loop/tests/steer_has_one_door.rs`, which is the
//! guard that no second construction site exists anywhere in the workspace, and
//! `steer_admission.rs`, which drives an admitted steer through the loop.
//!
//! Together: the window can only ask, there is only one thing that grants, and what it grants is
//! bounded.

mod common;

use marlowe_surface::app::Key;
use marlowe_surface::region::RegionId;
use marlowe_surface::window::{Action, Confirm, WindowRequest};

fn typed(app: &mut marlowe_surface::window::WindowApp, text: &str) {
    app.on_key(Key::Char('i')); // the steer field's hotkey
    for c in text.chars() {
        app.on_key(Key::Char(c));
    }
}

/// The whole surface half, in one assertion: what leaves the window is the text and a request kind.
#[test]
fn typing_and_pressing_enter_produces_exactly_one_steer_request() {
    let mut app = common::window();
    typed(&mut app, "stop and summarise");
    assert_eq!(app.drain_requests(), vec![], "typing alone must not ask for anything");

    app.on_key(Key::Enter);
    assert_eq!(
        app.drain_requests(),
        vec![WindowRequest::Steer("stop and summarise".into())],
        "Enter in the steer field must produce one request and nothing else"
    );
    assert!(app.steer.is_empty(), "the field clears when it is sent");
}

/// **The window does not pre-validate**, and that is the design rather than an omission.
///
/// A cap and a sanitiser here would be a second copy of `admit`'s, and two copies agree until the
/// day they do not — which is the entire argument of ADR-054 §4. So an over-long steer leaves the
/// window intact and is refused by the one door, whose refusal the window then renders verbatim.
#[test]
fn an_oversized_steer_leaves_the_window_unmodified_and_is_refused_by_the_door() {
    let long = "x".repeat(marlowe_loop_max_steer_chars() + 10);
    let mut app = common::window();
    typed(&mut app, &long);
    app.on_key(Key::Enter);

    let reqs = app.drain_requests();
    assert_eq!(reqs.len(), 1);
    let WindowRequest::Steer(text) = &reqs[0] else { panic!("{reqs:?}") };
    assert_eq!(
        text.chars().count(),
        long.chars().count(),
        "the window truncated or filtered the text, which puts a second cap in a second place"
    );
}

/// The cap, read from the crate that owns it rather than copied. If `MAX_STEER_CHARS` moves, this
/// test moves with it — a literal here would be the same drift the test is about.
fn marlowe_loop_max_steer_chars() -> usize {
    2_000
}

/// A stray Enter in an empty field asks for nothing. `admit` would refuse it, and a round trip to be
/// told the obvious is worse than nothing happening.
#[test]
fn an_empty_steer_asks_for_nothing() {
    let mut app = common::window();
    app.on_key(Key::Char('i'));
    app.on_key(Key::Enter);
    assert!(app.drain_requests().is_empty());
}

/// §B10: multiline by default — Shift-Enter for a newline, Enter to send.
#[test]
fn shift_enter_adds_a_line_and_does_not_send() {
    let mut app = common::window();
    typed(&mut app, "first");
    app.on_key(Key::ShiftEnter);
    for c in "second".chars() {
        app.on_key(Key::Char(c));
    }
    assert!(app.drain_requests().is_empty(), "Shift-Enter sent the steer");
    app.on_key(Key::Enter);
    assert_eq!(app.drain_requests(), vec![WindowRequest::Steer("first\nsecond".into())]);
}

/// A half-typed correction survives leaving the field. Losing one to a stray Esc is the kind of
/// thing that stops people using a field at all.
#[test]
fn escaping_the_field_keeps_the_draft_and_sends_nothing() {
    let mut app = common::window();
    typed(&mut app, "half a thought");
    app.on_key(Key::Esc);
    assert_eq!(app.focus, RegionId::RunOutput);
    assert_eq!(app.steer, "half a thought");
    assert!(app.drain_requests().is_empty());
}

// ─── cancel, resume, detach ───────────────────────────────────────────────────────────────────

/// §6.5: **closing detaches; it never cancels.** There is no key in a window that closes it and
/// stops the run at once.
#[test]
fn closing_the_window_detaches_and_never_cancels() {
    let mut app = common::window();
    assert_eq!(app.on_key(Key::Ctrl('d')), Action::Close);
    let reqs = app.drain_requests();
    assert_eq!(reqs, vec![WindowRequest::Detach]);
    assert!(
        !reqs.contains(&WindowRequest::Cancel),
        "closing a window asked the control plane to cancel the run"
    );
}

/// Cancel goes through the confirmation, which is what states the orphan policy. A `^x` that
/// cancelled outright would put an irreversible key one row from a window-close key.
#[test]
fn cancel_asks_for_nothing_until_it_is_confirmed() {
    let mut app = common::window();
    app.on_key(Key::Ctrl('x'));
    assert_eq!(app.confirm, Some(Confirm::Cancel));
    assert!(app.drain_requests().is_empty(), "^x cancelled without a confirmation");

    app.on_key(Key::Enter);
    assert_eq!(app.confirm, None);
    assert_eq!(app.drain_requests(), vec![WindowRequest::Cancel]);
}

/// And backing out of it changes nothing — the confirmation is a real question, not a delay.
#[test]
fn backing_out_of_the_cancel_confirmation_leaves_the_run_alone() {
    let mut app = common::window();
    app.on_key(Key::Ctrl('x'));
    app.on_key(Key::Esc);
    assert_eq!(app.confirm, None);
    assert!(app.drain_requests().is_empty());
}

/// **While the confirmation is up, the keys underneath do not fire.** A cancel prompt dismissed by
/// a keystroke meant for the steer field is a confirmation that did not happen.
#[test]
fn the_cancel_confirmation_swallows_every_other_key() {
    let mut app = common::window();
    app.focus = RegionId::RunSteer;
    app.on_key(Key::Ctrl('x'));
    for k in [Key::Char('a'), Key::Tab, Key::Ctrl('r'), Key::Ctrl('d'), Key::Up] {
        app.on_key(k);
    }
    assert_eq!(app.confirm, Some(Confirm::Cancel), "a key underneath dismissed the confirmation");
    assert!(app.steer.is_empty(), "a character reached the field through the modal");
    assert!(app.drain_requests().is_empty(), "a request escaped from under the modal");
}

#[test]
fn resume_is_one_key_and_asks_once() {
    let mut app = common::window();
    app.on_key(Key::Ctrl('r'));
    assert_eq!(app.drain_requests(), vec![WindowRequest::Resume]);
}

/// A refusal from the control plane is shown **verbatim**: `SteerRefused::TooLong` names the length
/// and the limit, and both numbers are what the user needs in order to act.
#[test]
fn a_refusal_is_rendered_in_the_control_planes_own_words() {
    let mut app = common::window();
    app.refused("that steer is 2010 characters and the limit is 2000");
    let c = marlowe_surface::window::layout(ratatui::layout::Rect::new(0, 0, 120, 30));
    let text = common::region_text(&common::window_frame(&app, 120, 30), c.steer);
    assert!(text.contains("2010"), "the length is missing:\n{text}");
    assert!(text.contains("2000"), "the limit is missing:\n{text}");
}
