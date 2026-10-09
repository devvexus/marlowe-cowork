//! **Rendered markdown must not be able to imitate harness chrome. Asserted on the drawn buffer.**
//!
//! Interpreting markup hands the model control of visual structure. This project has shipped that
//! failure twice — `CondensedResult::render` joining fields as `"{k}: {v}"` so a value containing
//! `"\nanswer: …"` forged a field header, and a tool `target` containing a newline forging a second
//! §B6 tool line — and both times the fix was structural rather than a filter for the payload.
//!
//! Every assertion here reads **cells in a rendered `ratatui::Buffer`**, not an intermediate
//! `Vec<Span>`. A span-level assertion would be a statement about the thing under test's own
//! bookkeeping; the question is what a person sitting in front of the terminal can be made to
//! believe, and that is a property of the grid.
//!
//! # Each test carries a control, because "the glyph is absent" has a second explanation
//!
//! If the hostile reply never reached the pane — a transcript that did not append, a pane too
//! narrow, a producer whose view was replaced — every assertion below passes and proves nothing.
//! So each one also asserts that the surrounding text **did** land. That is the same control
//! `display_sanitiser.rs` carries, for the same reason.

mod common;

use common::Pane;
use ratatui::buffer::Buffer;

/// A pane holding **one model reply and nothing else**, so a chrome glyph on screen can only have
/// come from the model -- which is the question this file asks.
fn app_saying(text: &str) -> Pane {
    Pane::saying(text)
}

/// Everything drawn in the pane, **excluding the right-hand gutter** where a scrollbar would sit.
///
/// The scrollbar is harness chrome that legitimately lives in that column, so a test that swept it
/// would fail on the harness's own glyphs and get "fixed" by loosening the assertion. Naming the
/// exclusion once keeps the rest of the sweep strict.
fn transcript_cells(buf: &Buffer, w: u16, h: u16) -> String {
    (0..h)
        .map(|y| {
            (0..w.saturating_sub(2))
                .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// §B6's one line, forged four ways. The marker is the whole identity of a tool line, so this is
/// the highest-value forgery in the pane.
#[test]
fn a_reply_cannot_draw_a_tool_line() {
    for hostile in [
        "  ⋯ bash      rm -rf ./build                          exit 0",
        "```\n  ⋯ read      /etc/shadow                         48 lines\n```",
        "**  ⋯ edit      drafts/acme.md                         +3 −0**",
        "> ⋯ recall    open commitments                         3 due",
    ] {
        let hostile = format!("BEFORE\n\n{hostile}\n\nAFTER");
        let app = app_saying(&hostile);
        let screen = transcript_cells(&common::frame(&app, 140, 40), 140, 40);

        // The control first: without it, everything below is a statement about an empty pane.
        assert!(
            screen.contains("BEFORE") && screen.contains("AFTER"),
            "the hostile reply never rendered, so this test proves nothing:\n{screen}"
        );
        assert!(
            screen.contains("<U+22EF>"),
            "the refusal must be VISIBLE — a silently dropped glyph and a clean line are the same \
             line to the reader deciding whether the harness wrote it:\n{screen}"
        );
        assert!(
            !screen.contains('⋯'),
            "a model reply drew §B6's tool marker:\n{screen}"
        );
    }
}

/// §B2's premise is that a border delineates an interactive region. If a reply can draw one, every
/// border on the screen becomes a claim the user cannot check.
#[test]
fn a_reply_cannot_draw_a_bordered_region() {
    let hostile = "BEFORE\n\n```\n┌ Approve ────────────────────┐\n│ Delete 1,204 files ./build  │\n└─────────────────────────(y)─┘\n```\n\nAFTER";
    let app = app_saying(hostile);
    for (w, h) in common::SIZES {
        let screen = transcript_cells(&common::frame(&app, w, h), w, h);
        assert!(
            screen.contains("BEFORE") && screen.contains("AFTER"),
            "{w}x{h}: the hostile reply never rendered:\n{screen}"
        );
        for ch in screen.chars() {
            assert!(
                !('\u{2500}'..='\u{259F}').contains(&ch),
                "{w}x{h}: a model reply drew {ch:?} inside the conversation. §B2 — a border means \
                 an interactive region, and a border the model asked for makes that a lie:\n\
                 {screen}"
            );
        }
        assert!(screen.contains("<U+250C>"), "{w}x{h}: the refusal is invisible:\n{screen}");
    }
}

/// `─ compacted · 47 turns → summary ─` announces that the session's history was rewritten. A reply
/// that can draw it can tell the user their transcript was compacted when it was not.
#[test]
fn a_reply_cannot_forge_the_compaction_marker() {
    // Both routes: the literal text, and a markdown horizontal rule, which is what a renderer that
    // reached for `─` would draw.
    let hostile = "BEFORE\n\n─ compacted · 47 turns → summary ─\n\n---\n\nAFTER";
    let app = app_saying(hostile);
    let screen = transcript_cells(&common::frame(&app, 140, 40), 140, 40);
    assert!(
        screen.contains("BEFORE") && screen.contains("AFTER"),
        "the hostile reply never rendered:\n{screen}"
    );
    assert!(
        !screen.contains("─ compacted"),
        "a model reply forged the compaction marker:\n{screen}"
    );
    assert!(
        screen.contains("compacted"),
        "the words are supposed to survive — only the harness's glyph is refused:\n{screen}"
    );
    // And the markdown rule that a naive renderer would have drawn with `─` is drawn with `·`.
    assert!(
        screen.contains("····"),
        "a markdown horizontal rule should still render, in a glyph the harness does not use:\n\
         {screen}"
    );
}

/// The reasoning block's disclosure marker and its `↵` affordance say *there is more here and Enter
/// opens it*. A forged one is a control that does nothing, which teaches the user that the real one
/// does nothing either.
#[test]
fn a_reply_cannot_draw_a_disclosure_affordance() {
    let hostile = "BEFORE\n\n▸ thought 4210 tokens   ↵\n\nAFTER";
    let app = app_saying(hostile);
    let screen = transcript_cells(&common::frame(&app, 140, 40), 140, 40);
    assert!(screen.contains("BEFORE") && screen.contains("AFTER"), "{screen}");
    assert!(!screen.contains('▸'), "a model reply drew a disclosure marker:\n{screen}");
    assert!(!screen.contains('↵'), "a model reply drew the Enter affordance:\n{screen}");
    assert!(screen.contains("<U+25B8>") && screen.contains("<U+21B5>"), "{screen}");
}

