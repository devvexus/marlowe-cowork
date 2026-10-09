//! **The head line a person actually reads: `thinking… 412 tokens`, then `thought 412 tokens`.**
//!
//! It said `characters`, on both states, and `text.len()` was where the number came from. The unit
//! was wrong and the source was worse: a renderer counting the string it was handed can only ever
//! report a property of that string, and what a person wants to know is what the model spent.
//!
//! Everything here is asserted on a drawn `ratatui::Buffer`. Asserting on the `String` the head
//! line was built from would be a statement about this test's own arithmetic — the same reason
//! `markdown_render.rs` says so in its header.

mod common;

use marlowe_surface::app::App;
use marlowe_view::Entry;

/// Draw one reasoning block, collapsed or expanded, and return the whole screen as text.
fn screen(text: &str, tokens: u64, done: bool) -> String {
    let producer = marlowe_stub::Session::new();
    let mut view = producer.view().clone();
    view.transcript.clear();
    view.transcript.push(Entry::Reasoning { text: text.into(), tokens, done });
    let app = App::new(view).expect("the shipped key set has no conflicts");
    common::buffer_text(&common::frame(&app, 120, 30))
}

#[test]
fn a_live_block_reports_tokens_and_a_finished_one_reports_what_it_cost() {
    let live = screen("weighing the options", 412, false);
    assert!(live.contains("thinking… 412 tokens"), "live head line:\n{live}");

    let done = screen("weighing the options", 412, true);
    assert!(done.contains("thought 412 tokens"), "finished head line:\n{done}");
}

/// **The negative control, and it is the point of the change.**
///
/// `"weighing the options"` is twenty characters and, here, four tokens. If the line were still
/// counting the string — or estimating tokens from it — the number on screen would track the text
/// rather than the count, and no assertion about the *unit* would notice. So this fixes the text
/// and varies only the count.
#[test]
fn the_number_follows_the_count_and_not_the_text() {
    let four = screen("weighing the options", 4, true);
    let nine_hundred = screen("weighing the options", 900, true);

    assert!(four.contains("thought 4 tokens"), "{four}");
    assert!(nine_hundred.contains("thought 900 tokens"), "{nine_hundred}");
    // Twenty characters, and neither frame says so.
    assert!(!four.contains("20"), "the head line is still reporting the string:\n{four}");
    assert!(!four.contains("characters"), "the old unit survived:\n{four}");
}

#[test]
fn one_token_is_singular() {
    let one = screen("hm", 1, true);
    assert!(one.contains("thought 1 token"), "{one}");
    assert!(!one.contains("1 tokens"), "{one}");
}

/// **An unknown count renders as no count, never as zero.**
///
/// A replayed turn has the reasoning text and never had the number — `WireTurn` carries what the
/// endpoint documents, and a token count is not part of that shape. `thought 0 tokens` would be a
/// claim about a model that plainly did think; the absence of a figure is the truth, and it is the
/// rule `Event::Approval`'s `novelty` already follows.
#[test]
fn a_count_that_was_never_recorded_renders_as_missing_rather_than_zero() {
    let replayed = screen("weighing the options", 0, true);
    assert!(replayed.contains("thought"), "the block is still drawn:\n{replayed}");
    assert!(!replayed.contains("0 token"), "a zero was asserted:\n{replayed}");
    assert!(!replayed.contains("tokens"), "a unit with no number:\n{replayed}");
}
