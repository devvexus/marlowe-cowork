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

use marlowe_surface::app::App;
use marlowe_surface::render;
use marlowe_view::notice::Speech;
use marlowe_view::Entry;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

/// A surface whose conversation holds **one model reply and nothing else**.
///
/// The stub's opening transcript carries real §B6 tool lines, and those legitimately contain `⋯`.
/// Asserting "no tool marker on screen" against it would be asserting about whatever happened to be
/// scrolled into view, and it would fail for the right reason on the wrong evidence. With the
/// transcript cleared, a chrome glyph in the pane can only have come from the model — which is the
/// question this file asks. That the harness itself still draws every one of those glyphs is
/// `every_reserved_glyph_is_one_the_harness_actually_draws`, below.
fn app_saying(text: &str) -> App {
    let producer = marlowe_stub::Session::new();
    let mut view = producer.view().clone();
    view.transcript.clear();
    view.transcript
        .push(Entry::Said(Speech::Model(text.to_string())));
    App::new(view).expect("the shipped key set has no conflicts")
}

/// Everything drawn inside the conversation's scroll area, **excluding the scrollbar column**.
///
/// The scrollbar is harness chrome that legitimately lives inside the scroll rect, so a test that
/// swept the whole rect would fail on the harness's own glyphs and get "fixed" by loosening the
/// assertion. Naming the exclusion once keeps the rest of the sweep strict.
fn transcript_cells(buf: &Buffer, w: u16, h: u16) -> String {
    let c = render::layout(Rect::new(0, 0, w, h));
    let r = c.conversation_scroll;
    (r.y..r.bottom())
        .map(|y| {
            (r.x..r.right() - 1)
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

/// **The other half of the reservation, and the half that rots silently.**
///
/// Reserving a glyph the harness has stopped drawing guards nothing, and nothing would report it —
/// the forgery tests above would still pass, because the thing being forged no longer exists. This
/// is the fourteenth-instance shape (*a guard is a claim, and a claim needs something checking its
/// subject is still there*) applied to a glyph rather than to a file path.
#[test]
fn every_reserved_glyph_is_one_the_harness_actually_draws() {
    // Every kind of chrome: tool lines from the stub's opening transcript, a collapsed reasoning
    // block, a model reply with a block quote, borders from every region, and a transcript long
    // enough to need a scrollbar.
    let producer = marlowe_stub::Session::new();
    let mut view = producer.view().clone();
    // Filler first, so the pane overflows and a scrollbar exists at all; then the two entries whose
    // glyphs only appear at the bottom.
    for n in 0..40 {
        view.transcript
            .push(Entry::Said(Speech::Model(format!("filler line {n}"))));
    }
    view.transcript.push(Entry::Reasoning {
        text: "weighing the options".into(),
        tokens: 4,
        done: true,
    });
    view.transcript.push(Entry::Said(Speech::Model(
        "> a quotation, which draws the quote rule".into(),
    )));
    let mut app = App::new(view).expect("the shipped key set has no conflicts");
    // Two frames, top and bottom. The claim is *the harness draws this glyph*, not *in one frame* —
    // the stub's tool lines are at the top of the transcript and the reasoning block is at the
    // bottom, and no single viewport holds both.
    let bottom = common::buffer_text(&common::frame(&app, 140, 40));
    app.scroll = Some(0);
    let top = common::buffer_text(&common::frame(&app, 140, 40));
    let screen = format!("{top}\n{bottom}");

    for c in marlowe_surface::chrome::MARKERS {
        assert!(
            screen.contains(c),
            "U+{:04X} ({c:?}) is reserved from model prose and the harness does not draw it \
             anywhere in this frame. Either it moved — in which case the reservation is guarding \
             a glyph nobody uses — or it was deleted, in which case delete the entry. A guard is a \
             claim about something existing.\n{screen}",
            c as u32
        );
    }
    println!(
        "reserved glyphs: {} declared, {} of them drawn by the harness in one frame",
        marlowe_surface::chrome::MARKERS.len(),
        marlowe_surface::chrome::MARKERS.len()
    );
}

/// Content is clipped to its region by the layout, and this asserts it rather than assuming it.
/// The status band, the approval overlay and the message field all sit outside the conversation;
/// a reply that could write into them would be forging the parts of the screen that carry the
/// harness's own claims about what is happening.
#[test]
fn a_reply_writes_nowhere_but_the_conversation() {
    for (w, h) in common::SIZES {
        let quiet = app_saying("a short reply");
        let loud = app_saying(
            &"# Heading\n\nA very long paragraph that will wrap many times over. ".repeat(40),
        );
        let before = common::frame(&quiet, w, h);
        let after = common::frame(&loud, w, h);
        let c = render::layout(Rect::new(0, 0, w, h));
        let strays: Vec<_> = common::diff_cells(&before, &after)
            .into_iter()
            .filter(|(x, y)| {
                !(*x >= c.conversation.x
                    && *x < c.conversation.right()
                    && *y >= c.conversation.y
                    && *y < c.conversation.bottom())
            })
            .collect();
        assert!(
            strays.is_empty(),
            "{w}x{h}: a model reply changed {} cells outside the conversation, first at {:?}",
            strays.len(),
            strays.first()
        );
    }
    println!("model prose reaching outside the conversation region: 0 cells across 5 sizes");
}

/// **A tool target is model-composed and reaches the grid inside a §B6 line.**
///
/// CLAUDE.md's own example is a `target` containing a newline forging a second tool line. The TUI
/// was covered incidentally, by ratatui's filtering — and ADR-047 measured that filtering and found
/// the **tag block passes through it**: U+E0000–U+E007F is an invisible ASCII alphabet, and the
/// place it would matter most is the one line that tells the user what the agent just did.
#[test]
fn a_tool_target_carries_no_invisible_text() {
    use marlowe_view::turn::Metric;
    use marlowe_view::ToolCall;

    let producer = marlowe_stub::Session::new();
    let mut view = producer.view().clone();
    view.transcript.clear();
    // TAG LATIN SMALL R and M — the invisible alphabet. Two is enough: each expands to eight
    // visible columns once marked, and the assertion below needs the trailing word still to fit.
    let hidden = "\u{E0072}\u{E006D}";
    view.transcript.push(Entry::Tools(vec![ToolCall::ok(
        1,
        "read",
        &format!("BEFORE{hidden}AFTER"),
        vec![Metric::Count { n: 48, unit: "lines" }],
    )]));
    let app = App::new(view).expect("the shipped key set has no conflicts");
    let screen = transcript_cells(&common::frame(&app, 140, 40), 140, 40);

    assert!(
        screen.contains("BEFORE") && screen.contains("AFTER"),
        "the tool line never rendered:\n{screen}"
    );
    for c in hidden.chars() {
        assert!(
            !screen.contains(c),
            "U+{:05X} reached a §B6 tool line. It occupies no columns and carries bytes, inside \
             the one line that states what the agent did:\n{screen}",
            c as u32
        );
    }
    assert!(screen.contains("<U+E0072>"), "refused, but invisibly:\n{screen}");
}

/// The harness's own prose is **not** put through the reservation, and it must not be — a `/help`
/// listing that lost its glyphs would be the cure being worse than the disease.
///
/// This is the control on the whole mechanism's *scope*: it fires if somebody later routes notices
/// through `prepare_model_text` for symmetry.
#[test]
fn harness_notices_are_not_subject_to_the_reservation() {
    let mut r = common::rig();
    r.app.input = "/help".into();
    r.app.submit();
    r.settle(0);
    let screen = common::buffer_text(&common::frame(&r.app, 160, 45));
    assert!(
        !screen.contains("<U+"),
        "a harness notice was put through the model-prose reservation:\n{screen}"
    );
}
