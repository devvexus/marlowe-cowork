//! **Maths, asserted on the drawn grid: coverage, never-raw, streaming stability, and cost.**
//!
//! Matthew's requirement, verbatim: *"Full LaTeX rendering in the terminal is a must. And done
//! smoothly. Not jittery. LaTeX renders only when it parses, never renders raw."* Each test here is
//! one of those sentences turned into a cell-level assertion.
//!
//! This file reads a wall clock in one test (`a_formula_typesets_in_well_under_a_frame`), the same
//! category as `markdown_cost.rs`.

use marlowe_surface::chrome::Ink;
use marlowe_surface::{markdown, math, Theme};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

fn theme() -> Theme {
    Theme::default_truecolor()
}

fn lines(text: &str, live: bool, width: usize) -> Vec<Line<'static>> {
    let t = theme();
    let base = Ink::Speech.style(&t);
    if live {
        markdown::render_prose_live(text, width, &t, base)
    } else {
        markdown::render_prose(text, width, &t, base)
    }
}

fn texts(ls: &[Line<'_>]) -> Vec<String> {
    ls.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()).collect()
}

/// Draw into a real `Buffer`: the property is what reaches the screen.
fn frame(text: &str, live: bool, w: u16, h: u16) -> Buffer {
    let area = Rect::new(0, 0, w, h);
    let mut buf = Buffer::empty(area);
    Paragraph::new(lines(text, live, w as usize)).render(area, &mut buf);
    buf
}

fn screen(buf: &Buffer) -> String {
    let a = buf.area;
    (0..a.height)
        .map(|y| (0..a.width).map(|x| buf.cell((x, y)).map_or(" ", |c| c.symbol())).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

include!("data/maths_corpus.rs");
include!("data/maths_engineering.rs");

/// A source's braces are only legitimately on screen when it wrote `\{`, `\}` or `\lbrace`, or a
/// `cases` or `Bmatrix`, whose one-line form draws its brace as a brace.
fn may_show_braces(src: &str) -> bool {
    src.contains(r"\{") || src.contains(r"\}") || src.contains("brace") || src.contains("cases") || src.contains("Bmatrix")
}

/// Every formula with its category: the general 75, then the engineering and research sets.
fn all_formulas() -> Vec<(&'static str, &'static str, bool)> {
    let mut v: Vec<(&str, &str, bool)> = CORPUS.iter().map(|(s, d)| ("general", *s, *d)).collect();
    for (cat, items) in ENGINEERING {
        v.extend(items.iter().map(|(s, d)| (*cat, *s, *d)));
    }
    v
}

/// The never-raw check on one drawn screen, at a width where the formula fits and at one where a
/// wide matrix is clipped or a wide formula is set on lines.
fn assert_no_source(src: &str, shown: &str) {
    assert!(!shown.contains('\\'), "a backslash reached the grid for {src:?}:\n{shown}");
    assert!(!shown.contains('$'), "a dollar reached the grid for {src:?}:\n{shown}");
    assert!(!shown.contains('&'), "an alignment & reached the grid for {src:?}:\n{shown}");
    if !may_show_braces(src) {
        assert!(!shown.contains(['{', '}']), "a brace reached the grid for {src:?}:\n{shown}");
    }
}

#[test]
fn every_corpus_formula_parses_and_renders_with_no_source_on_screen() {
    let all = all_formulas();
    assert!(all.len() >= 200, "the corpus has {} formulas", all.len());
    let mut per: Vec<(&str, usize, usize)> = Vec::new();
    let mut failures = Vec::new();
    for (cat, src, display) in &all {
        let ok = math::parses(src);
        match per.iter_mut().find(|(c, _, _)| c == cat) {
            Some(e) => {
                e.1 += 1;
                e.2 += usize::from(ok);
            }
            None => per.push((cat, 1, usize::from(ok))),
        }
        if !ok {
            failures.push(*src);
        }
        let md = if *display { format!("Here:\n\n$$\n{src}\n$$\n\nDone.") } else { format!("Here ${src}$ done.") };
        for (w, h) in [(160, 40), (60, 40)] {
            let shown = screen(&frame(&md, false, w, h));
            assert_no_source(src, &shown);
            assert!(shown.contains("Here") && shown.contains("done") || shown.contains("Done"), "{shown}");
        }
        if *display {
            let f = math::display(src);
            assert!(f.fidelity == math::Fidelity::Typeset, "{src:?} was {:?}", f.fidelity);
        }
    }
    println!("maths corpus, grid (strict parse, nothing raw on screen at 160 and 60 columns):");
    for (cat, n, ok) in &per {
        println!("  {cat:<30} {ok:>3}/{n:<3} {:>5.1}%", 100.0 * *ok as f64 / *n as f64);
    }
    let parsed = all.len() - failures.len();
    println!("  {:<30} {parsed:>3}/{:<3} {:>5.1}%", "all", all.len(), 100.0 * parsed as f64 / all.len() as f64);
    assert!(failures.is_empty(), "these did not parse strictly: {failures:#?}");
}

/// Model slips. Each is rendered three ways, inline, `\(…\)` and as a display block, and **no**
/// frame may show a backslash, a dollar or a brace the source did not escape.
const MALFORMED: &[&str] = &[
    r"\frac{a}{b",
    r"\sqrt{x + 1",
    r"x^{2",
    r"\left( \frac{a}{b}",
    r"\frac{a}{b} \right)",
    r"\begin{aligned} a &= b \\ c &= d",
    r"a &= b \\ c &= d",
    r"\end{pmatrix}",
    r"\unknowncmd{x}",
    r"\foo",
    r"\frac{1}",
    r"x^",
    r"\sqrt[3",
    r"\text{unclosed",
    r"}}}{{{",
    r"\begin{matrix} a & b \end{pmatrix}",
    r"\left[ x \right",
    r"##1",
    r"\\\\\\",
    r"\mathbb{",
    r"\over",
    r"a_{b_{c_{d",
    r"\operatorname{",
    r"\hat",
    r"\begin{unknownenv} x \end{unknownenv}",
    r"\frac{a}{b}}}",
    r"{x",
    r"x_",
    r"\left",
    r"\right.",
    r"\begin{cases} a & b \\",
    r"\int_0^",
    r"\sum_{i=1}^{n",
];

#[test]
fn a_malformed_formula_never_puts_its_source_on_screen() {
    let mut frames = 0;
    for src in MALFORMED {
        let forms = [
            format!("Here \\({src}\\) done."),
            format!("Here:\n\n$$\n{src}\n$$\n\nDone."),
            format!("Here:\n\n\\[{src}\\]\n\nDone."),
        ];
        for md in forms.iter().chain(
            // The single-dollar form only where the source has no dollar of its own to close it.
            (!src.contains('$')).then(|| format!("Here ${src}$ done.")).iter(),
        ) {
            let shown = screen(&frame(md, false, 120, 20));
            frames += 1;
            assert!(!shown.contains('\\'), "{src:?} as {md:?} showed a backslash:\n{shown}");
            assert!(!shown.contains('$'), "{src:?} as {md:?} showed a dollar:\n{shown}");
            // A `cases` brace is the rendering of the environment, not its source.
            if !src.contains("cases") {
                assert!(!shown.contains(['{', '}']), "{src:?} as {md:?} showed a brace:\n{shown}");
            }
        }
    }
    println!("malformed maths: {} sources, {frames} frames, no source on screen", MALFORMED.len());
}

/// A reply streamed one character at a time. Every prefix is a frame.
const STREAMED: &str = "The attention weights are $\\operatorname{softmax}(QK^\\top/\\sqrt{d})$, and the loss is\n\
$$\n\
\\mathcal{L} = -\\frac{1}{N}\\sum_{i=1}^{N} \\log p_\\theta(y_i \\mid x_i)\n\
$$\n\
which we minimise with $\\theta \\leftarrow \\theta - \\eta \\nabla_\\theta \\mathcal{L}$ until\n\
\\[\n\
\\begin{aligned} \\|g\\| &< \\epsilon \\\\ t &\\leq T \\end{aligned}\n\
\\]\n\
holds. Done.";

#[test]
fn streaming_shows_no_raw_frame_and_nothing_moves_once_a_block_lands() {
    let width = 100;
    let chars: Vec<char> = STREAMED.chars().collect();
    let mut previous: Vec<String> = Vec::new();
    // Rows that have settled: once a display block has closed, it and everything above it are fixed.
    let mut settled: usize = 0;
    let mut placeholder_frames = 0;
    for k in 1..=chars.len() {
        let prefix: String = chars[..k].iter().collect();
        let now = texts(&lines(&prefix, true, width));
        let joined = now.join("\n");
        assert!(!joined.contains('\\'), "frame {k} showed a backslash:\n{joined}");
        assert!(!joined.contains('$'), "frame {k} showed a dollar:\n{joined}");
        assert!(!joined.contains(['{', '}']), "frame {k} showed a brace:\n{joined}");
        if joined.contains(markdown::MATHS_PLACEHOLDER) {
            placeholder_frames += 1;
        }
        // Nothing that had settled may change, and the tail only grows: a reader following it is
        // never pulled back.
        assert!(now.len() >= previous.len(), "frame {k}: the reply got shorter ({} -> {} rows)", previous.len(), now.len());
        assert!(now.len() >= settled, "frame {k} lost settled rows");
        assert_eq!(
            &now[..settled],
            &previous[..settled.min(previous.len())],
            "frame {k} moved a row above a landed display block"
        );
        // A display block has just closed when its closer is the last thing that arrived.
        if prefix.ends_with("$$") && prefix.matches("$$").count().is_multiple_of(2) || prefix.ends_with("\\]") {
            // The block's last row is the last non-empty row now on screen.
            settled = now.iter().rposition(|l| !l.trim().is_empty()).map_or(0, |r| r + 1);
            // Held behind one row, it lands by growing downward from it.
            assert_eq!(previous.last().map(String::as_str).map(str::trim), Some(markdown::MATHS_PLACEHOLDER), "frame {k}: the block was not held");
        }
        previous = now;
    }
    assert!(settled > 0, "no display block ever landed");
    assert!(placeholder_frames > 20, "only {placeholder_frames} frames held a span back");
    // And the end of the stream is not a jump either: the finished render equals the last live one.
    assert_eq!(texts(&lines(STREAMED, false, width)), previous, "the finished reply re-laid out");
    println!(
        "streaming maths: {} frames, {placeholder_frames} held behind the placeholder, 0 raw, 0 settled rows moved",
        chars.len()
    );
}

/// Tokens the way a BPE model emits them: a command with its backslash, a word with the space
/// before it, every other character alone.
fn tokens(s: &str) -> Vec<String> {
    let cs: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < cs.len() {
        let start = i;
        if cs[i] == '\\' && cs.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic()) {
            i += 1;
            while i < cs.len() && cs[i].is_ascii_alphabetic() {
                i += 1;
            }
        } else if cs[i] == ' ' && cs.get(i + 1).is_some_and(|c| c.is_alphabetic()) {
            i += 1;
            while i < cs.len() && cs[i].is_alphabetic() {
                i += 1;
            }
        } else if cs[i].is_alphabetic() {
            while i < cs.len() && cs[i].is_alphabetic() {
                i += 1;
            }
        } else {
            i += 1;
        }
        out.push(cs[start..i].iter().collect());
    }
    out
}

/// Every corpus formula streamed token by token in each way a reply carries maths: a `$$` block,
/// a `\[` block with each matrix row on its own line, `$$` mid-paragraph, and inline. While it
/// streams the reply may only grow: a frame with fewer rows than the one before pulls a reader who
/// is following the tail back up the screen, which is the jitter Matthew saw.
#[test]
fn every_corpus_formula_streams_without_the_reply_ever_getting_shorter() {
    let width = 90;
    let mut frames = 0;
    let mut falls: Vec<String> = Vec::new();
    for (_, src, display) in all_formulas() {
        let forms = if display {
            vec![
                format!("Before it.\n\n$$\n{src}\n$$\n\nAfter it, with $x$ inline."),
                format!("Before it.\n\n\\[\n{}\n\\]\n\nAfter it.", src.replace("\\\\ ", "\\\\\n")),
                format!("so that $$ {src} $$ holds, and then more words follow."),
            ]
        } else {
            vec![format!("We have ${src}$ here, and some words after it.")]
        };
        for md in forms {
            let mut so_far = String::new();
            let mut prev = 0usize;
            for t in tokens(&md) {
                so_far.push_str(&t);
                let n = lines(&so_far, true, width).len();
                frames += 1;
                if n < prev {
                    falls.push(format!("{prev} -> {n} rows after {t:?} in:\n{so_far}"));
                    break;
                }
                prev = n;
            }
            let done = lines(&md, false, width).len();
            if done < prev {
                falls.push(format!("{prev} -> {done} rows when it finished:\n{md}"));
            }
        }
    }
    println!("corpus streamed: {frames} frames, {} replies got shorter", falls.len());
    assert!(falls.is_empty(), "{}", falls.join("\n\n"));
}

#[test]
fn the_placeholder_is_calm_and_the_maths_is_in_the_palette_with_no_fill() {
    let t = theme();
    let held = frame("The value is $\\frac{a}{", true, 80, 5);
    let mut seen = false;
    for y in 0..5 {
        for x in 0..80 {
            let c = held.cell((x, y)).unwrap();
            assert_eq!(c.bg, Color::Reset, "a maths cell carries a background fill");
            if c.symbol() == markdown::MATHS_PLACEHOLDER {
                seen = true;
                assert_eq!(c.fg, t.speech(), "the placeholder is not in Marlowe's speech colour");
                assert!(c.modifier.contains(ratatui::style::Modifier::DIM), "the placeholder is not dim");
            }
        }
    }
    assert!(seen, "no placeholder on screen:\n{}", screen(&held));
    let declared = t.declared_colours();
    let done = frame(&format!("$$\n{}\n$$", CORPUS[32].0), false, 160, 10);
    for y in 0..10 {
        for x in 0..160 {
            let c = done.cell((x, y)).unwrap();
            assert_eq!(c.bg, Color::Reset);
            assert!(declared.contains(&c.fg), "{:?} is outside the palette", c.fg);
        }
    }
}

#[test]
fn a_formula_typesets_in_well_under_a_frame() {
    let mut samples: Vec<f64> = Vec::new();
    let all = all_formulas();
    // Cold: the first layout this test asks for (the grid has no engine to build).
    let t0 = std::time::Instant::now();
    std::hint::black_box(math::typeset_uncached(all[0].1, true));
    let cold = t0.elapsed().as_secs_f64() * 1e3;
    for round in 0..8 {
        for (_, src, _) in &all {
            // A distinct source each round so nothing is served from the memo.
            let s = format!("{src}{}", " ".repeat(round));
            let t0 = std::time::Instant::now();
            let d = math::typeset_uncached(&s, true);
            let i = math::typeset_uncached(&s, false);
            samples.push(t0.elapsed().as_secs_f64() * 1e3);
            assert!(!d.rows.is_empty() && !i.rows.is_empty());
        }
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at = |q: f64| samples[((samples.len() as f64 - 1.0) * q) as usize];
    let (p50, p95, max) = (at(0.5), at(0.95), samples[samples.len() - 1]);
    println!(
        "maths typeset (display + inline, uncached, this build's profile): cold {cold:.3} ms; p50 {p50:.3} ms, p95 {p95:.3} ms, max {max:.3} ms over {} renders",
        samples.len()
    );
    assert!(p95 < 30.0, "p95 {p95:.3} ms is over the 30 ms budget");
}

#[test]
fn user_text_typesets_its_maths_and_leaves_everything_else_as_typed() {
    let t = theme();
    let out = texts(&markdown::render_text(
        "is **this** right: $\\frac{a}{b} = c$ and it costs $5 and $10",
        80,
        &t,
        ratatui::style::Style::default(),
        false,
    ))
    .join("\n");
    assert!(out.contains("**this**"), "the user's markdown was interpreted: {out}");
    assert!(out.contains("a/b = c"), "{out}");
    assert!(out.contains("$5 and $10"), "currency was eaten: {out}");
    assert!(!out.contains('\\'), "{out}");
}

/// Inline spans with **no command**, whose only TeX is scripts, primes and brackets: the shape of
/// `$G = -L'(f(X_{batch}))$`, which ui-preview5 showed as source because `batch` read as a word.
/// Each is set in a sentence after other inline maths, finished and streamed; no frame may show a
/// dollar or a brace, and the prose around it stays as written.
#[test]
fn inline_spans_with_words_in_their_scripts_are_typeset_in_a_sentence() {
    const SPANS: &[&str] = &[
        "G = -L'(f(X_{batch}))",
        "X_{batch}",
        "L_{total} = L_{ce} + L_{reg}",
        "W^{out} h_t",
        "p_{model}(x)",
        "y' = f'(x_{train})",
        "x_batch",
    ];
    // Generated: a script word in each of the places a model writes one.
    const WORDS: &[&str] = &["batch", "train", "test", "model", "out", "hidden", "total", "data", "prior", "post"];
    const SHAPES: &[&str] = &[
        "X_{@}",
        "f(X_{@})",
        "-L'(f(X_{@}))",
        "W^{@}x",
        "h_{@,t}",
        "E_{@}[x^2]",
        "(X_{@}, Y_{@})",
        "a_{@}' = b",
    ];
    let mut all: Vec<String> = SPANS.iter().map(|s| s.to_string()).collect();
    for w in WORDS {
        for s in SHAPES {
            all.push(s.replace('@', w));
        }
    }
    let mut frames = 0;
    for src in &all {
        let md = format!(r"Take $X_b \subset X$, compute ${src}$, then update $\theta_j$ and stop.");
        for (w, live) in [(120u16, false), (60, false), (120, true)] {
            let shown = screen(&frame(&md, live, w, 12));
            frames += 1;
            assert!(!shown.contains('$'), "{src:?} at {w} (live {live}) showed a dollar:\n{shown}");
            assert!(!shown.contains(['{', '}']), "{src:?} at {w} (live {live}) showed a brace:\n{shown}");
            assert!(!shown.contains('\\'), "{src:?} at {w} (live {live}) showed a backslash:\n{shown}");
            let flat = shown.split_whitespace().collect::<Vec<_>>().join(" ");
            for word in ["Take", "compute", "then update", "and stop."] {
                assert!(flat.contains(word), "{src:?}: the prose lost {word:?}:\n{shown}");
            }
        }
        // Every prefix, as it streams: a held span is a placeholder, never its dollar.
        let chars: Vec<char> = md.chars().collect();
        for n in 1..=chars.len() {
            let prefix: String = chars[..n].iter().collect();
            let shown = screen(&frame(&prefix, true, 120, 12));
            assert!(!shown.contains(['{', '}', '\\']), "{src:?} streamed to {prefix:?}:\n{shown}");
        }
    }
    // Prices beside them are still prices.
    let shown = screen(&frame("compute $X_{batch}$ for $5 and $10 a run", false, 120, 6));
    assert!(shown.contains("$5 and $10"), "{shown}");
    println!("inline script-word spans: {} sources, {frames} finished frames, every prefix streamed", all.len());
}
