//! **`a_second_render_of_the_same_state_changes_not_one_cell`, for a run window.**
//!
//! `M3-DESIGN.md` §6.4 borrows §B13's flicker rows wholesale: *"Anything that moves is a pure
//! function of `(state, now_ms)` — there is no `App::tick`, so pacing state belongs in `App` as a
//! property of looking at the run."*
//!
//! # The instrument, and why the negative control is half the file
//!
//! A cell-by-cell diff of two frames is a strong assertion and a **trivially satisfiable** one: a
//! window that drew nothing at all would pass every test below. So every purity assertion here is
//! paired with a control that makes the same diff come back non-empty — a moved clock, a new line
//! of output, a focus change. Without those, this file would be measuring an empty buffer and
//! reporting zero flicker.

mod common;

use marlowe_surface::window::{self, WindowApp};
use marlowe_view::notice::Speech;
use marlowe_view::Entry;

#[test]
fn a_second_render_of_the_same_state_changes_not_one_cell() {
    for (w, h) in common::WINDOW_SIZES {
        let mut app = common::window();
        // Real content, not an empty run: a blank window is the case that passes for free.
        app.update(with_output(vec![
            Entry::Said(Speech::Model("## Findings\n\nThree of the four sources agree.".into())),
            Entry::Reasoning { text: "weighing the fourth".into(), tokens: 4, done: true },
        ]));

        let mut term = common::terminal(w, h);
        let first = common::draw_window_into(&mut term, &app);
        let second = common::draw_window_into(&mut term, &app);
        let moved = common::diff_cells(&first, &second);
        assert!(
            moved.is_empty(),
            "at {w}x{h}, {} cells changed between two renders of one state: {:?}",
            moved.len(),
            &moved[..moved.len().min(8)]
        );
    }
}

/// **The control.** If the diff instrument could not see a change, the test above would be
/// measuring nothing — and this is the shape that has gone wrong in this project more than once.
///
/// It used to move the clock. **That control is gone because the thing it moved is gone**: elapsed
/// is resolved on the daemon now, so no cell in a window is a function of time and a window holds
/// no `now_ms` to advance. What still changes a frame is state, which is what this moves instead.
#[test]
fn the_diff_does_see_a_change_when_the_run_advances() {
    let mut app = common::window();
    let mut term = common::terminal(120, 30);
    let a = common::draw_window_into(&mut term, &app);

    let mut v = common::run_view();
    v.elapsed_ms = 121_000;
    v.spend_micros_usd = 900_000;
    app.update(v);
    let b = common::draw_window_into(&mut term, &app);
    assert!(
        !common::diff_cells(&a, &b).is_empty(),
        "the identity panel did not repaint when elapsed and spend changed, so the purity check \
         above is asserting nothing"
    );
}

/// The second control: new output moves cells. Together with the one above, the purity assertion is
/// bracketed on both of the things that actually change in a live window.
#[test]
fn the_diff_does_see_a_change_when_output_arrives() {
    let mut app = common::window();
    let mut term = common::terminal(120, 30);
    let a = common::draw_window_into(&mut term, &app);
    app.update(with_output(vec![Entry::Said(Speech::Model("a new line arrived".into()))]));
    let b = common::draw_window_into(&mut term, &app);
    assert!(!common::diff_cells(&a, &b).is_empty(), "output arrived and nothing repainted");
}

/// A focus change repaints **the perimeter, not the region**. §B2's whole colour scheme rests on
/// focus being carried by border and title styles rather than by a fill, and §B12's flicker target
/// rests on that costing a border's worth of cells.
#[test]
fn moving_focus_repaints_borders_and_not_the_inside_of_a_region() {
    let mut app = common::window();
    app.update(with_output(vec![Entry::Said(Speech::Model(
        "a paragraph of ordinary output that fills several cells inside the region".into(),
    ))]));

    let mut term = common::terminal(120, 30);
    let a = common::draw_window_into(&mut term, &app);
    app.on_key(marlowe_surface::app::Key::Char('i')); // jump to the steer field
    let b = common::draw_window_into(&mut term, &app);

    let c = window::layout(ratatui::layout::Rect::new(0, 0, 120, 30));
    let changed = common::diff_cells(&a, &b);
    assert!(!changed.is_empty(), "focus moved and nothing repainted at all");

    // Not one cell strictly *inside* the output region changed: the prose is untouched.
    let scroll = c.output_scroll;
    for (x, y) in &changed {
        let inside = *x >= scroll.x
            && *x < scroll.right()
            && *y >= scroll.y
            && *y < scroll.bottom();
        assert!(
            !inside,
            "focus repainted ({x},{y}), which is inside the output region — focus must be a \
             border-and-title style swap, not a repaint"
        );
    }
}

/// **Scrolling moves the scroll area and nothing else.** §B6's *"zero chrome inside a scroll area"*
/// has a mirror obligation: zero scroll outside it.
#[test]
fn scrolling_moves_only_the_scroll_area() {
    let mut app = common::window();
    let many: Vec<Entry> = (0..80)
        .map(|i| Entry::Said(Speech::Model(format!("line {i}"))))
        .collect();
    app.update(with_output(many));

    // Exactly what the driver does: it holds the terminal size, so it is the thing that can say
    // how far the window may scroll. See `WindowApp::scroll_max_hint`.
    let area = ratatui::layout::Rect::new(0, 0, 120, 30);
    app.set_scroll_max(window::scroll_max(&app, &common::theme(), area));

    let mut term = common::terminal(120, 30);
    let a = common::draw_window_into(&mut term, &app);
    app.on_key(marlowe_surface::app::Key::Up);
    app.on_key(marlowe_surface::app::Key::Up);
    let b = common::draw_window_into(&mut term, &app);

    let c = window::layout(ratatui::layout::Rect::new(0, 0, 120, 30));
    let changed = common::diff_cells(&a, &b);
    assert!(!changed.is_empty(), "premise: there was enough output to scroll");
    for (x, y) in &changed {
        let inside = *x >= c.output_scroll.x
            && *x < c.output_scroll.right()
            && *y >= c.output_scroll.y
            && *y < c.output_scroll.bottom();
        assert!(inside, "scrolling repainted ({x},{y}), which is outside the scroll area");
    }
}

fn with_output(output: Vec<Entry>) -> marlowe_view::RunView {
    let mut v = common::run_view();
    v.output = output;
    v
}

/// The one thing about a window that is allowed to be a function of time is a function of the time
/// it is **given**, not of the time it reads. There is no clock in this crate and this asserts it
/// where it would show: two windows at the same `now_ms` are identical frames.
#[test]
fn two_windows_at_the_same_instant_are_the_same_frame() {
    let mut a = WindowApp::new(common::run_view());
    let mut b = WindowApp::new(common::run_view());
    assert_eq!(
        common::buffer_text(&common::window_frame(&a, 120, 30)),
        common::buffer_text(&common::window_frame(&b, 120, 30)),
    );
}
