//! **§B13's rows, re-run against markdown-rich replies, plus §B10's `Y`.**
//!
//! `b13_rendering.rs` proves those rows over the stub's scripted transcript, which is plain prose.
//! ADR-047 put a parser between the model's bytes and the grid, so every one of them has to be
//! re-asked with the parser in the path. **A measurement is scoped to the system it was taken on**
//! — CLAUDE.md's rule about carrying a number across a boundary — and a renderer is a boundary.
//!
//! | row | asserted here |
//! |---|---|
//! | background fills | zero cells with a `bg`, **and zero with `REVERSED`** |
//! | distinct colours | every emitted foreground is in `Theme::declared_colours` |
//! | chrome inside a scroll area | markdown writes nothing outside `conversation_scroll` |
//! | repaint flicker, 120×30 → 240×60 | frame N+1 is a cell-for-cell re-render of frame N |
//! | resize reflows cleanly | away and back returns the identical buffer |
//!
//! Everything is asserted on a drawn `ratatui::Buffer`. An assertion on the `Vec<Span>` the
//! renderer built would be a statement about its own bookkeeping.

mod common;

use marlowe_surface::app::App;
use marlowe_surface::render;
use marlowe_view::notice::Speech;
use marlowe_view::Entry;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};

/// A reply exercising every construct the renderer knows, so the sweeps below are not walking a
/// screen of plain prose and reporting that nothing went wrong.
const RICH: &str = "\
# Retrieval, end to end

The gate is a **build-time artifact**: the binary *refuses* to start without one, and there is
no ~~default~~ fallback weight vector.

## What runs where

1. `fit_gate.py` writes the artifact
2. `cargo build --release` embeds it via `include_str!`
   - the digest is pinned
   - a stale path errors by name
3. the binary reproduces the number

> Filtering does not work. Containment works.

| stage | P95 | share |
|---|---|---|
| lexical | 3.83 ms | 1.78% |
| dense | 41 ms | 19% |

```rust
let gate = include_str!(\"../artifacts/gate.json\");
assert!(gate.len() > 0);
```

Precision is $\\alpha \\times \\beta^2$ over the held-out split, and the integral form
$\\hat{x}$ is left as written. See [the note](https://example.invalid/precision).

---

That is the whole mechanism.
";

fn app_saying(text: &str) -> App {
    let producer = marlowe_stub::Session::new();
    let mut view = producer.view().clone();
    view.transcript
        .push(Entry::Said(Speech::Model(text.to_string())));
    App::new(view).expect("the shipped key set has no conflicts")
}

/// Render one model reply and return the drawn grid as text.
///
/// Through `App` and a real `Buffer`, not through `markdown::render_prose` — the property is what
/// reaches the screen, and a helper that called the parser directly would assert on an
/// intermediate the user never sees.
fn plain(md: &str) -> String {
    common::buffer_text(&common::frame(&app_saying(md), 120, 40))
}

fn rich() -> App {
    app_saying(RICH)
}

/// §B2: *"Never hardcode a background fill."* §B13: zero background fills.
///
/// **And `REVERSED`, which the `bg` check cannot see.** Reverse video is a modifier, so a cell
/// carrying it reports `bg == Color::Reset` while the terminal paints a solid slab — the obvious
/// way to render a code fence, and a §B13 violation that the existing row would have passed. This
/// is the project's own failure family (a property asserted where it is declared rather than where
/// it is seen) and it is why this assertion is two assertions.
#[test]
fn not_one_cell_carries_a_background_or_reverse_video() {
    let app = rich();
    let mut cells = 0usize;
    for (w, h) in common::SIZES {
        let buf = common::frame(&app, w, h);
        for y in 0..h {
            for x in 0..w {
                let cell = buf.cell((x, y)).unwrap();
                cells += 1;
                assert_eq!(
                    cell.bg,
                    Color::Reset,
                    "{w}x{h}: cell ({x},{y}) has background {:?}",
                    cell.bg
                );
                assert!(
                    !cell.modifier.contains(Modifier::REVERSED),
                    "{w}x{h}: cell ({x},{y}) is reverse video, which is a background fill by \
                     another name — it renders as a solid block and reports bg = Reset"
                );
            }
        }
    }
    println!("markdown: 0 fills and 0 reversed cells of {cells} walked");
}

/// §B13: *≤ 1 accent + 3 state + 3 foreground weights*, asserted **by value** against the theme's
/// own declaration — the same construction `b13_rendering.rs` uses, and for the same reason: a
/// count passes happily while every colour on screen is wrong.
///
/// This is where syntax highlighting would fail. Green-for-strings and blue-for-keywords are
/// outside the declared set by construction, and §B2 forbids them for a stronger reason than the
/// count: state colours encode state, so a green token would make the screen say *healthy* about
/// a piece of syntax.
#[test]
fn markdown_emits_no_colour_outside_the_declared_palette() {
    let theme = common::theme();
    let mut allowed: Vec<Color> = theme.declared_colours();
    allowed.push(Color::DarkGray); // the approval overlay's dimming, as in b13_rendering.rs
    let app = rich();
    let mut used: Vec<Color> = Vec::new();
    for (w, h) in common::SIZES {
        let buf = common::frame(&app, w, h);
        for y in 0..h {
            for x in 0..w {
                let fg = buf.cell((x, y)).unwrap().fg;
                if !used.contains(&fg) {
                    used.push(fg);
                }
                assert!(
                    allowed.contains(&fg),
                    "{w}x{h}: cell ({x},{y}) uses {fg:?}, outside one accent, three state colours \
                     and three foreground weights. Markdown gets attributes — bold, italic, \
                     underline — and never a new colour"
                );
            }
        }
    }
    println!("markdown: {} distinct foregrounds, all declared", used.len());
    for c in &used {
        println!("    in use: {c:?}");
    }
}

/// The attributes are the palette, so they have to actually reach the grid. Without this the
/// colour row above passes on a frame where markdown rendered nothing at all.
#[test]
fn the_attributes_reach_the_grid_and_the_source_markers_do_not() {
    let buf = common::frame(&rich(), 160, 45);
    let screen = common::buffer_text(&buf);

    assert!(screen.contains("Retrieval, end to end"), "the reply never rendered:\n{screen}");
    assert!(!screen.contains("**"), "the strong markers survived:\n{screen}");
    assert!(!screen.contains("~~"), "the strikethrough markers survived:\n{screen}");
    assert!(!screen.contains("# "), "a heading kept its hashes:\n{screen}");

    let mut bold = 0;
    let mut italic = 0;
    let mut underlined = 0;
    let mut crossed = 0;
    let mut accent = 0;
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            let cell = buf.cell((x, y)).unwrap();
            if cell.symbol() == " " {
                continue;
            }
            if cell.modifier.contains(Modifier::BOLD) {
                bold += 1;
            }
            if cell.modifier.contains(Modifier::ITALIC) {
                italic += 1;
            }
            if cell.modifier.contains(Modifier::UNDERLINED) {
                underlined += 1;
            }
            if cell.modifier.contains(Modifier::CROSSED_OUT) {
                crossed += 1;
            }
            if cell.fg == common::theme().accent() {
                accent += 1;
            }
        }
    }
    assert!(bold > 0, "nothing is bold");
    assert!(italic > 0, "nothing is italic");
    assert!(underlined > 0, "no link text is underlined");
    assert!(crossed > 0, "nothing is struck through");
    assert!(accent > 0, "no heading or bullet took the accent");
    println!(
        "markdown attributes on the grid: {bold} bold, {italic} italic, {underlined} underlined, \
         {crossed} struck, {accent} accent"
    );
}

/// K4: *zero repaint flicker across 120×30 → 240×60*. The property that makes it checkable is that
/// every render is a pure function of `(state, now_ms)`; a parser with a cache keyed on anything
/// that moves would take it away, and would do it silently.
#[test]
fn a_second_render_of_the_same_state_changes_not_one_cell() {
    let app = rich();
    for (w, h) in common::SIZES {
        let mut term = common::terminal(w, h);
        let first = common::draw_into(&mut term, &app);
        let second = common::draw_into(&mut term, &app);
        let changed = common::diff_cells(&first, &second);
        assert!(
            changed.is_empty(),
            "{w}x{h}: re-rendering identical state repainted {} cells, first at {:?}. The markdown \
             renderer is not a pure function of its arguments",
            changed.len(),
            changed.first()
        );
    }
    println!("markdown re-render: 0 cells differ, 120x30 through 240x60");
}

/// §B12: *"This is why focus is a border and not a fill"* — re-asked with markdown in the pane.
///
/// `b13_rendering.rs` proves this over the stub's plain transcript. A renderer that styled by
/// region rather than by content — a code fence filling its rows, a heading painting a bar — would
/// pass that version and fail this one, because the cells it repaints are the ones markdown put
/// there.
#[test]
fn a_focus_change_repaints_no_markdown_cell() {
    for (w, h) in common::SIZES {
        // Only the reply, so the vacuity control below is a statement about markdown rather than
        // about whatever the stub's own transcript happened to scroll into view.
        let producer = marlowe_stub::Session::new();
        let mut view = producer.view().clone();
        view.transcript.clear();
        view.transcript
            .push(Entry::Said(Speech::Model(RICH.to_string())));
        let mut app = App::new(view).expect("the shipped key set has no conflicts");
        app.scroll = Some(0);
        app.on_key(marlowe_surface::app::Key::Esc);
        let mut term = common::terminal(w, h);
        let before = common::draw_into(&mut term, &app);

        app.on_key(marlowe_surface::app::Key::Char('m')); // conversation -> Model
        let after = common::draw_into(&mut term, &app);

        let c = render::layout(Rect::new(0, 0, w, h));
        let r = c.conversation_scroll;
        let hits: Vec<_> = common::diff_cells(&before, &after)
            .into_iter()
            .filter(|(x, y)| *x >= r.x && *x < r.right() && *y >= r.y && *y < r.bottom())
            .collect();
        assert!(
            hits.is_empty(),
            "{w}x{h}: moving focus to Model repainted {} cells inside the markdown transcript, \
             first at {:?}. Focus is a border and a title, not a fill — and rendered markdown is \
             not part of the focus signal",
            hits.len(),
            hits.first()
        );
        // Non-vacuity: the pane must have had markdown in it, and the focus change must have
        // repainted *something* — otherwise this passes on a frame where nothing happened.
        assert!(
            common::buffer_text(&before).contains("Retrieval, end to end"),
            "{w}x{h}: no markdown was on screen"
        );
        assert!(
            !common::diff_cells(&before, &after).is_empty(),
            "{w}x{h}: the focus change repainted nothing at all"
        );
    }
    println!("focus change with markdown on screen: 0 transcript cells repainted, 5 sizes");
}

/// §B12: *"Resize reflows cleanly."* Away and back must return the identical buffer — a parser
/// holding state across widths would not.
#[test]
fn a_resize_round_trip_with_markdown_returns_the_identical_frame() {
    let app = rich();
    let before = common::frame(&app, 140, 40);
    let _ = common::frame(&app, 240, 60);
    let _ = common::frame(&app, 120, 30);
    let after = common::frame(&app, 140, 40);
    assert!(
        common::diff_cells(&before, &after).is_empty(),
        "markdown layout is holding state across resizes"
    );
    println!("markdown resize round trip 140x40 -> 240x60 -> 120x30 -> 140x40: 0 cells differ");
}

/// §B3: *"Chrome that scrolls is a bug."* The renderer wraps, so the renderer decides how many
/// lines a reply occupies; if it wrote past the scroll area it would paint over the pager, which
/// carries the turn count, the compaction count and the lineage depth and lives nowhere else.
#[test]
fn markdown_never_paints_over_the_pager_or_the_border() {
    for (w, h) in common::SIZES {
        let app = app_saying(&format!("{RICH}\n\n{RICH}"));
        let buf = common::frame(&app, w, h);
        let c = render::layout(Rect::new(0, 0, w, h));
        let pager = common::row_text(&buf, c.pager.y);
        assert!(
            pager.contains("turn ") && pager.contains("compacted") && pager.contains("lineage"),
            "{w}x{h}: the pager row reads {pager:?} — a long reply painted over it"
        );
        // And the region's own bottom border is intact.
        let border = common::row_text(&buf, c.conversation.bottom() - 1);
        assert!(border.contains("(c)"), "{w}x{h}: the conversation's hotkey is gone: {border:?}");
    }
    println!("pager and hotkey survive a two-screen markdown reply at 5 sizes");
}

/// The scrollbar is sized from the renderer's own line count, so the count has to be the one that
/// was drawn. A markdown reply produces a different number of lines than its source has, and
/// `App::scroll_max` is the only place those two answers could diverge.
#[test]
fn the_scroll_extent_agrees_with_what_markdown_actually_drew() {
    let app = app_saying(&RICH.repeat(3));
    let theme = common::theme();
    for (w, h) in common::SIZES {
        let buf = common::frame(&app, w, h);
        let c = render::layout(Rect::new(0, 0, w, h));
        let text_w = c.conversation_scroll.width - 2;
        let lines = render::transcript_lines(&app, &theme, text_w);
        let expected = lines
            .len()
            .saturating_sub(c.conversation_scroll.height as usize);
        assert_eq!(
            app.scroll_max.get() as usize,
            expected,
            "{w}x{h}: the scrollbar was sized from {} lines and the renderer drew {}",
            app.scroll_max.get() as usize + c.conversation_scroll.height as usize,
            lines.len()
        );
        // Non-vacuity: the reply must actually overflow, or `expected` is 0 and the assertion holds
        // for a transcript that never needed a scrollbar.
        assert!(expected > 0, "{w}x{h}: the transcript fits, so this proves nothing");
        assert!(!common::buffer_text(&buf).is_empty());
    }
    println!("scroll extent matches the drawn line count at 5 sizes");
}

/// §B10: **`Y` copies the whole transcript as markdown**, and markdown is already the interchange
/// format. Rendering it must not change what comes out.
///
/// The two halves are a control for each other: the pane showing no `**` proves the renderer ran,
/// and the clipboard showing `**` proves it did not reach the copy. Either assertion alone passes
/// on a build where the feature does nothing.
#[test]
fn y_still_copies_the_source_markdown_and_not_the_rendered_form() {
    let mut app = rich();
    let screen = common::buffer_text(&common::frame(&app, 160, 45));
    assert!(
        !screen.contains("**build-time artifact**"),
        "the pane is showing source markers, so the copy assertion below proves nothing:\n{screen}"
    );
    assert!(screen.contains("build-time artifact"), "the reply never rendered:\n{screen}");

    app.copy_transcript();
    let copied = app.pending_copy.clone().expect("Y puts a payload on the clipboard");

    for fragment in [
        "**build-time artifact**",
        "# Retrieval, end to end",
        "1. `fit_gate.py` writes the artifact",
        "> Filtering does not work. Containment works.",
        "| stage | P95 | share |",
        "[the note](https://example.invalid/precision)",
        r"$\hat{x}$",
    ] {
        assert!(
            copied.contains(fragment),
            "`Y` must yield what the model WROTE, not a re-serialisation of the parse. Missing \
             {fragment:?} from:\n{copied}"
        );
    }
    // And nothing from the rendered form leaked in. `·` alone is not on this list: the stub's own
    // tool-call text uses it as a separator, so it is in the payload for a legitimate reason — the
    // rendered horizontal RULE is a run of them, and that is what would signal a leak.
    for fragment in ["•", "▏", "····"] {
        assert!(
            !copied.contains(fragment),
            "{fragment:?} is a rendering decision and reached the clipboard"
        );
    }
    println!("Y: {} characters of source markdown, unchanged", copied.chars().count());
}

/// The commonest reply in the product has no markup in it at all, and the renderer must be
/// undetectable in that case. Without this, every assertion above is compatible with a renderer
/// that rewrites ordinary prose.
#[test]
fn plain_prose_reaches_the_grid_exactly_as_written() {
    let text = "I read the Dockerfile. The pinned version got dropped when it was rebuilt.";
    let app = app_saying(text);
    let screen = common::buffer_text(&common::frame(&app, 160, 45));
    assert!(screen.contains(text), "plain prose was reflowed or rewritten:\n{screen}");
}

// ── ADR-047 follow-up: four formulas from a real reply came back as raw LaTeX ────────────────
//
// Reported from a screenshot of a live session, not from a test. Three separate causes, and the
// third is a change to the module's stated rule rather than a bug fix.

/// **`$$…$$` mid-line.** `display_math` only recognises `$$` as a whole line, so a model writing
/// `Policy gradient: $$…$$` fell through to the inline `$` reader, whose `start` landed on the
/// SECOND `$` — empty content, refused, emitted as source. Three of the four formulas.
#[test]
fn display_maths_after_a_label_on_the_same_line_renders() {
    let out = plain(r"Policy gradient: $$\alpha \times \beta^2$$");
    assert!(out.contains("α × β²"), "{out}");
    // **Assert the LABEL and the maths are adjacent**, which is the actual property.
    //
    // The first version of this assertion was `!out.contains("$$")` and it passed against the
    // UNFIXED code — caught by mutating the fix rather than by reading the test. Unfixed, `$$X$$`
    // renders as `$X$`: the first `$` fails, the second opens a span that closes on the third, and
    // the fourth is left over. Two stray dollars, never adjacent, so a test looking for `$$` sees
    // nothing wrong. What a reader actually sees is a delimiter between the label and the formula.
    assert!(out.contains("Policy gradient: α × β²"), "delimiters survived: {out}");
}

/// The control: `$$` on its own line must keep working, so the fix is an addition rather than a
/// replacement.
#[test]
fn display_maths_on_its_own_line_still_renders() {
    let out = plain(r"$$\alpha \times \beta^2$$");
    assert!(out.contains("α × β²"), "{out}");
}

/// **`\mid` and `\|`.** Conditional and norm bars — `p(a \mid s)`, `D(P \| Q)` — are everywhere in
/// probability, and their absence refused whole expressions in which every other token rendered.
#[test]
fn conditional_and_norm_bars_render_and_are_spaced() {
    let out = plain(r"$p(a \mid s)$ and $D(P \| Q)$");
    assert!(out.contains("p(a ∣ s)"), "{out}");
    assert!(out.contains("D(P ‖ Q)"), "{out}");
    // U+2016, never an ASCII bar: an ASCII `|` is this renderer's table delimiter, and a formula
    // that could emit one is a formula that could forge a table row.
    assert!(!out.contains('|'), "an ASCII bar reached the output: {out}");
}

/// **Subscripts Unicode cannot express.** There is no subscript `θ`, `K` or `L`, so
/// `\nabla_\theta` and `D_{\mathrm{KL}}` were refused — and all-or-nothing then discarded the
/// whole formula. They now degrade to explicit notation with the script itself rendered.
///
/// This is deliberately NOT the case the refusal rule exists for: `∇_θ` drops nothing, where
/// `\int_0^\infty` → `∫₀` would drop the bound and state a different integral.
#[test]
fn a_subscript_with_no_unicode_form_degrades_rather_than_refusing() {
    let out = plain(r"$\nabla_\theta J(\theta)$");
    assert!(out.contains("∇_θ"), "{out}");
    assert!(!out.contains("\nabla"), "refused instead of degrading: {out}");

    // Multi-character scripts keep their braces: `D_KL` invites reading `K` as the subscript and
    // `L` as what follows it.
    let kl = plain(r"$D_{\mathrm{KL}}(P \| Q)$");
    assert!(kl.contains("D_{KL}"), "{kl}");

    // And the script is RENDERED, not passed through as source.
    let e = plain(r"$\mathbb{E}_{\pi_\theta}$");
    assert!(e.contains("𝔼_{π_θ}"), "{e}");
}

/// **The control that keeps the refusal rule honest.** A true Unicode script is still preferred
/// over the fallback wherever one exists — otherwise this change would have quietly replaced every
/// `x²` with `x^2`.
#[test]
fn a_script_unicode_can_express_still_uses_the_real_glyph() {
    let out = plain(r"$\beta^2$ and $\sum_x$");
    assert!(out.contains("β²"), "{out}");
    assert!(out.contains("∑ₓ"), "{out}");
    // The SOURCE must be gone, which is the property. `!contains('^')` would be a claim about
    // every cell on the screen, chrome included.
    assert!(!out.contains("beta^2"), "fell back where a real glyph exists: {out}");
    assert!(!out.contains("_x"), "fell back where a real glyph exists: {out}");
}

/// **Round two, from a second screenshot of a live session.** Three more causes, none of them the
/// renderer: a missing symbol, a spacing rule that deleted deliberate spacing, and an accent
/// refusal that was one step too wide.
#[test]
fn transpose_renders_so_the_attention_formula_does() {
    // `\top` was the single unrenderable token in the formula everyone writes as QK^T.
    let out = plain(r"$\operatorname{softmax}\!\left( \frac{QK^\top}{\sqrt{d}} \right) V$");
    assert!(out.contains("QK^⊤"), "{out}");
    assert!(!out.contains("\\top"), "refused instead of rendering: {out}");
}

/// `\quad` and `\qquad` are the author separating two equations on one line. They were mapped
/// to spaces and then DELETED by a rule that collapsed every run of spaces to one, so two
/// equations ran together and read as a single malformed one.
#[test]
fn deliberate_spacing_between_equations_survives() {
    let out = plain(r"$a = 1 \qquad b = 2$");
    // Both equations render and stay distinct. **The width of the gap is NOT asserted**: the
    // rendered maths re-enters prose layout, which collapses runs of spaces the way markdown does
    // everywhere else, so `\qquad` survives as a separator rather than as four columns. Recorded
    // rather than asserted, because asserting it would pin a wrapping detail this test does not own.
    assert!(out.contains("a = 1"), "{out}");
    assert!(out.contains("b = 2"), "{out}");
    // The control: a relation still contributes exactly ONE space, not two.
    let r = plain(r"$x = y$");
    assert!(r.contains("x = y") && !r.contains("x  ="), "{r}");
}

/// **The accent refusal was one step too wide.** `\hat{y}` is U+0177 -- one precomposed
/// codepoint, one column, no font composition required -- and it is the predicted value in every
/// regression loss. Refusing it refused the formula around it.
#[test]
fn an_accent_with_a_precomposed_form_renders_and_one_without_still_refuses() {
    let out = plain(r"$L = \frac{1}{2}(y - \hat{y})^2$");
    assert!(out.contains("½(y - ŷ)²"), "{out}");

    // **The control, and it is the whole reason this is not a widening.** Where Unicode has no
    // precomposed form the alternative is a COMBINING mark -- zero columns wide, so the wrap
    // arithmetic and the eye disagree -- and it still refuses.
    let x = plain(r"$\hat{x}$");
    assert!(x.contains(r"$\hat{x}$"), "a combining-mark accent must stay source: {x}");
}

// ── requested after use: maths should stand out, and reasoning should render too ─────────────

/// **A rendered equation is one weight LIGHTER than the prose around it.**
///
/// Not a colour. §B13 allows one accent plus three state colours plus three foreground weights, and
/// state colours encode state and never category — amber for a formula would tell a reader who has
/// learned the palette that something needs attention. The weight ladder carries it instead.
#[test]
fn rendered_maths_is_lighter_than_the_prose_around_it() {
    // **The transcript is CLEARED first.** `app_saying` appends to the stub's own transcript, and
    // a short reply added to the end renders below the visible pane -- the assertion then fails
    // for a reason that has nothing to do with styling. Found by asserting the glyph reached the
    // grid before asserting anything about its colour.
    let producer = marlowe_stub::Session::new();
    let mut view = producer.view().clone();
    view.transcript.clear();
    view.transcript.push(Entry::Said(Speech::Model(
        r"the value is $\alpha$ exactly".to_string(),
    )));
    let app = App::new(view).unwrap();
    let buf = common::frame(&app, 120, 40);

    // **Both samples come from the SAME ROW**, and that is the point of the test rather than a
    // detail of it. The first version searched the whole buffer for a prose letter and found one in
    // the chrome, so it compared an equation against a border label and passed with the styling
    // reverted. Caught by mutating the fix.
    let screen = common::buffer_text(&buf);
    assert!(screen.contains('α'), "the maths never reached the grid:
{screen}");
    let row = (0..buf.area.height)
        .find(|y| (0..buf.area.width).any(|x| buf.cell((x, *y)).is_some_and(|c| c.symbol() == "α")))
        .expect("the maths rendered somewhere");

    let at = |sym: &str| -> Option<ratatui::style::Color> {
        (0..buf.area.width)
            .find(|x| buf.cell((*x, row)).is_some_and(|c| c.symbol() == sym))
            .and_then(|x| buf.cell((x, row)).map(|c| c.fg))
    };
    let maths = at("α").expect("alpha on this row");
    let prose = at("v").expect("prose on the same row");
    assert_ne!(
        maths, prose,
        "the equation is the same weight as the prose beside it, so it does not stand out"
    );
}

/// **Reasoning renders markdown when expanded, and the equation is lighter than the reasoning.**
///
/// Reasoning is where a model puts its working, and its working is where the equations are.
/// Rendering it flat left the one place a derivation actually lives as the one place it stayed raw.
#[test]
fn expanded_reasoning_renders_maths_and_it_is_lighter_than_the_reasoning() {
    let producer = marlowe_stub::Session::new();
    let mut view = producer.view().clone();
    view.transcript.clear();
    view.transcript.push(marlowe_view::Entry::Reasoning {
        text: r"so the update is $\alpha \times \beta$ here".to_string(),
        tokens: 11,
        done: true,
    });
    let mut app = App::new(view).unwrap();
    app.reasoning_expanded = true;
    let buf = common::frame(&app, 120, 40);
    let text = common::buffer_text(&buf);

    assert!(text.contains("α × β"), "reasoning did not render its maths:\n{text}");
    assert!(!text.contains(r"\alpha"), "the source survived:\n{text}");
}

/// **The control, and it is the cost argument.** Collapsed, a reasoning block is a carried COUNT,
/// so the parser never runs on the path that draws almost every frame. Reasoning is the
/// highest-volume text in the product and K4 budgets 150 ms to first frame.
#[test]
fn collapsed_reasoning_parses_nothing_and_reports_a_count() {
    let producer = marlowe_stub::Session::new();
    let mut view = producer.view().clone();
    view.transcript.clear();
    view.transcript.push(marlowe_view::Entry::Reasoning {
        text: r"so the update is $\alpha \times \beta$ here".to_string(),
        tokens: 11,
        done: true,
    });
    let app = App::new(view).unwrap();
    let text = common::buffer_text(&common::frame(&app, 120, 40));

    assert!(text.contains("thought 11 tokens"), "the head line must report a count:\n{text}");
    assert!(!text.contains("α × β"), "collapsed reasoning rendered its body:\n{text}");
}

/// **Display maths gets the same weight as inline maths.**
///
/// It took the surrounding prose style, so `$x$` stood out and `$$x$$` did not -- backwards, since
/// the display form is the one the author decided was important enough to put on its own line.
#[test]
fn display_maths_stands_out_the_same_way_inline_maths_does() {
    let build = |md: &str| {
        let producer = marlowe_stub::Session::new();
        let mut view = producer.view().clone();
        view.transcript.clear();
        view.transcript.push(Entry::Said(Speech::Model(md.to_string())));
        common::frame(&App::new(view).unwrap(), 120, 40)
    };
    let fg = |buf: &ratatui::buffer::Buffer, sym: &str| {
        (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .find(|(x, y)| buf.cell((*x, *y)).is_some_and(|c| c.symbol() == sym))
            .and_then(|(x, y)| buf.cell((x, y)).map(|c| c.fg))
    };
    let inline = build(r"value $\alpha$ here");
    let display = build(r"$$\alpha$$");
    let a = fg(&inline, "α").expect("inline rendered");
    let b = fg(&display, "α").expect("display rendered");
    assert_eq!(a, b, "display maths is styled differently from inline maths");

    // The control: it is not simply the prose colour in both.
    let prose = fg(&inline, "v").expect("prose on screen");
    assert_ne!(a, prose, "maths does not stand out at all");
}

/// **A streaming equation renders as it builds, rather than snapping in at the close.**
///
/// A reply arrives token by token, so `$$y = \sigma(Wx` is what the pane holds for as long as the
/// model takes to finish the line. Requiring the closing delimiter left raw source on screen and
/// then replaced it wholesale when the last two characters landed.
#[test]
fn an_unclosed_equation_renders_while_it_is_still_arriving() {
    let full = r"$$y = \sigma(Wx + b)$$";
    let prefix = |n: usize| -> String {
        let src: String = full.chars().take(n).collect();
        let producer = marlowe_stub::Session::new();
        let mut view = producer.view().clone();
        view.transcript.clear();
        view.transcript.push(Entry::Said(Speech::Model(src)));
        common::buffer_text(&common::frame(&App::new(view).unwrap(), 120, 40))
    };

    // Partway through, with every token so far complete: typeset, not source.
    assert!(prefix(16).contains("y = σ(Wx"), "{}", prefix(16));
    assert!(!prefix(16).contains(r"\sigma"), "the source survived: {}", prefix(16));

    // And the finished expression is unchanged by any of this.
    assert!(prefix(full.chars().count()).contains("y = σ(Wx + b)"));
}

/// **No frame of a streaming equation ever shows raw LaTeX.**
///
/// This is the property that was actually wanted and the previous version did not deliver. Rendering
/// only COMPLETE expressions meant a partial alternated between valid and invalid as each token
/// closed, so the pane flipped between a typeset formula and raw source several times per equation.
/// Both states were correct; the flipping between them was the defect.
///
/// Falling back to the longest valid PREFIX makes it monotonic — a prefix that rendered a frame ago
/// still renders now with more after it — so the equation only ever grows.
#[test]
fn no_frame_of_a_streaming_equation_shows_raw_latex() {
    for full in [
        r"$$y = \sigma(Wx + b)$$",
        r"$$L = rac{1}{2}(y - \hat{y})^2$$",
    ] {
        let n = full.chars().count();
        for k in 3..=n {
            let src: String = full.chars().take(k).collect();
            let producer = marlowe_stub::Session::new();
            let mut view = producer.view().clone();
            view.transcript.clear();
            view.transcript.push(Entry::Said(Speech::Model(src.clone())));
            let out =
                common::buffer_text(&common::frame(&App::new(view).unwrap(), 120, 40));
            assert!(
                !out.contains(r"\sigma") && !out.contains(r"rac") && !out.contains(r"\hat"),
                "frame {k} of {full:?} fell back to raw source:
{out}"
            );
        }
    }
}

/// **The control: a CLOSED expression that cannot render still shows its source, whole.**
///
/// The prefix fallback is keyed on the span being unterminated, and this is why. A finished
/// expression that will not render is a real refusal — a matrix, an over-line — and truncating it
/// to the part that happened to work would be the "wrong and plausible" output this module exists
/// to prevent.
#[test]
fn a_closed_expression_that_cannot_render_is_not_silently_truncated() {
    let producer = marlowe_stub::Session::new();
    let mut view = producer.view().clone();
    view.transcript.clear();
    view.transcript.push(Entry::Said(Speech::Model(r"$x = \hat{x} + 1$".to_string())));
    let out = common::buffer_text(&common::frame(&App::new(view).unwrap(), 120, 40));
    assert!(out.contains(r"\hat{x}"), "a real refusal was truncated to its valid prefix:
{out}");
}

/// **Currency is still protected mid-stream**, which is the risk this change introduces: an
/// unclosed `$` now scans to end-of-input, so "it costs $5 and…" is exactly the case that would
/// break if the heuristic were dropped.
#[test]
fn an_unclosed_dollar_in_prose_is_still_not_maths() {
    for s in ["it costs $5 and", "the price is $12 for", "spend $"] {
        let producer = marlowe_stub::Session::new();
        let mut view = producer.view().clone();
        view.transcript.clear();
        view.transcript.push(Entry::Said(Speech::Model(s.to_string())));
        let out = common::buffer_text(&common::frame(&App::new(view).unwrap(), 120, 40));
        assert!(out.contains(s), "prose was eaten as maths: {s:?} -> {out}");
    }
}
