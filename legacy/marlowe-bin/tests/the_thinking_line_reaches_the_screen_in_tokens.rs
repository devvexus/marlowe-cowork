//! **`thinking… 226 tokens` under a finished answer, asserted on the GRID.**
//!
//! # Why this test binary lives in `marlowe`
//!
//! The claim is about a path. The count is produced by a provider, put on the wire by the daemon's
//! `to_wire`, accumulated by `marlowe_daemon::project`, and painted by `marlowe_surface::render`.
//! `marlowe-surface` deliberately cannot see `marlowe-daemon`, so no test inside it crosses that
//! seam — the same reason `tool_detail_reaches_the_screen.rs` lives here.
//!
//! That seam is exactly where this broke. `marlowe-provider` had tests, `marlowe-daemon` had tests,
//! `marlowe-surface` had tests, all green, and the product showed a stale line — because the fold
//! closed a reasoning block only while it was still the last entry, and nothing rendered a fold of
//! a *tool-using* turn. **A boundary is not verified until something crosses it.**
//!
//! # The event order is copied off the wire, not invented
//!
//! Read from a live daemon on `marlowe-mini:2b`, asking it to read a file:
//!
//! ```text
//! R×58  R0  TOOL(running) TOOL(ok)  R×297  T×9  R0  DONE
//! ```
//!
//! `R0` is the settlement — an empty reasoning chunk carrying the tokens the engine counted and
//! never streamed. Two model calls, so two reasoning blocks, and the first one has a tool line
//! sitting on top of it.

use marlowe_daemon::protocol::{Event, StatusReport};
use marlowe_daemon::{apply_events, view_from_status};
use marlowe_surface::app::App;
use marlowe_surface::{render, Theme};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn report() -> StatusReport {
    StatusReport {
        version: "0.1.0".into(),
        workspace: "/ws".into(),
        model: "marlowe-mini:2b".into(),
        model_disclosure: "marlowe-mini:2b".into(),
        degraded: None,
        rerank_provider: "cpu".into(),
        model_provider: "ollama".into(),
        live_runs: 0,
        models: Vec::new(),
        announcements: Vec::new(),
        uptime_ms: 0,
    }
}

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

fn tool(state: &str) -> Event {
    Event::Tool {
        id: 1,
        verb: "read".into(),
        target: "README.md".into(),
        state: state.into(),
        summary: "48 lines".into(),
        detail: None,
    }
}

/// The turn from the trace above, drawn.
fn a_tool_using_turn() -> Vec<Event> {
    vec![
        Event::Reasoning { delta: "which file do they mean".into(), tokens: 58 },
        Event::Reasoning { delta: String::new(), tokens: 4 },
        tool("running"),
        tool("ok"),
        Event::Reasoning { delta: "now the first line".into(), tokens: 297 },
        Event::Text { delta: "It is a heading.".into() },
        Event::Reasoning { delta: String::new(), tokens: 6 },
        Event::Done {
            outcome: "completed".into(),
            detail: String::new(),
            spend_micros_usd: 0,
            elapsed_ms: 826,
        },
    ]
}

#[test]
fn a_finished_turn_draws_no_thinking_line_and_both_thoughts_report_their_tokens() {
    let text = screen(&a_tool_using_turn());

    // The reported symptom, asserted as an absence on the grid.
    assert!(
        !text.contains("thinking…"),
        "the turn has answered; nothing is still thinking:\n{text}"
    );
    // 58 + 4, and 297 + 6 — each block carrying its own settlement, neither carrying the other's.
    assert!(text.contains("thought 62 tokens"), "the first thought:\n{text}");
    assert!(text.contains("thought 303 tokens"), "the second thought:\n{text}");
    assert!(text.contains("It is a heading."), "the answer is still drawn:\n{text}");
}

/// **The control, and without it the assertion above is satisfied by never drawing `thinking…`.**
///
/// Mid-turn — the same events with the answer and everything after it withheld — the live block
/// must read `thinking…`, because §B5's whole point is that motion means Marlowe is working. A
/// sweep that closed every block would pass the test above and break the product.
#[test]
fn the_same_turn_mid_flight_still_says_thinking() {
    let mut events = a_tool_using_turn();
    events.truncate(5); // up to and including the second block's first chunk

    let text = screen(&events);
    assert!(text.contains("thinking… 297 tokens"), "the live block:\n{text}");
    // …and the one the tool line landed on is already finished, in the same frame.
    assert!(text.contains("thought 62 tokens"), "the block behind the tool line:\n{text}");
}
