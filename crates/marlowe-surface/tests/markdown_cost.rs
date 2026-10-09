//! **What markdown costs per frame, against K4's 150 ms first-frame budget.**
//!
//! ADR-047 parses on every frame and caches nothing. That is not an oversight: §B13's flicker rows
//! are only checkable because every render is a pure function of `(state, now_ms)`, and a cache
//! keyed on anything that moves would make frame N+1 differ from a re-render of frame N without
//! anything reporting it. The scrollbar depends on the same purity — `render::transcript_lines` is
//! the only thing that knows how many wrapped lines a transcript produced, and a cached count and a
//! freshly wrapped one would be two answers to that question.
//!
//! So the cost has to be paid, which means it has to be **measured**. CLAUDE.md: *a target that is
//! not a command printing a number does not exist.*
//!
//! # This file reads a real clock, and that is why it is named in `determinism_guard.rs`
//!
//! `marlowe-surface` gets no clock exemption in `src/` and must not — the guard's own header says
//! so. This is a `tests/` file whose entire purpose is a wall-clock number, in the same category as
//! `marlowe-daemon/tests/socket_auth.rs`, and it is listed in `BENCHMARKS_AND_TIMING_TESTS` by
//! **name** rather than by directory, so the exemption cannot spread.
//!
//! # The control is the point of the second measurement
//!
//! A number for the markdown path alone answers *"is it fast"*, which is adjacent to the question.
//! The question is *"what did ADR-047 add"*, and that needs the flat path measured in the same
//! process, on the same bytes, at the same width. `Entry::User` still renders flat, so the two
//! paths sit side by side in one renderer and the difference is a difference rather than a
//! comparison across builds.

mod common;

use marlowe_surface::app::App;
use marlowe_surface::render;
use marlowe_view::notice::Speech;
use marlowe_view::Entry;
use std::time::Instant;

/// A paragraph's worth of realistic reply — headings, emphasis, a list, a fence, a link, maths.
const CHUNK: &str = "\
## What changed

The gate is a **build-time artifact** and the binary *refuses* to start without one. Three things
follow from that, and the third is the one that matters:

- a fresh clone reproduces the number rather than inheriting it
- `fit_gate.py` refuses without the split or the pre-registration
- the corpus is never vendored, and `fetch.py` pins its digest

```rust
let gate = include_str!(\"../artifacts/gate.json\");
```

Precision is $\\alpha \\times \\beta^2$ on held-out. See [the note](https://example.invalid/x).
";

/// **The ceilings, and the two different questions they answer.**
///
/// K4 is 150 ms to first frame for the *whole* startup path — process, config, theme resolution,
/// layout, the draw — and it is a property of the **shipped release binary**. A debug build of this
/// crate measures 7–8× slower on the identical input, so one number cannot serve both. Asserting
/// the release ceiling in a debug run would fail on a correct build; asserting the debug ceiling in
/// a release run would pass on a build that had regressed fourfold.
///
/// So there are two, and each is labelled with what it can actually establish:
///
/// * **release** — evidence about K4. A third of the whole first-frame budget for the pessimistic
///   transcript, against a measured 25–30 ms.
/// * **debug** — evidence about the *algorithm*, and nothing about K4. It exists so that the
///   workspace suite, which runs in debug, still fails if the quadratic that
///   `markdown.rs::scan_budget` closed ever comes back. Measured 227 ms; the ceiling is 500.
///
/// `cfg!(debug_assertions)` rather than a feature: it is the same switch that decides which
/// question is being asked, and it cannot be set wrong by a command line.
const fn heavy_ceiling_ms() -> u128 {
    if cfg!(debug_assertions) {
        500
    } else {
        50
    }
}

/// The typical case, which is the one K4 is really about — a fresh session's first frame does not
/// hold a full context window of scrollback.
const fn typical_ceiling_ms() -> u128 {
    if cfg!(debug_assertions) {
        60
    } else {
        8
    }
}

/// How much transcript a frame can be asked to lay out.
///
/// Compaction fires at 70% context (§6), so a session's uncompacted transcript is bounded by the
/// window. 400 KB is comfortably past that for any model in use — it is the pessimistic case, not
/// the typical one, and the typical one is printed alongside it.
const HEAVY_BYTES: usize = 400 * 1024;

fn transcript_of(bytes: usize, model: bool) -> App {
    let producer = marlowe_stub::Session::new();
    let mut view = producer.view().clone();
    view.transcript.clear();
    let mut n = 0;
    while n < bytes {
        n += CHUNK.len();
        view.transcript.push(if model {
            Entry::Said(Speech::Model(CHUNK.to_string()))
        } else {
            Entry::User(CHUNK.to_string())
        });
    }
    App::new(view).expect("the shipped key set has no conflicts")
}

fn time_layout(app: &App, width: u16, reps: u32) -> (u128, usize) {
    let theme = common::theme();
    // One untimed pass, so the measurement is of the work and not of the first touch of a page.
    let lines = render::transcript_lines(app, &theme, width).len();
    let start = Instant::now();
    for _ in 0..reps {
        std::hint::black_box(render::transcript_lines(app, &theme, width));
    }
    (start.elapsed().as_micros() / reps as u128, lines)
}

/// The measurement, at both ends of §B13's supported width range.
#[test]
fn laying_out_a_heavy_transcript_stays_inside_the_first_frame_budget() {
    // 120×30 and 240×60 are §B13's flicker range; the conversation is 58% of the width, less its
    // border and the scrollbar gutter.
    for (label, width) in [("120x30", 66u16), ("240x60", 136u16)] {
        let app = transcript_of(HEAVY_BYTES, true);
        let (markdown_us, md_lines) = time_layout(&app, width, 3);
        let flat_app = transcript_of(HEAVY_BYTES, false);
        let (flat_us, flat_lines) = time_layout(&flat_app, width, 3);

        println!(
            "{label} (pane {width} cols), {} KB of transcript:\n    \
             markdown {:.1} ms -> {md_lines} lines\n    \
             flat     {:.1} ms -> {flat_lines} lines\n    \
             ADR-047 adds {:.1} ms ({:.2}x)",
            HEAVY_BYTES / 1024,
            markdown_us as f64 / 1000.0,
            flat_us as f64 / 1000.0,
            (markdown_us.saturating_sub(flat_us)) as f64 / 1000.0,
            markdown_us as f64 / flat_us.max(1) as f64,
        );

        assert!(
            md_lines > 1000,
            "{label}: only {md_lines} lines laid out; the transcript did not build, so this \
             measures nothing"
        );
        assert!(
            markdown_us / 1000 < heavy_ceiling_ms(),
            "{label}: laying out {} KB of markdown took {:.1} ms, over the {} ms ceiling. Either \
             the parser regressed or the ceiling needs an argument — do not raise it without one, \
             and note that in a debug build this number says nothing about K4",
            HEAVY_BYTES / 1024,
            markdown_us as f64 / 1000.0,
            heavy_ceiling_ms()
        );
    }
}

/// The case that actually decides whether the product feels fast: a first frame with a normal
/// session's worth of conversation in it.
#[test]
fn a_typical_transcript_lays_out_in_under_a_millisecond() {
    let app = transcript_of(32 * 1024, true);
    let (us, lines) = time_layout(&app, 66, 20);
    println!(
        "32 KB of markdown at 66 columns: {:.3} ms -> {lines} lines (ceiling {} ms, {})",
        us as f64 / 1000.0,
        typical_ceiling_ms(),
        if cfg!(debug_assertions) { "debug" } else { "release" }
    );
    assert!(lines > 100, "only {lines} lines; the transcript did not build");
    assert!(
        us / 1000 < typical_ceiling_ms(),
        "a typical transcript took {:.1} ms to lay out. This is the case K4's first-frame budget \
         is actually about — a fresh session does not open holding a full context window",
        us as f64 / 1000.0
    );
}

/// A reply built to be expensive to parse must not be able to cost more than one that is merely
/// long. Nesting is the lever — an inline parser that recursed without a bound would be
/// quadratic-or-worse on a line of stars.
#[test]
fn a_pathological_reply_costs_no_more_than_a_long_one() {
    let producer = marlowe_stub::Session::new();
    let mut view = producer.view().clone();
    view.transcript.clear();
    for hostile in [
        "*".repeat(20_000),
        "`".repeat(20_000),
        "[".repeat(20_000),
        "***nested ".repeat(2_000),
        format!("{}{}", "> ".repeat(200), "quote"),
        "$".repeat(20_000),
        r"\frac{".repeat(5_000),
    ] {
        view.transcript.push(Entry::Said(Speech::Model(hostile)));
    }
    let app = App::new(view).expect("the shipped key set has no conflicts");
    let (us, lines) = time_layout(&app, 66, 1);
    println!(
        "140 KB of pathological markup: {:.1} ms -> {lines} lines (ceiling {} ms)",
        us as f64 / 1000.0,
        heavy_ceiling_ms()
    );
    assert!(lines > 100, "only {lines} lines; the hostile transcript did not build");
    assert!(
        us / 1000 < heavy_ceiling_ms(),
        "a reply designed to be expensive took {:.1} ms — a model can stall the surface by \
         choosing its punctuation. This is the measurement `scan_budget` exists for: before it, \
         this same input took 144 ms in RELEASE, past K4's entire first-frame budget",
        us as f64 / 1000.0
    );
}
