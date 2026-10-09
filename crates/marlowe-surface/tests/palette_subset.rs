//! **The assertion that would have caught M3 F2's finding, and that no per-surface check can
//! make: the run window's palette is a subset of the conversation pane's.**
//!
//! # Why this file exists
//!
//! `window.rs` drew its resume line in `Tone::Green`. `render.rs` — the conversation pane — spends
//! green nowhere. §B13's colour budget was satisfied on **both** surfaces throughout, because it
//! counts colours *within* one surface: one accent, three state colours, three weights, and green
//! is a legal §B2 state colour. So the product read as two applications and every test was green.
//!
//! The human's words after using it: *"Too many colours. It is supposed to look like almost a
//! clone minus the irrelevant parts — same colour scheme, same feel, same everything, just
//! different sections. SAME STYLE."*
//!
//! # Where it asserts, and why not on the constants
//!
//! `chrome.rs`'s own unit tests check that [`chrome::RUN_WINDOW`] is a subset of
//! [`chrome::CONVERSATION`]. That is the value of a constant, and this project has logged what a
//! test on a declaration is worth: `web_is_inert_and_never_inlines` asserted
//! `inline_threshold_bytes == 0` on a build where nothing read the field (family #16).
//!
//! So every assertion here walks a **drawn `Buffer`** and maps each cell's foreground back through
//! [`chrome::Ink::of_color`]. What is being asserted is the fate of a pixel.

mod common;

use std::collections::BTreeSet;

use marlowe_surface::chrome::{self, Ink};
use marlowe_surface::window::WindowApp;
use marlowe_view::run::{CheckpointView, RunState};
use marlowe_view::{Entry, Speech, StatusState, Tab};
use ratatui::buffer::Buffer;

/// Every foreground role that reached a cell, optionally ignoring one region.
fn inks_outside(buf: &Buffer, skip: Option<ratatui::layout::Rect>) -> BTreeSet<Ink> {
    let theme = common::theme();
    let mut out = BTreeSet::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            if skip.is_some_and(|r| {
                x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
            }) {
                continue;
            }
            let Some(cell) = buf.cell((x, y)) else { continue };
            // A blank cell in the terminal's own colours is not a colour choice; counting it would
            // put `Body` in every set for free and make the subset weaker than it looks.
            if cell.symbol().trim().is_empty() {
                continue;
            }
            match Ink::of_color(&theme, cell.fg) {
                Some(ink) => {
                    out.insert(ink);
                }
                None => panic!(
                    "a cell at {x},{y} is painted {:?}, which is not in the palette at all — \
                     `chrome::Ink` is meant to be the only way to choose a colour, and something \
                     reached around it",
                    cell.fg
                ),
            }
        }
    }
    out
}

/// Every foreground role that actually reached a cell.
fn inks(buf: &Buffer) -> BTreeSet<Ink> {
    inks_outside(buf, None)
}

fn names(set: &BTreeSet<Ink>) -> String {
    set.iter().map(|i| i.name()).collect::<Vec<_>>().join(", ")
}

// ─── the window, exercised across everything it can be ────────────────────────────────────────

/// Every frame a run window can draw, so the observed set is the window's whole vocabulary rather
/// than one fixture's.
///
/// **A thin sweep here weakens every assertion below**, so this deliberately reaches the states
/// that are the *reason* the remaining state colours exist: a failure (red), a run needing the
/// user (amber), a refused steer (amber), and a run at its ceiling (red).
fn window_frames() -> Vec<Buffer> {
    let mut out = Vec::new();

    let states = [
        RunState::Queued,
        RunState::Running,
        RunState::WaitingApproval { decision: 7 },
        RunState::WaitingEvent,
        RunState::Paused { reason: "the token budget".into() },
        RunState::Completed,
        RunState::Failed { error: "the model endpoint refused".into() },
        RunState::Cancelled,
    ];

    for state in states {
        for checkpoint in [
            CheckpointView { last_completed: Some(41), resumable: true },
            CheckpointView { last_completed: None, resumable: true },
            CheckpointView { last_completed: Some(41), resumable: false },
        ] {
            let mut v = common::run_view();
            v.state = state.clone();
            v.checkpoint = checkpoint;
            // Marlowe's own prose, so `speech` is in the observed set — the window streams a run's
            // output through the same `entry_lines` the conversation pane uses.
            v.output = vec![
                Entry::User("resume from the last checkpoint".into()),
                Entry::Said(Speech::Model("Picking up at step 41.".into())),
            ];
            let mut app = WindowApp::new(v);
            for (w, h) in common::WINDOW_SIZES {
                out.push(common::window_frame(&app, w, h));
            }
            // Focus moves; §B2 signals it with three style swaps and no fill, so each position is
            // a different set of border colours.
            for _ in 0..5 {
                app.on_key(marlowe_surface::app::Key::Tab);
                out.push(common::window_frame(&app, 120, 30));
            }
            // A refusal displaces the steer draft, and it is the window's one amber that does not
            // come from a run's state.
            app.refused("a steer must not be empty");
            out.push(common::window_frame(&app, 120, 30));

            // **The cancel overlay, which is the window's third surface and was the one place a
            // hard-coded grey survived.** `overlay.rs` scrimmed every cell to `Color::DarkGray`
            // -- a colour picked outside the palette, invisible to any check that reads it. A
            // sweep that never raised a modal would not have found it.
            app.on_key(marlowe_surface::app::Key::Ctrl('x'));
            out.push(common::window_frame(&app, 120, 30));
            app.on_key(marlowe_surface::app::Key::Esc);
        }
    }

    // A run at its ceiling: the spend figure goes red without the state doing so.
    let mut v = common::run_view();
    v.spend_micros_usd = v.ceiling_micros_usd;
    out.push(common::window_frame(&WindowApp::new(v), 120, 30));

    out
}

/// The conversation surface, across the ordinary product path — the thing the window is supposed
/// to look like.
///
/// **Every §B5 state, voice included.** An earlier draft left `listening` and `speaking` out to
/// keep green from entering the observed set, which would have been choosing the fixture to make
/// the answer come out right. The exclusion that IS made is a *region*, not a state, and it is
/// made below with its own control.
fn conversation_frames() -> Vec<Buffer> {
    let mut rig = common::rig();
    let mut out = Vec::new();

    for state in [
        StatusState::Listening,
        StatusState::Thinking,
        StatusState::Speaking,
        StatusState::Writing,
        StatusState::Running,
        StatusState::Waiting,
        StatusState::Idle,
    ] {
        rig.force_state(state, 1_000);
        for (w, h) in common::SIZES {
            out.push(common::frame(&rig.app, w, h));
        }
        for tab in [Tab::Runs, Tab::Schedule, Tab::Sessions, Tab::Skills, Tab::Trust] {
            rig.tab(tab);
            out.push(common::frame(&rig.app, 140, 40));
        }
        rig.tab(Tab::Runs);
    }

    // The scripted session, played far enough to hold speech, tool lines and a reasoning block.
    let mut rig = common::rig();
    for t in (0..24_000).step_by(500) {
        rig.tick(t);
        out.push(common::frame(&rig.app, 140, 40));
    }

    out
}

fn observed(frames: &[Buffer]) -> BTreeSet<Ink> {
    frames.iter().flat_map(|b| inks(b)).collect()
}

/// The conversation surface **minus §B5's status band** — which is the comparison the window is
/// actually subject to, and the reason is structural rather than convenient.
///
/// A run window has no voice. §B5's band is the one region that reports whether Marlowe is
/// listening or speaking, and green is the colour of exactly those two states (§B2: *green —
/// healthy, live, running normally*; §B5's colour column). The band is a **region the window does
/// not have**, so comparing against a set that includes its colours would let the window paint
/// green for free — the subset would hold and would say nothing.
///
/// `the_status_band_is_the_only_place_the_conversation_spends_green` is the control that keeps
/// this honest: it measures that green really is confined to the band rather than assuming it. If
/// green ever appears outside it, this exclusion has stopped being a fact about regions and must
/// be re-derived before anything below is believed again.
fn conversation_inks_outside_the_band() -> BTreeSet<Ink> {
    conversation_frames()
        .iter()
        .flat_map(|b| {
            let band = marlowe_surface::render::layout(b.area).status;
            inks_outside(b, Some(band))
        })
        .collect()
}

// ─── the assertions ───────────────────────────────────────────────────────────────────────────

/// **The cross-surface property.** No check inside either surface can make it.
#[test]
fn the_run_window_paints_no_colour_the_conversation_pane_does_not() {
    let window = observed(&window_frames());
    let conversation = conversation_inks_outside_the_band();

    let extra: Vec<_> = window.difference(&conversation).copied().collect();
    assert!(
        extra.is_empty(),
        "the run window paints {} — which the conversation pane never does. A window is the same \
         product with different sections, so a hue that exists nowhere near it reads as a second \
         application.\n  window:       {}\n  conversation: {}",
        extra.iter().map(|i| i.name()).collect::<Vec<_>>().join(", "),
        names(&window),
        names(&conversation),
    );
}

/// **The control for the assertion above**, and it is the whole reason that assertion bites.
///
/// The subset test compares against the conversation surface with §B5's band cut out. That cut is
/// only legitimate if green really does live *only* in the band — otherwise it is a hole punched
/// in the comparison at exactly the place the answer was going to come from. So this measures it:
/// green must appear in a whole conversation frame (or the fixture never reaches a voice state and
/// the cut is doing nothing at all) **and** must be absent once the band is removed.
///
/// This project's standing question, applied here: *what would this read if the thing I care about
/// were broken?* A conversation set that carried green would make
/// `the_run_window_paints_no_colour_the_conversation_pane_does_not` pass with a green window.
#[test]
fn the_status_band_is_the_only_place_the_conversation_spends_green() {
    let whole = observed(&conversation_frames());
    assert!(
        whole.contains(&Ink::Green),
        "no conversation frame reached a green cell at all, so removing the band proves nothing \
         and the fixture is not exercising §B5's voice states: {}",
        names(&whole),
    );

    let outside = conversation_inks_outside_the_band();
    assert!(
        !outside.contains(&Ink::Green),
        "green reached a cell outside §B5's band. The subset test excludes the band because a \
         window has no voice; if green is no longer confined there, that exclusion is not a fact \
         about regions any more and the subset test has stopped being evidence: {}",
        names(&outside),
    );

    // ...and the remaining fixture is not simply empty. A sweep that drew nothing would satisfy
    // the line above for the wrong reason.
    assert!(
        outside.len() >= 6,
        "only {} roles reached a cell outside the band; the fixture is too thin to be evidence \
         about anything: {}",
        outside.len(),
        names(&outside),
    );
}

/// **The declaration has a reader, and the reader is a drawn buffer.**
///
/// Both directions, because each catches a different mistake:
///
/// * a colour drawn but not declared — the window reached around the vocabulary;
/// * a colour declared but never drawn — the vocabulary is wider than the window, which quietly
///   weakens every subset argument made from it. That is family #16 with the sign flipped, and it
///   is the door somebody walks through when they "fix" the subset test by widening the constant.
#[test]
fn the_windows_declared_palette_is_exactly_what_it_draws() {
    let drawn = observed(&window_frames());
    let declared: BTreeSet<Ink> = chrome::RUN_WINDOW.iter().copied().collect();

    let undeclared: Vec<_> = drawn.difference(&declared).copied().collect();
    assert!(
        undeclared.is_empty(),
        "the window drew {} without declaring it in `chrome::RUN_WINDOW`",
        undeclared.iter().map(|i| i.name()).collect::<Vec<_>>().join(", "),
    );

    let unused: Vec<_> = declared.difference(&drawn).copied().collect();
    assert!(
        unused.is_empty(),
        "`chrome::RUN_WINDOW` declares {} and no window frame draws it. A vocabulary wider than \
         the surface makes the subset property look stronger than it is",
        unused.iter().map(|i| i.name()).collect::<Vec<_>>().join(", "),
    );
}

/// The window's set is **strictly** smaller. §6's window is the conversation pane with different
/// sections; equality would mean it had grown a dashboard's worth of colour.
#[test]
fn the_window_spends_strictly_less_colour_than_the_conversation() {
    let window = observed(&window_frames());
    let conversation = observed(&conversation_frames());
    assert!(
        window.len() < conversation.len(),
        "window {} ({}) is not smaller than conversation {} ({})",
        window.len(),
        names(&window),
        conversation.len(),
        names(&conversation),
    );
}
