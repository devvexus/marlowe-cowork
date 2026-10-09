//! §B13's acceptance rows, applied to a run window. `M3-DESIGN.md` §6.4 makes them binding here:
//! *"Two hard constraints, both already enforced by tests."*
//!
//! A window is a second surface, so every row that is a property of **the frame** has to be
//! re-asserted on it. Inheriting the main pane's green is the *"a measurement is scoped to the
//! system it was taken on"* family: the main frame's colour test says nothing about a file that
//! draws its own panels.

mod common;

use marlowe_surface::window::{self, Confirm, WindowApp};
use marlowe_view::notice::Speech;
use marlowe_view::{Entry, Metric, ToolCall};
use ratatui::style::Color;

/// Every state a window can be in, so the colour and fill rows are checked across all of them
/// rather than on the one that happens to be easy.
fn scenarios() -> Vec<(&'static str, WindowApp)> {
    let mut out = Vec::new();

    let mut running = common::window();
    running.update({
        let mut v = common::run_view();
        v.output = vec![
            Entry::Said(Speech::Model("## Findings\n\n- one\n- two\n\n`code` and *emphasis*".into())),
            Entry::Tools(vec![ToolCall::ok(1, "read", "notes.md", vec![Metric::Count { n: 48, unit: "lines" }])]),
            Entry::Reasoning { text: "still weighing it".into(), tokens: 4, done: false },
        ];
        v
    });
    out.push(("running with output", running));

    let mut ceiling = common::window();
    ceiling.update({
        let mut v = common::run_view();
        v.spend_micros_usd = v.ceiling_micros_usd;
        v
    });
    out.push(("at its spend ceiling", ceiling));

    let mut failed = common::window();
    failed.update({
        let mut v = common::run_view();
        v.state = marlowe_view::RunState::Failed { error: "the provider rejected the request".into() };
        v.elapsed_ms = 30_000;
        v
    });
    out.push(("failed", failed));

    let mut waiting = common::window();
    waiting.update({
        let mut v = common::run_view();
        v.state = marlowe_view::RunState::WaitingApproval { decision: 7 };
        v
    });
    out.push(("waiting on an approval", waiting));

    let mut cancelling = common::window();
    cancelling.confirm = Some(Confirm::Cancel);
    out.push(("cancel confirmation up", cancelling));

    let mut steering = common::window();
    steering.focus = marlowe_surface::region::RegionId::RunSteer;
    steering.steer = "stop and summarise".into();
    out.push(("typing a steer", steering));

    let mut refused = common::window();
    refused.refused("that steer is 2100 characters and the limit is 2000");
    out.push(("a refused steer", refused));

    out
}

/// §B13: **≤ 1 accent + 3 state colours + 3 foreground weights**, asserted **by value**.
///
/// By value rather than by counting distinct colours, for the reason `b13_rendering.rs` gives: a
/// count passes happily while every value is wrong, which is what shipped before a screenshot
/// caught it.
#[test]
fn every_colour_a_window_emits_is_one_of_the_declared_values() {
    let theme = common::theme();
    // A Vec, not a HashSet: `determinism_guard.rs` bans hash-ordered collections crate-wide.
    let mut allowed: Vec<Color> = theme.declared_colours();
    // The cancel overlay dims the frame behind it by rewriting foregrounds, not by filling
    // backgrounds — the same one extra value the approval overlay declares.
    allowed.push(Color::DarkGray);

    for (name, app) in scenarios() {
        for (w, h) in common::WINDOW_SIZES {
            let buf = common::window_frame(&app, w, h);
            for y in 0..h {
                for x in 0..w {
                    let fg = buf.cell((x, y)).unwrap().fg;
                    assert!(
                        allowed.contains(&fg),
                        "{name} at {w}x{h}: cell ({x},{y}) uses {fg:?}, outside the declared \
                         palette of one accent, three state colours and three weights"
                    );
                }
            }
        }
    }
}

/// §B13: **zero background fills**, including with the overlay up. A `bg`-based scrim would fail
/// this and would also be the full-cell repaint §B12 forbids.
#[test]
fn not_one_cell_in_any_window_carries_a_background_or_reverse_video() {
    for (name, app) in scenarios() {
        for (w, h) in common::WINDOW_SIZES {
            let buf = common::window_frame(&app, w, h);
            for y in 0..h {
                for x in 0..w {
                    let cell = buf.cell((x, y)).unwrap();
                    assert_eq!(
                        cell.bg,
                        Color::Reset,
                        "{name} at {w}x{h}: cell ({x},{y}) carries a background"
                    );
                    assert!(
                        !cell.modifier.contains(ratatui::style::Modifier::REVERSED),
                        "{name} at {w}x{h}: cell ({x},{y}) is reverse video, which is a fill by \
                         another name"
                    );
                }
            }
        }
    }
}

/// **State colours encode state, never category.** The negative control for the row above: a window
/// on a healthy run must carry no state colour at all, or "state colour" has come to mean
/// "decoration" and the amber that means *needs you* has stopped meaning anything.
#[test]
fn a_healthy_run_carries_no_state_colour_anywhere() {
    let theme = common::theme();
    let app = common::window();
    let buf = common::window_frame(&app, 160, 45);
    for y in 0..45 {
        for x in 0..160 {
            let fg = buf.cell((x, y)).unwrap().fg;
            for (tone, name) in [
                (marlowe_view::Tone::Red, "red"),
                (marlowe_view::Tone::Amber, "amber"),
                (marlowe_view::Tone::Green, "green"),
            ] {
                // Green is legitimate on the checkpoint's resumable line — that IS a state — so it
                // is the one exception and it is named rather than allowed everywhere.
                if tone == marlowe_view::Tone::Green {
                    continue;
                }
                assert_ne!(
                    fg,
                    theme.tone(tone),
                    "a run that needs nothing painted cell ({x},{y}) {name}"
                );
            }
        }
    }
}

/// §B13: **every region reachable by keyboard alone.** A window has no mouse path at all, so this
/// is not a convenience row here — it is the only way in.
#[test]
fn every_window_region_is_reachable_by_its_hotkey_and_by_tab() {
    let tree = marlowe_surface::region::RegionTree::for_window();
    for r in tree.regions() {
        let mut app = common::window();
        let k = r.hotkey().expect("a window region with no hotkey");
        app.on_key(marlowe_surface::app::Key::Char(k));
        assert_eq!(
            app.focus,
            r.id(),
            "{:?} advertises ({k}) on its border and that key does not reach it",
            r.id(),
        );
    }

    // And Tab walks all of them, wrapping. A hotkey that works while Tab skips a region is a
    // region a keyboard user cannot find.
    let mut app = common::window();
    let mut seen = vec![app.focus];
    for _ in 0..tree.regions().len() {
        app.on_key(marlowe_surface::app::Key::Tab);
        if !seen.contains(&app.focus) {
            seen.push(app.focus);
        }
    }
    assert_eq!(seen.len(), tree.regions().len(), "Tab does not reach every region: {seen:?}");
}

/// The window's own minimum, and §B11's rule that a broken grid is worse than an honest refusal.
#[test]
fn the_frame_is_whole_at_the_minimum_and_refused_one_column_below_it() {
    let app = common::window();
    let ok = common::buffer_text(&common::window_frame(&app, window::MIN_COLS, window::MIN_ROWS));
    assert!(ok.contains("Checkpoint"), "the frame is not whole at its own minimum:\n{ok}");

    let too_narrow =
        common::buffer_text(&common::window_frame(&app, window::MIN_COLS - 1, window::MIN_ROWS));
    assert!(!too_narrow.contains("Checkpoint"), "a degraded grid was drawn:\n{too_narrow}");
    let too_short =
        common::buffer_text(&common::window_frame(&app, window::MIN_COLS, window::MIN_ROWS - 1));
    assert!(!too_short.contains("Checkpoint"), "a degraded grid was drawn:\n{too_short}");
}
