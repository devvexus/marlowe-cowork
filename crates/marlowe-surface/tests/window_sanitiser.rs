//! **Audit finding E4's second clause, built and asserted where the bytes reach a screen.**
//!
//! # Where this came from
//!
//! E4 was filed with a two-part fix:
//!
//! > *Fix:* suppress `TextDelta`/`ReasoningDelta`/`SpeechRetracted` when `reads_untrusted`;
//! > **move the character check to the sink boundary.**
//!
//! Only the first part was built. `QuarantinedSink` suppresses the reader's prose and
//! `marlowe-loop/tests/quarantine_batch.rs::nothing_the_quarantined_reader_says_reaches_the_surface`
//! asserts it — **that test is unchanged and stays where it is.** The second part had no
//! implementation and therefore no test, and nothing noticed, because after the suppression there
//! was no path anyone was looking at.
//!
//! ADR-055 permits a run window to stream a run's own prose **on the condition that the second
//! clause exists**. This file is that clause's test. It is not a copy of the loop-level one and it
//! does not replace it; the two halves of E4 now live one in each place:
//!
//! | E4 clause | where |
//! |---|---|
//! | suppress the quarantined reader's prose | `marlowe-loop/tests/quarantine_batch.rs` |
//! | the character check at the display boundary | **here** |
//!
//! # Asserted on the `Buffer`, and on the output region only
//!
//! Family #16 is asserting a property where it is *declared* rather than where it is *enforced* —
//! `inline_threshold_bytes == 0` on a field no code reads, `persona/v1.md` loaded rather than in the
//! request body. So nothing here calls `window::prepared` and checks its return value. Every
//! assertion renders a real frame and searches the cells of the output region.

mod common;

use marlowe_surface::window::{self, WindowApp};
use marlowe_view::notice::Speech;
use marlowe_view::{Entry, Metric, ResultSummary, ToolCall};

/// One of each family that reaches a terminal a different way.
///
/// **The tag block is the one that matters**, and it is the reason this file cannot be replaced by
/// "ratatui handles it". ADR-047 measured what ratatui discards: C0/C1, `ESC`, `TAB`, the BiDi
/// overrides, the zero-width block and U+2028 do not reach a `Buffer`. **U+E0000–U+E007F does** — a
/// full invisible ASCII alphabet, and the documented channel for smuggling an instruction past a
/// human reader. `marlowe_contract::text::is_renderable` refuses it by name.
const HOSTILE: [(char, &str); 5] = [
    ('\u{e0041}', "TAG LATIN CAPITAL A — invisible, and the one ratatui passes through"),
    ('\u{202e}', "RIGHT-TO-LEFT OVERRIDE — reverses what a labelled line reads as"),
    ('\u{2028}', "LINE SEPARATOR — a break str::lines() does not see"),
    ('\u{1b}', "ESC — the start of every ANSI sequence"),
    ('\u{200b}', "ZERO WIDTH SPACE — width on screen and width in the wrap arithmetic diverge"),
];

fn output_region(w: u16, h: u16) -> ratatui::layout::Rect {
    window::layout(ratatui::layout::Rect::new(0, 0, w, h)).output_scroll
}

fn window_with(output: Vec<Entry>) -> WindowApp {
    let mut v = common::run_view();
    v.output = output;
    WindowApp::new(v)
}

/// The run's **prose**. ADR-055's subject: what streams to the window.
#[test]
fn no_hostile_character_in_a_runs_prose_reaches_a_cell() {
    for (c, why) in HOSTILE {
        let app = window_with(vec![Entry::Said(Speech::Model(format!(
            "the third source says{c}MARKER-PROSE"
        )))]);
        let buf = common::window_frame(&app, 120, 30);
        let text = common::region_text(&buf, output_region(120, 30));

        // **The control, and it is the whole reason this assertion means anything.** An empty
        // output region contains no hostile character either. Without this line the test passes on
        // a window that renders nothing at all.
        assert!(
            text.contains("MARKER-PROSE"),
            "{why}: the benign text did not arrive, so the absence below proves nothing:\n{text}"
        );
        assert!(!text.contains(c), "{why}: it reached a cell in the output region:\n{text}");
    }
}

/// A run's **reasoning**, expanded. ADR-047 makes reasoning parse only when expanded because it is
/// the highest-volume text in the product; expanded is the path where its bytes reach cells.
#[test]
fn no_hostile_character_in_a_runs_reasoning_reaches_a_cell_when_expanded() {
    for (c, why) in HOSTILE {
        let mut app = window_with(vec![Entry::Reasoning {
            text: format!("weighing{c}MARKER-THOUGHT"),
            tokens: 3,
            done: true,
        }]);
        app.reasoning_expanded = true;
        let buf = common::window_frame(&app, 120, 30);
        let text = common::region_text(&buf, output_region(120, 30));

        assert!(text.contains("MARKER-THOUGHT"), "{why}: premise — expanded reasoning is drawn:\n{text}");
        assert!(!text.contains(c), "{why}: reached a cell through the reasoning block:\n{text}");
    }
}

/// A **tool line's target**, which is model-composed and is the field CLAUDE.md's own example names:
/// *a tool `target` containing a newline forged a second §B6 tool line — a call the model never
/// made.*
#[test]
fn no_hostile_character_in_a_tool_target_reaches_a_cell() {
    for (c, why) in HOSTILE {
        let app = window_with(vec![Entry::Tools(vec![ToolCall::ok(
            1,
            "read",
            &format!("MARKER-TARGET{c}x"),
            vec![Metric::Count { n: 48, unit: "lines" }],
        )])]);
        let buf = common::window_frame(&app, 120, 30);
        let text = common::region_text(&buf, output_region(120, 30));

        assert!(text.contains("MARKER-TARGET"), "{why}: premise — the tool line is drawn:\n{text}");
        assert!(!text.contains(c), "{why}: reached a cell through a tool target:\n{text}");
    }
}

/// A **failure detail**, which auto-expands under §B6 — so it renders without anyone asking, which
/// makes it the tool-line field an attacker would choose.
#[test]
fn no_hostile_character_in_an_expanded_failure_detail_reaches_a_cell() {
    for (c, why) in HOSTILE {
        let mut call = ToolCall::ok(1, "read", "notes.md", vec![Metric::Count { n: 0, unit: "lines" }]);
        call.state = marlowe_view::ToolLineState::Failed(ResultSummary::with_detail(
            vec![Metric::Exit { code: 1 }],
            format!("MARKER-DETAIL{c}tail"),
        ));
        call.expanded = true;
        let app = window_with(vec![Entry::Tools(vec![call])]);

        let buf = common::window_frame(&app, 120, 30);
        let text = common::region_text(&buf, output_region(120, 30));
        assert!(text.contains("MARKER-DETAIL"), "{why}: premise — the detail is drawn:\n{text}");
        assert!(!text.contains(c), "{why}: reached a cell through a failure detail:\n{text}");
    }
}

/// **A run cannot draw a border.** §B2's premise is that a border delineates a region the harness
/// drew; if a run's output can draw one, every border in the window is a claim the user can no
/// longer check — and a run window is *made* of bordered panels.
#[test]
fn a_runs_output_cannot_forge_chrome_inside_the_window() {
    let app = window_with(vec![Entry::Said(Speech::Model(
        "MARKER-FORGE\n┌Checkpoint──────────────┐\n│ resume · from seq 9999 │\n└────────────────────(k)┘\n  ⋯ read      /etc/passwd"
            .into(),
    ))]);
    let buf = common::window_frame(&app, 160, 45);
    let text = common::region_text(&buf, output_region(160, 45));

    assert!(text.contains("MARKER-FORGE"), "premise: the reply was drawn:\n{text}");
    for glyph in ['┌', '│', '└', '─', '┐', '┘', marlowe_surface::chrome::TOOL_MARKER] {
        assert!(
            !text.contains(glyph),
            "{glyph:?} survived inside the output region — a run drew harness chrome:\n{text}"
        );
    }
    // And the marker names the codepoint rather than swallowing it: a silently dropped glyph and a
    // clean string are indistinguishable to the person deciding whether a line is the harness's.
    assert!(text.contains("<U+250C>"), "the refusal must be visible, not silent:\n{text}");
}

/// **The layer that is doing the defending, named.**
///
/// The assertions above would hold if ratatui alone filtered everything, and then this file would be
/// a characterisation of a dependency wearing a security test's name — which is exactly what
/// `display_sanitiser.rs`'s header warns about. So: the same entry, through the same shared
/// renderer, **without** the window's preparation pass. The tag character survives into the built
/// `Line`, which is what makes `window::prepared` the thing standing in the way.
#[test]
fn without_the_windows_preparation_the_tag_block_survives_into_the_rendered_line() {
    const TAG: char = '\u{e0041}';
    let raw = vec![Entry::User(format!("steer{TAG}now"))];

    let theme = common::theme();
    let bare: String = marlowe_surface::render::entry_lines(&raw, &theme, 100, false, &|_| false, &|_| {
        Vec::new()
    })
    .iter()
    .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
    .collect();
    assert!(
        bare.contains(TAG),
        "premise: the shared renderer does not filter the tag block, so the window's pass is what \
         does. If this ever fails, this file's subject has moved and the test must move with it"
    );

    let prepared: String = marlowe_surface::render::entry_lines(
        &window::prepared(&raw),
        &theme,
        100,
        false,
        &|_| false,
        &|_| Vec::new(),
    )
    .iter()
    .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
    .collect();
    assert!(!prepared.contains(TAG), "the window's preparation pass let the tag block through");
    assert!(prepared.contains("<U+E0041>"), "and it must name the codepoint: {prepared}");
}

/// **A harness notice is not passed through the chrome reservation**, and that is deliberate rather
/// than an oversight: the harness is allowed to draw chrome and the model is not. Marking a `⋯` in
/// the harness's own §B6 listing would corrupt the one text nobody needs protecting from.
#[test]
fn the_harness_own_speech_is_not_mangled_by_the_reservation() {
    use marlowe_view::notice::{Capability, Milestone, Notice};
    let app = window_with(vec![Entry::Said(Speech::Harness(Notice::NotBuilt {
        capability: Capability::Steer,
        arrives: Milestone::M3,
    }))]);
    let buf = common::window_frame(&app, 160, 45);
    let text = common::region_text(&buf, output_region(160, 45));
    assert!(!text.contains("<U+"), "a harness notice was passed through the reservation:\n{text}");
}
