//! **What ratatui filters, what it does not, and which layer is doing the defending.**
//!
//! # Amended by ADR-047, and the amendment includes a correction
//!
//! This file used to say, in its first line, *"the TUI does not sanitise."* That is **no longer
//! true of model prose**: `chrome::prepare_model_text` runs `marlowe_contract::text::sanitize_prose`
//! over every model reply before the markdown parser sees it, because interpreting markup means the
//! block structure and the wrap arithmetic now depend on where a line ends and how wide a character
//! is. The reasons are in ADR-047 §7.
//!
//! **That change made the old version of this file vacuous about its own subject**, and the failure
//! is worth naming because it is the one this project keeps logging. The tests below asserted that
//! a hostile character does not reach the grid, using `Entry::Said(Speech::Model(_))` — the path
//! that now has a sanitiser in front of it. They would have stayed green with ratatui's filtering
//! removed entirely, while claiming to be the thing that noticed. **A characterisation test of a
//! dependency has to use a path where the dependency is the only thing in the way.**
//!
//! So they use `Entry::User` instead, which is still rendered flat — the user typed it, and showing
//! their own `**` back to them as bold would be rewriting their words (ADR-047 §3.3).
//!
//! # What was measured, rather than assumed
//!
//! ADR-047 asserted in draft that ratatui *"says nothing about U+2028, the BiDi overrides, or the
//! zero-width block."* **That was wrong, and one probe showed it.** ratatui discards all of them.
//! What it does **not** discard is the block below, and that is the finding:
//!
//! | codepoint | reaches the `Buffer` |
//! |---|---|
//! | `ESC` U+001B, `BEL` U+0007, C0/C1 generally | no |
//! | `TAB` U+0009 | no |
//! | RIGHT-TO-LEFT OVERRIDE U+202E | no |
//! | ZERO WIDTH SPACE U+200B, BOM U+FEFF | no |
//! | LINE SEPARATOR U+2028 | no |
//! | **TAG characters U+E0000–U+E007F** | **YES** |
//!
//! The tag block is the classic invisible-instruction smuggling channel, and
//! `marlowe_contract::text::is_renderable` refuses it by name. So the two layers are complementary
//! rather than redundant, which is a better reason to run both than the one the draft gave.
//!
//! # This is still a characterisation test, not a guard
//!
//! It records **which component is doing the defending**, so the day a ratatui upgrade changes that
//! behaviour something fails with a name attached instead of an escape sequence quietly reaching a
//! terminal. The classic CLI's own guards are unit tests at their enforcement sites — `cli.rs`'s
//! `display_sanitiser` module and `marlowe/src/agent.rs`'s. Those are guards. This is not.

mod common;

use marlowe_surface::app::App;
use marlowe_view::notice::Speech;
use marlowe_view::Entry;

/// One of each family, chosen because each defeats a *different* defence.
const HOSTILE: [(char, &str); 5] = [
    ('\u{1b}', "ESC — the escape that starts every ANSI sequence"),
    ('\u{202e}', "RIGHT-TO-LEFT OVERRIDE — Trojan Source, reverses displayed order"),
    ('\u{200b}', "ZERO WIDTH SPACE — occupies no columns while carrying bytes"),
    ('\u{2028}', "LINE SEPARATOR — a break str::lines() does not see"),
    ('\u{feff}', "BOM as a zero-width joiner"),
];

/// The **flat** render path: `Entry::User` goes to the grid without a sanitiser in front of it, so
/// what happens to a character here is a statement about ratatui.
fn app_with_user_text(text: String) -> App {
    let producer = marlowe_stub::Session::new();
    let mut view = producer.view().clone();
    view.transcript.clear();
    view.transcript.push(Entry::User(text));
    App::new(view).unwrap()
}

fn app_saying(text: String) -> App {
    let producer = marlowe_stub::Session::new();
    let mut view = producer.view().clone();
    view.transcript.clear();
    view.transcript.push(Entry::Said(Speech::Model(text)));
    App::new(view).unwrap()
}

#[test]
fn ratatui_is_the_layer_that_stops_an_escape_reaching_the_grid() {
    for (c, why) in HOSTILE {
        // Wrapped in ordinary words so the vacuity control below can prove the text arrived.
        let app = app_with_user_text(format!("BEFORE{c}AFTER"));
        let buf = common::frame(&app, 140, 40);
        let screen = common::buffer_text(&buf);

        assert!(
            !screen.contains(c),
            "{why}: it reached the ratatui Buffer.\n\
             THIS IS THE ENFORCING LAYER MOVING, not a test being wrong — the flat render paths do \
             not sanitise, so if ratatui stops filtering this character they are undefended. See \
             this file's header.\n{screen}"
        );
    }
}

#[test]
fn the_hostile_text_actually_reached_the_screen() {
    // **The vacuity control, and it is the whole reason the test above means anything.**
    //
    // Every assertion up there passes if the text never rendered at all — a transcript that did
    // not append, a pane too small, a stub whose view was replaced. This asserts the surrounding
    // characters DID land on the grid, so "the escape is absent" is a statement about the escape
    // rather than about an empty buffer.
    let app = app_with_user_text("BEFORE\u{1b}AFTER".to_string());
    let screen = common::buffer_text(&common::frame(&app, 140, 40));
    assert!(screen.contains("BEFORE"), "the text never rendered:\n{screen}");
    assert!(screen.contains("AFTER"), "the text never rendered:\n{screen}");
}

/// **The gap ratatui leaves, measured rather than assumed.**
///
/// The tag block carries a full invisible ASCII alphabet — U+E0041 displays as nothing and is the
/// documented channel for smuggling instructions past a human reader. ratatui passes it through.
/// `marlowe_contract::text::is_renderable` refuses it, which is why ADR-047 runs both layers rather
/// than treating one as redundant.
///
/// If this ever starts failing, ratatui has begun filtering the block too — good news, and the
/// argument in ADR-047 §7 should be re-read rather than the test deleted.
#[test]
fn ratatui_does_not_filter_the_tag_block_and_the_project_predicate_does() {
    let tag = '\u{E0041}'; // TAG LATIN CAPITAL LETTER A
    let screen = common::buffer_text(&common::frame(
        &app_with_user_text(format!("BEFORE{tag}AFTER")),
        140,
        40,
    ));
    assert!(screen.contains("BEFORE") && screen.contains("AFTER"), "{screen}");
    assert!(
        screen.contains(tag),
        "ratatui now filters the tag block. That is an improvement, not a failure — but ADR-047 §7 \
         argues from this gap, so read it before deleting this test:\n{screen}"
    );
    assert!(
        !marlowe_contract::text::is_renderable(tag),
        "the project's display predicate must refuse the tag block, or nothing does"
    );
}

/// And the consequence: **the model path is covered where the flat path is not.**
///
/// This is the assertion that makes ADR-047's sanitiser non-decorative. Same character, same frame
/// size, two entry kinds, two outcomes — so "the model path is sanitised" is a difference that was
/// observed rather than a claim about a function call.
#[test]
fn a_tag_character_reaches_the_grid_from_the_user_and_never_from_the_model() {
    let tag = '\u{E0041}';
    let from_user = common::buffer_text(&common::frame(
        &app_with_user_text(format!("BEFORE{tag}AFTER")),
        140,
        40,
    ));
    let from_model = common::buffer_text(&common::frame(
        &app_saying(format!("BEFORE{tag}AFTER")),
        140,
        40,
    ));

    assert!(from_user.contains(tag), "the control did not reproduce:\n{from_user}");
    assert!(
        !from_model.contains(tag),
        "a tag character reached the grid from a MODEL reply. `chrome::prepare_model_text` is \
         supposed to be in that path:\n{from_model}"
    );
    assert!(
        from_model.contains("<U+E0041>"),
        "refused, but silently — the marker is what tells the reader something was removed:\n\
         {from_model}"
    );
    assert!(from_model.contains("BEFORE") && from_model.contains("AFTER"), "{from_model}");
}
