//! **§B6's *"Enter or Tab for full output in place"*, asserted on the GRID.**
//!
//! # Why this test binary lives in `marlowe` and not in `marlowe-surface`
//!
//! The claim is about a path, not a struct. `Event::Tool.detail` is filled by the daemon's
//! `to_wire`, folded into a `ToolCall` by `marlowe_daemon::project`, and painted by
//! `marlowe_surface::render`. `marlowe-surface` deliberately cannot see `marlowe-daemon` — its
//! `Cargo.toml` says so, at length — so no test inside it can cross that seam. This crate depends
//! on both, and it is the only one that does.
//!
//! # What these assert, and what they would read if the feature were absent
//!
//! Every assertion here is on **rendered cells**, never on a field. CLAUDE.md's sixteenth logged
//! instance is a control asserted where it is declared rather than where it is enforced —
//! `inline_threshold_bytes == 0`, green on a build where the field has no reader.
//! `detail.is_some()` is that same sentence in a different subsystem: it passes when the string is
//! empty, when it is the wrong string, and when `expansion` never draws it.
//!
//! So the shape of every test below is a **difference between two screens**. If `detail` were
//! dropped anywhere along the path — the wire, the fold, the renderer — the two screens would be
//! identical and the assertion fails. A renderer that painted the detail unconditionally fails the
//! other half. Neither half can pass on its own.
//!
//! # Both halves were verified by mutation, not by argument
//!
//! * `expansion` changed to ignore `ResultSummary::detail` → tests 1 and 2 fail, 3 and 4 stay
//!   green (they are not about the detail, and a test that moved here would be measuring
//!   something it does not name).
//! * `render::rendered` changed to ignore `ToolCall::summary_line` → test 3 fails alone.
//!
//! `render.rs` was restored byte-identically after each, verified by `md5sum`.

use marlowe_daemon::protocol::{Event, StatusReport};
use marlowe_daemon::{apply_events, view_from_status};
use marlowe_surface::app::App;
use marlowe_surface::{render, Theme};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

/// A quiet daemon. Nothing here bears on a tool line; it exists because a `SessionView` is built
/// from a report and `App::new` will not take half of one.
fn report() -> StatusReport {
    StatusReport {
        version: "0.1.0".into(),
        workspace: "/ws".into(),
        model: "qwen3.5:9b".into(),
        model_disclosure: "qwen3.5:9b · tool calls 12/12".into(),
        degraded: None,
        rerank_provider: "cpu-sequential".into(),
        model_provider: "ollama".into(),
        live_runs: 0,
        models: Vec::new(),
        // Nothing has been announced into this fixture and nothing has been up.
        announcements: Vec::new(),
        uptime_ms: 0,
    }
}

/// Every cell the surface paints for these events, as text.
///
/// **160x45, not the 120x30 floor.** A detail wraps at the pane width, so a narrow frame can push
/// the phrase being asserted on past the bottom of the transcript and fail the test for a reason
/// that has nothing to do with `detail`. The size is part of the fixture, not an accident of it.
fn screen(events: &[Event]) -> String {
    let mut view = view_from_status(&report());
    apply_events(&mut view, events);
    let app = App::new(view).expect("the shipped key set has no conflicts");
    let mut term = Terminal::new(TestBackend::new(160, 45)).expect("a test backend");
    term.draw(|f| render::draw(&app, &Theme::default_truecolor(), f.area(), f.buffer_mut()))
        .expect("a frame");
    let buf = term.backend().buffer().clone();
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf.cell((x, y)).map_or(" ", |c| c.symbol()).to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One §B6 line, with whatever the harness had to say about it.
fn tool(state: &str, summary: &str, detail: Option<&str>) -> Event {
    Event::Tool {
        id: 1,
        verb: "write".into(),
        target: "scratchpad/notes.md".into(),
        state: state.into(),
        summary: summary.into(),
        detail: detail.map(str::to_string),
    }
}

/// The sentence under test. Long enough to wrap, and worded the way a refusal actually is, so a
/// substring match cannot succeed against the target or the summary by accident.
const REASON: &str = "the path is outside the workspace, and write refuses a path it cannot \
                      scope. Ask for a path under the workspace root, or say which directory you \
                      meant.";

/// A phrase that appears in `REASON` and nowhere else on the frame — not in the verb, the target,
/// the summary, the status band or the footer.
const PHRASE: &str = "refuses a path it cannot";

// ── 1. the headline, and the live defect it closes ────────────────────────────────────────────

/// **STATE.md, 2026-08-27: *"Seen live as `write scratchpad/… blocked` with no reason on screen:
/// the model was told why, the USER was not."*** This is that sentence as a command.
///
/// The negative control is inside the test rather than beside it, because the two screens are the
/// evidence: identical events but for `detail`, and the difference must be the reason.
#[test]
fn a_refused_write_puts_its_reason_on_the_screen_and_says_nothing_extra_without_one() {
    let with = screen(&[tool("failed", "write", Some(REASON))]);
    let without = screen(&[tool("failed", "write", None)]);

    // Both screens show the call. Without this the assertions below could be comparing two frames
    // that never had a tool line on them at all.
    assert!(with.contains("scratchpad/notes.md"), "the tool line is missing:\n{with}");
    assert!(without.contains("scratchpad/notes.md"), "the tool line is missing:\n{without}");

    // The property.
    assert!(
        with.contains(PHRASE),
        "§B6: a failure auto-expands and the reason is on screen. It is not:\n{with}"
    );

    // **The control.** `detail: None` is the only difference between these two events, so a
    // renderer that ignored `detail` — or a `to_wire` that dropped it, or a fold that put it
    // somewhere nothing paints — produces two identical frames and fails here. This is the
    // assertion that cannot be satisfied by a field merely being `Some`.
    assert!(
        !without.contains(PHRASE),
        "the reason appeared on a frame whose event carried none:\n{without}"
    );
    assert_ne!(
        with, without,
        "the same frame with and without a detail. Nothing on this path reads the field."
    );
}

// ── 2. failures auto-expand; successes do not ─────────────────────────────────────────────────

/// §B6 draws exactly one distinction here: *"Failures auto-expand. The one case where the user
/// always wants detail."* A success's output waits for a keystroke.
///
/// **The same `REASON` string is used for both states on purpose.** A test that showed one string
/// on a failure and a different string on a success would pass if the renderer keyed on the text;
/// holding the text fixed and varying only the state means the only thing that can explain the
/// difference is the state.
#[test]
fn a_failure_expands_itself_and_the_identical_text_on_a_success_stays_collapsed() {
    let failed = screen(&[tool("failed", "write", Some(REASON))]);
    let ok = screen(&[tool("ok", "+3 −0", Some(REASON))]);

    assert!(failed.contains(PHRASE), "a failure did not auto-expand:\n{failed}");
    assert!(
        !ok.contains(PHRASE),
        "a successful call printed its output unasked. §B6 expands on demand, and a `read` would \
         put a whole file between the tool line and the answer:\n{ok}"
    );
    // The success still rendered — otherwise the line above passes because nothing was drawn.
    assert!(ok.contains("scratchpad/notes.md"), "the successful call is not on screen:\n{ok}");
}

// ── 3. the summary column, which held the wrong string in the other direction ─────────────────

/// **§B6: *"Summaries are typed, never generic. `done` is not acceptable."*** The projection built
/// `metrics = [State("ok")]` and put the rendered summary in `detail`, so this column read **`ok`**
/// on every successful call while `48 lines` sat in the expansion — in the failure colour, because
/// that is what the slot used to hold. Two things in one slot, both wrong.
///
/// Asserting the column reads `48 lines` is not enough on its own: it would pass if the renderer
/// printed both. The second half asserts the word that used to be there is gone from that row.
#[test]
fn the_b6_summary_column_carries_the_metrics_and_not_the_word_ok() {
    let s = screen(&[Event::Tool {
        id: 1,
        verb: "read".into(),
        target: "Dockerfile".into(),
        state: "ok".into(),
        summary: "48 lines".into(),
        detail: None,
    }]);
    let row = s
        .lines()
        .find(|l| l.contains("Dockerfile"))
        .unwrap_or_else(|| panic!("no tool line:\n{s}"))
        .to_string();

    assert!(row.contains("48 lines"), "§B6's summary column is not the metrics: {row:?}");
    assert!(
        !row.contains(" ok"),
        "the summary column still reads the state word. §B6 forbids a generic summary, and \
         `48 lines` is the whole example the addendum gives: {row:?}"
    );
}

// ── 4. a running call has produced nothing, and says so by having nothing ─────────────────────

/// `None` is the absence, not an empty string standing in for one. A running line that inherited
/// the summary would make an expansion showing `12 ms` indistinguishable from one showing output.
#[test]
fn a_running_call_expands_to_nothing_because_it_has_produced_nothing() {
    let running = screen(&[tool("running", "12 ms", None)]);
    assert!(running.contains("scratchpad/notes.md"), "the running line is missing:\n{running}");
    assert!(
        !running.contains(PHRASE),
        "a running call showed a detail it cannot have:\n{running}"
    );
}
