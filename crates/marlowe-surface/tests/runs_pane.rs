//! **`/runs` asks the daemon, and says how many there are only once it has answered.**
//!
//! The Runs tab had never shown a live run to anyone. `/runs` was an `Outcome::Tab`: it switched
//! tab and re-summarised the client's cached view, which was last filled from `Event::Run` at
//! daemon boot. The daemon side was correct throughout — `turn()` registers every run — and the
//! surface simply never asked.
//!
//! The second half is why `Outcome::TabLive` carries no `Notice`. A summary is a **count of what
//! the pane holds**, and one composed at dispatch time counts the stale view: the pane would fill
//! correctly and the sentence beside it would describe the moment before.

mod common;

use marlowe_surface::commands::{self, Outcome};
use marlowe_surface::region::RegionId;
use marlowe_view::{Intent, Tab};

#[test]
fn slash_runs_asks_the_daemon_rather_than_re_reading_the_cache() {
    let app = common::app();
    match commands::dispatch(app.view(), "runs", &[]) {
        Outcome::TabLive(Tab::Runs, Intent::Runs) => {}
        other => panic!("/runs did not ask for the runs: {other:?}"),
    }
}

/// The control, and the reason the variant exists. If `/runs` still carried a `Notice`, that
/// notice would have been composed from the view as it stood *before* the answer.
#[test]
fn the_request_carries_no_summary_because_there_is_nothing_to_summarise_yet() {
    let app = common::app();
    let outcome = commands::dispatch(app.view(), "runs", &[]);
    assert!(
        !matches!(outcome, Outcome::Tab(..)),
        "/runs is an Outcome::Tab again, which is the stale-summary shape it was moved out of"
    );
}

#[test]
fn pressing_slash_runs_switches_the_tab_at_once_and_queues_the_request() {
    // The tab is the user's keystroke and is instant; the count waits for the daemon.
    let mut app = common::app();
    app.tab = Tab::Schedule;
    app.run_command("runs", &[]);
    assert_eq!(app.tab, Tab::Runs, "the tab did not switch");
    assert!(
        app.drain_intents().contains(&Intent::Runs),
        "nothing was asked of the producer"
    );
}

/// **The summary lands after the answer, not with the request.**
#[test]
fn the_pane_summary_is_said_once_the_producer_has_answered() {
    let mut app = common::app();
    let before = app.view().transcript.len() + app.client_lines.len();
    app.run_command("runs", &[]);
    assert_eq!(
        app.client_lines.len() + app.view().transcript.len(),
        before,
        "a summary was said before the daemon answered — that is the stale count"
    );

    // `advance` applies the intent and republishes in one crank; `update` is that republish.
    let view = app.view().clone();
    app.update(view);
    assert!(
        app.client_lines.len() + app.view().transcript.len() > before,
        "no summary was ever said, so /runs is silent about what it found"
    );
}

// ─── the pane's own keyboard ───────────────────────────────────────────────────────────────────

use marlowe_surface::app::{Action, Key};
use marlowe_view::Item;

/// A view holding runs the boot-time one had never heard of — which is what a live `/runs` produces
/// and what `App::new` could never have seen.
fn view_with_runs(app: &marlowe_surface::app::App, n: usize) -> marlowe_view::SessionView {
    let mut view = app.view().clone();
    let keys = ['b', 'd', 'e', 'f', 'g'];
    view.runs = (0..n)
        .map(|i| {
            Item::new(
                &format!("run-{i}"),
                keys[i],
                marlowe_view::Tone::Normal,
                &[("running", marlowe_view::Tone::Normal)],
            )
            .identified(format!("0000000{i}-0000-4000-8000-000000000000"))
        })
        .collect();
    view
}

/// **The defect: the key registry was built once, in `App::new`, and never again.**
///
/// That was correct while the pane held whatever the connect-time snapshot produced — the registry
/// and the view came from one view and could not disagree. Making `/runs` live broke it: the pane
/// filled with runs the registry had never seen, every one drew a hotkey on its border, and
/// `resolve` missed all of them. §B10's words for exactly this: *"the borders are then lying."*
#[test]
fn a_run_that_arrived_after_startup_answers_to_the_key_on_its_border() {
    let mut app = common::app();
    app.tab = Tab::Runs;
    let view = view_with_runs(&app, 3);
    app.update(view);

    app.on_key(Key::Char('b'));
    assert!(
        matches!(app.focus, RegionId::Item(_, 0)),
        "the key printed on the first run's border did nothing: focus is {:?}",
        app.focus
    );

    // ...and it is the row the key belongs to, not merely *a* row.
    app.on_key(Key::Char('e'));
    assert!(matches!(app.focus, RegionId::Item(_, 2)), "{:?}", app.focus);
}

/// The control for the test above: with no runs in the view the same key must do nothing, so the
/// assertion is about the registry tracking the pane rather than about `b` being bound to anything.
#[test]
fn the_same_key_does_nothing_when_the_pane_is_empty() {
    let mut app = common::app();
    app.tab = Tab::Runs;
    let mut view = app.view().clone();
    view.runs.clear();
    app.update(view);
    let before = app.focus;
    app.on_key(Key::Char('b'));
    assert_eq!(app.focus, before, "an empty pane answered a run key");
}

/// **A run listed with no way to act on it is a listing, not a pane.** The only route to a window
/// was typing `/watch <name>` from memory — the affordance the pane exists to replace.
#[test]
fn enter_on_a_focused_run_opens_its_window() {
    let mut app = common::app();
    app.tab = Tab::Runs;
    app.update(view_with_runs(&app, 2));
    app.on_key(Key::Char('d'));
    assert_eq!(app.on_key(Key::Enter), Action::Redraw);

    // Both halves of Session F's split: the spawn is the driver's, the pane refresh the producer's.
    let asks = app.drain_window_asks();
    assert!(
        asks.iter().any(|o| matches!(o, Outcome::Watch(r) if r.ends_with("-0000-4000-8000-000000000000"))),
        "the driver was not asked to open a window: {asks:?}"
    );
    assert!(
        app.drain_intents().iter().any(|i| matches!(i, Intent::Watch { .. })),
        "the producer was not asked to refresh the run"
    );
}

/// Enter opens the run the user is looking at, and the id it carries — not the label, which is a
/// mnemonic and can be shared by two runs.
#[test]
fn enter_opens_the_run_the_row_names_rather_than_the_first_one() {
    let mut app = common::app();
    app.tab = Tab::Runs;
    app.update(view_with_runs(&app, 3));
    app.on_key(Key::Char('e'));
    app.on_key(Key::Enter);
    let asks = app.drain_window_asks();
    assert!(
        asks.iter().any(|o| matches!(o, Outcome::Watch(r) if r.starts_with("00000002"))),
        "the wrong run was opened: {asks:?}"
    );
}

/// The affordance has to be visible, or it is folklore. §B2 puts the enter keycap where Enter acts.
#[test]
fn the_focused_run_shows_that_enter_opens_it() {
    let mut app = common::app();
    app.tab = Tab::Runs;
    app.update(view_with_runs(&app, 2));
    app.on_key(Key::Char('b'));
    let text = common::buffer_text(&common::frame(&app, 140, 40));
    assert!(text.contains("watch"), "the focused run does not say what Enter does:\n{text}");

    // The control: it is on the FOCUSED row only, not on every row.
    assert_eq!(text.matches("↵ watch").count(), 1, "every row is carrying the keycap:\n{text}");
}

// ─── selecting among many runs ────────────────────────────────────────────────────────────────

/// More runs than the pane can show, so the scroll and the fold are both real.
fn many_runs(app: &marlowe_surface::app::App, n: usize) -> marlowe_view::SessionView {
    let pool = ['b', 'd', 'e', 'f', 'g', 'h', 'j', 'k', 'l', 'n', 'o', 'q', 'r', 't', 'u', 'x', 'z'];
    let mut view = app.view().clone();
    view.runs = (0..n)
        .map(|i| {
            // **Past the pool, no key** — exactly what `pane_key` now produces. A fixture that
            // handed out duplicates would take the registry down and every assertion below with
            // it, which is precisely how the real defect presented.
            let lines = [("running", marlowe_view::Tone::Normal)];
            let label = format!("run-{i:02}");
            match pool.get(i) {
                Some(k) => Item::new(&label, *k, marlowe_view::Tone::Normal, &lines),
                None => Item::unkeyed(&label, marlowe_view::Tone::Normal, &lines),
            }
            .identified(format!("{i:08}-0000-4000-8000-000000000000"))
        })
        .collect();
    view
}

#[test]
fn the_arrows_step_from_one_run_to_the_next() {
    let mut app = common::app();
    app.tab = Tab::Runs;
    app.update(many_runs(&app, 5));
    app.on_key(Key::Char('b'));
    assert!(matches!(app.focus, RegionId::Item(_, 0)));

    app.on_key(Key::Down);
    assert!(matches!(app.focus, RegionId::Item(_, 1)), "Down did not move: {:?}", app.focus);
    app.on_key(Key::Down);
    app.on_key(Key::Up);
    assert!(matches!(app.focus, RegionId::Item(_, 1)), "Up did not come back: {:?}", app.focus);
}

/// The ends are stops, not wraps. A list that wrapped would make "am I at the bottom" unanswerable
/// without counting.
#[test]
fn the_arrows_stop_at_the_ends_rather_than_wrapping() {
    let mut app = common::app();
    app.tab = Tab::Runs;
    app.update(many_runs(&app, 3));
    app.on_key(Key::Char('b'));
    app.on_key(Key::Up);
    assert!(matches!(app.focus, RegionId::Item(_, 0)), "Up wrapped off the top: {:?}", app.focus);

    for _ in 0..10 {
        app.on_key(Key::Down);
    }
    assert!(matches!(app.focus, RegionId::Item(_, 2)), "Down ran past the end: {:?}", app.focus);
}

/// **Selection that walks off the fold is selection you cannot see.** With more runs than fit, the
/// pane has to follow the focus down.
#[test]
fn stepping_past_the_fold_scrolls_the_pane_to_keep_the_selection_visible() {
    let mut app = common::app();
    app.tab = Tab::Runs;
    app.update(many_runs(&app, 30));
    app.on_key(Key::Char('b'));
    // Draw once so the pane knows how much it can scroll.
    let _ = common::frame(&app, 140, 40);
    assert_eq!(app.inspector_scroll, 0);

    for _ in 0..25 {
        app.on_key(Key::Down);
        let _ = common::frame(&app, 140, 40);
    }
    assert!(app.inspector_scroll > 0, "the pane never followed the selection down");

    let text = common::buffer_text(&common::frame(&app, 140, 40));
    assert!(
        text.contains("run-25"),
        "the focused run is below the fold and cannot be seen:\n{text}"
    );
}

#[test]
fn a_run_below_the_fold_is_reachable_and_then_openable() {
    // The whole point of scrolling a list of runs: reach one, and act on it.
    let mut app = common::app();
    app.tab = Tab::Runs;
    app.update(many_runs(&app, 30));
    app.on_key(Key::Char('b'));
    for _ in 0..20 {
        app.on_key(Key::Down);
        let _ = common::frame(&app, 140, 40);
    }
    app.on_key(Key::Enter);
    let asks = app.drain_window_asks();
    assert!(
        asks.iter().any(|o| matches!(o, Outcome::Watch(r) if r.starts_with("00000020"))),
        "the run below the fold did not open: {asks:?}"
    );
}

/// **The pointer is the only way to reach the eighteenth run**, because `pane_key` runs out of
/// letters at seventeen. That is what makes item hit-testing more than a convenience.
#[test]
fn every_visible_run_has_a_rect_the_pointer_can_land_on() {
    let app = {
        let mut a = common::app();
        a.tab = Tab::Runs;
        let v = many_runs(&a, 30);
        a.update(v);
        a
    };
    let area = ratatui::layout::Rect::new(0, 0, 140, 40);
    let chrome = marlowe_surface::render::chrome_for(&app, area);
    let rects = marlowe_surface::render::item_rects(&app, &chrome);

    assert!(rects.len() > 1, "the pane offered no clickable rows: {rects:?}");
    // Every rect is inside the pane and none overlaps its neighbour — an overlap would mean two
    // rows claiming one cell, and a click landing on whichever was tested first.
    for w in rects.windows(2) {
        let (a, b) = (w[0].1, w[1].1);
        assert!(a.y + a.height <= b.y, "rows {a:?} and {b:?} overlap");
    }
    for (_, r) in &rects {
        assert!(
            r.y >= chrome.inspector_scroll.y
                && r.y + r.height <= chrome.inspector_scroll.bottom(),
            "a row is drawn outside the pane: {r:?}"
        );
    }
}

/// The geometry the pointer uses is the geometry the draw used — one definition, or the click
/// targets sit somewhere other than the borders.
#[test]
fn the_rect_of_a_row_is_where_that_row_is_actually_drawn() {
    let mut app = common::app();
    app.tab = Tab::Runs;
    app.update(many_runs(&app, 6));
    let area = ratatui::layout::Rect::new(0, 0, 140, 40);
    let chrome = marlowe_surface::render::chrome_for(&app, area);
    let buf = common::frame(&app, 140, 40);

    for (i, r) in marlowe_surface::render::item_rects(&app, &chrome) {
        let label = format!("run-{i:02}");
        let band: String = (r.y..r.y + r.height)
            .map(|y| common::row_text(&buf, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(band.contains(&label), "{label} is not inside the rect claimed for it:\n{band}");
    }
}
