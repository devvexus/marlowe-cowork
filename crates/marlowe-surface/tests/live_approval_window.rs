//! **The live approval window: yes, no, and no-with-a-reason.**
//!
//! The daemon blocks on this answer, which makes the window the one piece of UI with a process
//! waiting on the other end of it. Two properties follow and both are asserted here:
//!
//! 1. every key that leaves the window **sends an answer**, and
//! 2. there is **no key that leaves it without one** — a dismissed question is a hung turn.

use marlowe_surface::app::{App, Key};
use marlowe_view::approval::PendingApproval;
use marlowe_view::view::Intent;

fn app_awaiting_approval() -> App {
    let producer = marlowe_stub::Session::new();
    let mut view = producer.view().clone();
    view.pending_approval = Some(PendingApproval {
        decision: 3,
        verb: "web".into(),
        scope: "https://example.com/".into(),
        reversible: true,
        novelty: None,
    });
    App::new(view).unwrap()
}

fn answer(key: Key) -> Option<Intent> {
    let mut app = app_awaiting_approval();
    app.on_key(key);
    app.drain_intents().into_iter().next()
}

#[test]
fn y_approves_and_n_declines() {
    assert_eq!(answer(Key::Char('y')), Some(Intent::Approve { granted: true, reason: None }));
    assert_eq!(answer(Key::Char('n')), Some(Intent::Approve { granted: false, reason: None }));
    // Esc is a decline rather than a dismissal. It is the key a user reaches for to make a modal
    // go away, and the only safe meaning for it here is "no".
    assert_eq!(answer(Key::Esc), Some(Intent::Approve { granted: false, reason: None }));
}

#[test]
fn o_collects_a_reason_and_sends_it_with_the_decline() {
    let mut app = app_awaiting_approval();
    app.on_key(Key::Char('o'));
    assert!(app.drain_intents().is_empty(), "opening the editor must not answer yet");

    for c in "wrong host".chars() {
        app.on_key(Key::Char(c));
    }
    app.on_key(Key::Backspace);
    app.on_key(Key::Enter);

    assert_eq!(
        app.drain_intents(),
        vec![Intent::Approve {
            granted: false,
            reason: Some(marlowe_view::notice::Echo("wrong hos".into())),
        }],
        "other is a decline that carries the user's words, quoted and not reworded"
    );
}

/// **Esc inside the editor goes back to the question, not out of it.**
///
/// The tempting behaviour is to treat it as a cancel that closes the window. That would leave the
/// daemon blocked on a read with nothing on screen, which is the worst outcome available here.
#[test]
fn escaping_the_reason_editor_returns_to_the_question_without_answering() {
    let mut app = app_awaiting_approval();
    app.on_key(Key::Char('o'));
    app.on_key(Key::Char('x'));
    app.on_key(Key::Esc);
    assert!(app.drain_intents().is_empty(), "esc in the editor must not answer");

    // ...and the question is still live, so the keys still work.
    app.on_key(Key::Char('y'));
    assert_eq!(
        app.drain_intents(),
        vec![Intent::Approve { granted: true, reason: None }],
        "the window is still up and still answerable"
    );
}

/// **The negative control.** A key that is not an answer must not close the window.
#[test]
fn an_unrelated_key_neither_answers_nor_dismisses() {
    let mut app = app_awaiting_approval();
    for key in [Key::Tab, Key::Up, Key::Char('q'), Key::Char('1'), Key::Enter] {
        app.on_key(key);
        assert!(
            app.drain_intents().is_empty(),
            "{key:?} produced an answer, and it is not one of the three"
        );
    }
    assert!(
        app.view().pending_approval.is_some(),
        "the window must still be up: nothing has answered the daemon"
    );
}

// ── the window must show the whole target ────────────────────────────────────────────────
//
// The scope line is the thing the user is deciding about. Truncating it silently is the same
// defect as the blast radius dropping a target it could not stringify, one layer up.

use marlowe_surface::theme::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

fn rendered_window(scope: &str, reversible: bool) -> String {
    let producer = marlowe_stub::Session::new();
    let mut view = producer.view().clone();
    view.pending_approval = Some(PendingApproval {
        decision: 1,
        verb: "web".into(),
        scope: scope.into(),
        reversible,
        novelty: None,
    });
    let app = App::new(view).unwrap();
    let area = Rect { x: 0, y: 0, width: 100, height: 30 };
    let mut buf = Buffer::empty(area);
    marlowe_surface::overlay::draw_pending_approval(&app, &Theme::default_truecolor(), area, &mut buf);
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_string()))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// **A long URL must appear in full.** It wraps; it is never cut off.
#[test]
fn the_whole_target_is_on_screen_however_long_it_is() {
    let long = "https://a-really-quite-long-subdomain.example.com/some/deep/path?with=query&more=1";
    let screen = rendered_window(long, true);
    // Whitespace AND the box borders come out: a wrapped URL has a `║` and a newline in the
    // middle of it, which is the frame rather than the content.
    let strip = |t: &str| -> String {
        t.chars()
            .filter(|c| !c.is_whitespace() && !"║╔╗╚╝═".contains(*c))
            .collect()
    };
    let flat = strip(&screen);
    let want = strip(long);
    assert!(
        flat.contains(&want),
        "the target was truncated, so the user would approve a host they could not fully \
         read.\n{screen}"
    );
}

/// **The constants are gone.** `novelty not assessed` and `ceiling unknown` never varied — they
/// announced that two subsystems do not exist yet, on every prompt, forever. §B1 puts that under
/// `--dev`, not in the interface.
#[test]
fn the_window_does_not_report_which_subsystems_are_missing() {
    let screen = rendered_window("https://example.com/", true);
    assert!(screen.contains("reversible"), "what IS known still shows:\n{screen}");
    for noise in ["not assessed", "ceiling", "M6", "trust ledger"] {
        assert!(
            !screen.contains(noise),
            "{noise:?} is a constant on every prompt and belongs under --dev:\n{screen}"
        );
    }
}

/// Irreversibility is the half that changes a decision, so it stays and it is loud.
#[test]
fn an_irreversible_call_says_so() {
    assert!(rendered_window("rm -rf .", false).contains("NOT reversible"));
}
