//! **Markdown in the conversation pane, inside §B2's colour budget.**
//!
//! Model replies are markdown. Until ADR-047 they were drawn as flat text, so a reply built out of
//! headings, lists and code read as one undifferentiated paragraph with punctuation in it.
//!
//! # The budget is the design constraint, and it is not a limitation
//!
//! §B2: *one accent plus three state colours plus three foreground weights*, and **state colours
//! encode state only, never category**. That forbids syntax highlighting as normally understood —
//! green for strings and blue for keywords would make the screen say *healthy* and *needs
//! attention* about a token. §B13 asserts the row and `b13_rendering.rs` checks every emitted
//! foreground against `Theme::declared_colours`.
//!
//! What is left is a real palette, not a consolation one:
//!
//! | markdown | rendered as | budget role |
//! |---|---|---|
//! | heading | accent, bold at levels 1–2 | accent = structure (§B2's table) |
//! | strong | `BOLD` | a text attribute, not a colour |
//! | emphasis | `ITALIC` | " |
//! | strikethrough | `CROSSED_OUT` | " |
//! | inline code, code block | foreground **weight 1** — the terminal's own | contrast against Marlowe's violet prose, at zero cost to the budget |
//! | block quote | weight 2, with a `▏` rule at weight 3 | dimming is load-bearing (§B2) |
//! | list bullet, ordered marker | accent | structure |
//! | link text / target | underlined / weight 2 | see below |
//! | horizontal rule | a run of `·` at weight 3 | **not** `─` — see below |
//! | table | aligned columns, bold header, **no rules** | " |
//!
//! Code taking **weight 1** is the one worth explaining. Marlowe's prose is `theme.speech()`, a
//! violet tint; the terminal's own foreground sits clearly apart from it without spending a colour,
//! and it is what a reader already associates with verbatim output. Dimming code would have been
//! the obvious choice and is backwards — code is the part of a reply people most want to read.
//!
//! # No background fill, and `REVERSED` is a background fill
//!
//! §B2: *"Never hardcode a background fill. The terminal's own background is the background."*
//! §B13 has a zero-fills row and `b13_rendering.rs` walks every cell asserting `cell.bg ==
//! Color::Reset`.
//!
//! **`Modifier::REVERSED` would pass that test and produce a filled block on screen.** It is a
//! modifier, not a `bg`, so the assertion that reads the `bg` field reads `Reset` for a cell that
//! the terminal paints as a solid slab. That is this project's own failure family — asserting a
//! property where it is *declared* rather than where it is *seen* — and it is the reason a code
//! fence here gets an indent and a weight rather than the reverse-video treatment most terminal
//! markdown renderers use. `tests/markdown_render.rs` asserts no cell carries `REVERSED`.
//!
//! # Every render is a pure function of `(state, now_ms)`, and this one is a pure function of
//! `(text, width, theme)`
//!
//! K4 makes zero repaint flicker a kill criterion, and `b13_rendering.rs` proves it by rendering
//! frame N and frame N+1 and diffing the buffers cell by cell. That only works because rendering is
//! pure. Parsing happens on every frame, which is also what makes the scrollbar honest:
//! `render::transcript_lines` is the only thing that knows how many wrapped lines a transcript
//! produced. The one memo is [`crate::math`]'s, keyed on a formula's full source: a pure function
//! memoised by its whole input returns what it would have computed, so purity holds.
//!
//! The cost is measured rather than argued: `tests/markdown_cost.rs` lays out a large transcript
//! against K4's 150 ms first-frame budget.
//!
//! # Maths while a reply streams
//!
//! [`render_prose_live`] is the streaming form. An unclosed maths span at the tail of the reply is
//! **held**: inline it shows a dim `…` in the prose colour, display it reserves the rows the
//! formula will need (laid out from what has arrived) with the `…` on the first. Neither shows a
//! delimiter or a command. When the closer arrives the formula is typeset once, in place. A closed
//! span never changes again, so nothing above or below it moves.
//!
//! # The model authors this text
//!
//! See [`crate::chrome`]. Interpreting markup hands the model control of visual structure, and the
//! mitigation is structural: harness glyphs are refused in model prose, the horizontal rule is
//! drawn in a glyph the compaction marker does not use, and tables are laid out with no rules at
//! all so that a §B2 border cannot be assembled out of one.

use std::sync::Arc;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::math::{self, Formula};
use crate::theme::Theme;

/// What an unclosed maths span shows while it streams. Not `⋯`: that is §B6's tool-line marker.
pub const MATHS_PLACEHOLDER: &str = "…";

/// Display columns. Combining marks are zero wide and CJK is two, so `chars().count()` is wrong
/// for both, and maths now emits combining accents.
fn cols(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// How deep an inline construct may nest before the parser stops interpreting and takes the rest
/// literally. Bounded so a pathological reply cannot turn one frame into a stack walk.
const MAX_INLINE_DEPTH: usize = 8;

/// The narrowest pane worth laying out into. Below this the wrap arithmetic stops being meaningful;
/// §B11 already refuses the whole frame under 120 columns, so this is a floor, not a policy.
const MIN_WIDTH: usize = 8;

/// How deep block structure may nest before the parser stops recursing and takes the rest as
/// prose.
///
/// `> `×10_000 is one line of text and ten thousand levels of recursion in a naive block parser,
/// which is a stack overflow rather than a slow frame — a surface that aborts mid-draw on a reply.
/// Six levels is more than any legible terminal nesting; past it, the line is a paragraph.
const MAX_BLOCK_DEPTH: usize = 6;

/// **The one entry point.** Model prose, as styled lines, wrapped to `width`.
///
/// `base` is the style the surrounding transcript would have used — `theme.speech()` for Marlowe's
/// voice — so this module never decides what colour ordinary prose is.
pub fn render_prose(raw: &str, width: usize, theme: &Theme, base: Style) -> Vec<Line<'static>> {
    render(raw, width, theme, base, false)
}

/// [`render_prose`] for a reply that is **still streaming**. The only difference is the tail: an
/// unclosed maths span there is held behind a placeholder instead of being read as finished.
pub fn render_prose_live(raw: &str, width: usize, theme: &Theme, base: Style) -> Vec<Line<'static>> {
    render(raw, width, theme, base, true)
}

/// A display formula laid out on its own rows at the top level of a reply: where it is, and the
/// box of cells its grid layout fills. The image path (`marlowe-typeset`) draws into exactly this
/// box, so the rows around it never move when an image replaces the grid.
#[derive(Debug, Clone)]
pub struct MathBlock {
    /// Index of the block's first row in the returned lines.
    pub line: usize,
    /// Columns of indent before the formula.
    pub col: usize,
    pub rows: usize,
    pub cols: usize,
    pub formula: Arc<Formula>,
    /// The colour its grid rows are drawn in.
    pub fg: Color,
}

/// [`render_prose`] or [`render_prose_live`], and the display formulas in the result that an image
/// could replace. Only top-level blocks: inside a list or a quote a formula stays on the grid.
pub fn render_prose_blocks(
    raw: &str,
    width: usize,
    theme: &Theme,
    base: Style,
    live: bool,
) -> (Vec<Line<'static>>, Vec<MathBlock>) {
    let mut blocks = Vec::new();
    let lines = render_into(raw, width, theme, base, live, Some(&mut blocks));
    (lines, blocks)
}

fn render(raw: &str, width: usize, theme: &Theme, base: Style, live: bool) -> Vec<Line<'static>> {
    render_into(raw, width, theme, base, live, None)
}

fn render_into(
    raw: &str,
    width: usize,
    theme: &Theme,
    base: Style,
    live: bool,
    maths: Option<&mut Vec<MathBlock>>,
) -> Vec<Line<'static>> {
    let prepared = crate::chrome::prepare_model_text(raw);
    // Tabs are expanded before block parsing, not during. Markdown's block structure is defined in
    // columns, and a `\t` inside a `Buffer` cell is neither one column nor four.
    let expanded = expand_tabs(&prepared);
    let lines: Vec<&str> = expanded.split('\n').collect();
    let blocks = parse_blocks(&lines, 0);
    let ctx = Ctx {
        theme,
        base,
        budget: std::cell::Cell::new(scan_budget(expanded.len())),
    };
    let width = width.max(MIN_WIDTH);
    let Some(found) = maths else {
        return layout_blocks(&blocks, &ctx, width, live);
    };
    // The top level, laid out block by block so each display formula's first row is known.
    let mut out: Vec<Line<'static>> = Vec::new();
    for (n, b) in blocks.iter().enumerate() {
        let tail = live && n + 1 == blocks.len();
        out.extend(block_gap(&blocks, n));
        let at = out.len();
        out.extend(layout_block(b, &ctx, width, tail));
        if let Block::DisplayMath { src, end } = b {
            let held = *end == MathEnd::Eof && tail;
            let inner = width.saturating_sub(2).max(MIN_WIDTH);
            let f = math::display(src);
            // A matrix too wide for the pane is drawn clipped on the grid; its image is offered
            // the same box, and is used if it fits there legibly.
            let drawn_2d = f.width <= inner || f.clipped(inner).is_some();
            if !held && f.rows.len() > 1 && drawn_2d && f.typst.is_some() {
                found.push(MathBlock {
                    line: at,
                    col: 2,
                    rows: f.rows.len(),
                    cols: f.width.min(inner),
                    fg: ctx.maths().fg.unwrap_or(Color::Reset),
                    formula: f,
                });
            }
        }
    }
    out
}

/// Whether `text` has anything [`render_text`] would typeset. Lets a caller keep its own wrapping
/// for the text that has none.
pub fn has_maths(text: &str) -> bool {
    text.contains('$') || text.contains("\\(") || text.contains("\\[") || text.contains("\\begin{")
}

/// One line of text with each maths span replaced by its one-line form, for places that hold a
/// single unstyled line: a session title, a run title. Display spans take their linear form here.
pub fn maths_one_line(raw: &str) -> String {
    if !has_maths(raw) {
        return raw.to_string();
    }
    let theme = Theme::default_truecolor();
    let ctx = Ctx {
        theme: &theme,
        base: Style::default(),
        budget: std::cell::Cell::new(scan_budget(raw.len())),
    };
    inline_maths_only(raw, &ctx, Style::default(), false).into_iter().map(|r| r.text).collect()
}

/// **Plain text with its maths typeset, and nothing else interpreted.**
///
/// For the user's own words and for reasoning: showing someone's `**` back to them as bold would
/// rewrite what they typed, but showing their `$\alpha$` as source would break the rule that maths
/// never reaches the screen raw. Lines are kept as written; `$…$`, `\(…\)`, `$$…$$`, `\[…\]` and
/// `\begin{…}` blocks are rendered exactly as in a reply.
pub fn render_text(raw: &str, width: usize, theme: &Theme, base: Style, live: bool) -> Vec<Line<'static>> {
    let prepared = crate::chrome::prepare_model_text(raw);
    let expanded = expand_tabs(&prepared);
    let lines: Vec<&str> = expanded.split('\n').collect();
    let ctx = Ctx {
        theme,
        base,
        budget: std::cell::Cell::new(scan_budget(expanded.len())),
    };
    let width = width.max(MIN_WIDTH);
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        // A fenced code block is kept exactly as typed, `$` and all. ```` ```math ```` is maths.
        if let Some(fence) = fence_open(lines[i]) {
            if lines[i].trim_start()[fence.len()..].trim() == "math" {
                let mut j = i + 1;
                while j < lines.len() && !fence_closes(lines[j], &fence) {
                    j += 1;
                }
                let end = if j < lines.len() { MathEnd::Closed } else { MathEnd::Eof };
                let block = Block::DisplayMath { src: lines[i + 1..j].join("\n"), end };
                out.extend(layout_block(&block, &ctx, width, live));
                i = j + 1;
                continue;
            } else {
                let mut j = i;
                loop {
                    let runs = [Run::plain(lines[j], base)];
                    out.extend(wrap_runs(&runs, width).into_iter().map(Line::from));
                    j += 1;
                    if j >= lines.len() || (j > i + 1 && fence_closes(lines[j - 1], &fence)) {
                        break;
                    }
                }
                i = j;
                continue;
            }
        }
        if let Some((block, consumed, trailing)) = display_block_at(&lines, i) {
            let tail = live && i + consumed >= lines.len() && trailing.is_none();
            out.extend(layout_block(&block, &ctx, width, tail));
            if let Some(t) = trailing {
                let runs = inline_maths_only(&t, &ctx, base, live && i + consumed >= lines.len());
                out.extend(wrap_runs(&runs, width).into_iter().map(Line::from));
            }
            i += consumed.max(1);
            continue;
        }
        let line = lines[i];
        if line.trim().is_empty() {
            out.push(Line::from(""));
        } else {
            let runs = inline_maths_only(line, &ctx, base, live && i + 1 == lines.len());
            out.extend(wrap_runs(&runs, width).into_iter().map(Line::from));
        }
        i += 1;
    }
    out
}

/// Whether `chars[i]` may open a maths span: `$`, `\(`, `\[`, or a `\begin{…}` that
/// [`maths`] checks is a maths environment.
fn opens_maths(chars: &[char], i: usize) -> bool {
    match (chars[i], chars.get(i + 1)) {
        ('$', _) => true,
        ('\\', Some('(' | '[')) => true,
        ('\\', Some('b')) => chars[i..].starts_with(&['\\', 'b', 'e', 'g', 'i', 'n', '{']),
        _ => false,
    }
}

/// Literal text with only maths spans interpreted. A backtick code span is kept as typed, maths
/// signs and all: `` `echo $A$B` `` is a command, not a formula.
fn inline_maths_only(src: &str, ctx: &Ctx, style: Style, live: bool) -> Vec<Run> {
    let chars: Vec<char> = src.chars().collect();
    let end = if live { held_tail(&chars) } else { chars.len() };
    let mut out: Vec<Run> = Vec::new();
    let mut lit = String::new();
    let mut i = 0;
    while i < end {
        let c = chars[i];
        if c == '`' {
            let ticks = chars[i..end].iter().take_while(|c| **c == '`').count();
            let close = (i + ticks..end).find(|&j| {
                chars[j..end].iter().take_while(|c| **c == '`').count() == ticks
                    && (j == 0 || chars[j - 1] != '`')
            });
            let stop = close.map_or(i + ticks, |j| j + ticks);
            lit.extend(&chars[i..stop]);
            i = stop;
            continue;
        }
        if opens_maths(&chars[..end], i) && !ctx.exhausted() {
            if let Some((runs, next)) = maths(&chars[..end], i, ctx, style, live) {
                if !lit.is_empty() {
                    out.push(Run::plain(std::mem::take(&mut lit), style));
                }
                out.extend(runs);
                i = next;
                continue;
            }
        }
        lit.push(c);
        i += 1;
    }
    if !lit.is_empty() {
        out.push(Run::plain(lit, style));
    }
    out
}

/// Where the shown part of a streaming tail ends. A lone trailing `\`, or the start of a
/// `\begin{…}` that has not finished arriving, is the front of a delimiter and is held back.
fn held_tail(chars: &[char]) -> usize {
    let n = chars.len();
    let Some(slash) = chars.iter().rposition(|c| *c == '\\') else { return n };
    let tail: String = chars[slash..].iter().collect();
    let begin_prefix = "\\begin".starts_with(&tail)
        || (tail.starts_with("\\begin{") && !tail.contains('}'));
    // `\et` may be the front of `\eta`, which will not be shown as source for a frame.
    let command = tail.len() > 1 && tail[1..].chars().all(|c| c.is_ascii_alphabetic());
    if begin_prefix || tail == "\\" || command {
        slash
    } else {
        n
    }
}

/// How much look-ahead one reply may spend finding closing delimiters.
///
/// # A bound on the model's ability to stall the surface, and it was measured rather than argued
///
/// Inline markdown scans forward for a closer, and an opener with no closer scans to the end of the
/// block. That is cheap in prose, where closers are near, and quadratic in a reply that is nothing
/// but openers. `tests/markdown_cost.rs` measured **144 ms in one frame** on 140 KB of `[[[[…`,
/// backtick runs and `***nested ` — past K4's entire 150 ms first-frame budget, reached by a model
/// choosing its punctuation. The test found it; no amount of reading the parser would have.
///
/// One budget, shared by every scanning construct, charged per character examined. When it runs out
/// the rest of the reply renders as **literal text** — which is what the pane did before ADR-047,
/// so the degraded state is the previous product rather than a broken one.
///
/// Linear in the input, so a long legitimate reply is never penalised: ordinary markdown spends
/// roughly its own length, and this allows sixteen times that, plus a floor for short replies.
fn scan_budget(len: usize) -> usize {
    len.saturating_mul(16).saturating_add(4_096)
}

struct Ctx<'a> {
    theme: &'a Theme,
    base: Style,
    /// See [`scan_budget`]. Interior mutability, and it does **not** compromise purity: the cell is
    /// constructed fresh inside [`render_prose`], so the whole function is still a pure function of
    /// `(text, width, theme, base)` and two renders of one state are identical — which is what
    /// §B13's flicker rows require and `markdown_render.rs` asserts.
    budget: std::cell::Cell<usize>,
}

impl Ctx<'_> {
    /// Verbatim text: foreground **weight 1**, the terminal's own. See the module header.
    fn code(&self) -> Style {
        Style::default().fg(Color::Reset)
    }

    /// **Rendered maths, one weight LIGHTER than the prose around it.**
    ///
    /// An equation is the load-bearing part of a technical reply and it should be findable by
    /// scanning. It is not given a colour, because §B13 allows one accent plus three state colours
    /// plus three foreground weights and **state colours encode state, never category** — amber for
    /// a formula would say "needs attention" to a reader who has learned what amber means.
    ///
    /// So it moves up the weight ladder instead, relative to its context rather than absolutely:
    /// prose is `normal` and its maths is `bright`; a reasoning block is `dim` and its maths is
    /// `normal`. The equation stands out by the same distance either way, and a reasoning block
    /// stays quieter than the conversation, which is the whole point of dimming it.
    fn maths(&self) -> Style {
        let lighter = if self.base.fg == Some(self.theme.dim().fg.unwrap_or(Color::Reset)) {
            self.theme.normal()
        } else {
            self.theme.bright()
        };
        Style::default().fg(lighter.fg.unwrap_or(Color::Reset))
    }
    /// The held-back maths placeholder: the prose colour, dimmed. Calm, and plainly not content.
    fn placeholder(&self) -> Style {
        Style::default()
            .fg(self.base.fg.unwrap_or(self.theme.speech()))
            .add_modifier(Modifier::DIM)
    }
    fn accent(&self) -> Style {
        Style::default().fg(self.theme.accent())
    }
    /// Charge `n` characters of look-ahead. `false` once the reply has spent its budget.
    fn spend(&self, n: usize) -> bool {
        let left = self.budget.get();
        if left <= n {
            self.budget.set(0);
            return false;
        }
        self.budget.set(left - n);
        true
    }
    fn exhausted(&self) -> bool {
        self.budget.get() == 0
    }
}

fn expand_tabs(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.contains('\t') {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    for line in s.split('\n') {
        if !out.is_empty() {
            out.push('\n');
        }
        let mut col = 0usize;
        for c in line.chars() {
            if c == '\t' {
                let n = 4 - (col % 4);
                out.push_str(&" ".repeat(n));
                col += n;
            } else {
                out.push(c);
                col += 1;
            }
        }
    }
    std::borrow::Cow::Owned(out)
}

// ── blocks ──────────────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Block {
    Heading { level: u8, text: String },
    Para(String),
    /// A fenced or indented code block. Held as source lines: code is not re-flowed.
    Code(Vec<String>),
    Quote(Vec<Block>),
    List {
        ordered: bool,
        start: u64,
        items: Vec<Vec<Block>>,
    },
    Rule,
    Table {
        head: Vec<String>,
        rows: Vec<Vec<String>>,
        /// The source, kept so a table too wide for the pane can be shown as what the model wrote
        /// rather than as a squashed grid. §B11's instinct, applied to a table.
        source: Vec<String>,
    },
    /// Display maths: `$$…$$` or `\[…\]` from the start of a line, possibly over several lines, a
    /// `\begin{align}`-style environment, or a ```` ```math ```` fence. Held without delimiters.
    DisplayMath { src: String, end: MathEnd },
}

/// How a display block ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MathEnd {
    Closed,
    /// A blank line came before the closer: a slip, rendered as though closed.
    Blank,
    /// The input ran out first: still streaming if the reply is live.
    Eof,
}

/// Environments that are display maths on their own, without `$$` around them.
const MATH_ENVS: &[&str] = &[
    "equation", "align", "aligned", "gather", "gathered", "multline", "eqnarray", "split",
    "alignat", "alignedat", "flalign", "displaymath", "cases", "dcases", "rcases", "drcases",
    "matrix", "pmatrix", "bmatrix", "Bmatrix", "vmatrix", "Vmatrix", "smallmatrix", "array",
];

/// `\begin{env}` at the start of `t`, for a maths environment. Returns the name, star included.
fn math_env_open(t: &str) -> Option<String> {
    let rest = t.strip_prefix("\\begin{")?;
    let name = &rest[..rest.find('}')?];
    MATH_ENVS.contains(&name.trim_end_matches('*')).then(|| name.to_string())
}

/// Whether a line opens a display block. Cheap: used to end a paragraph.
fn opens_display(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("$$") || t.starts_with("\\[") || math_env_open(t).is_some()
}

/// The first unescaped `close` in `s`, as a byte offset.
fn find_closer(s: &str, close: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(p) = s[from..].find(close) {
        let at = from + p;
        let escaped = at > 0 && s.as_bytes()[at - 1] == b'\\' && close != "\\]";
        if !escaped {
            return Some(at);
        }
        from = at + close.len();
    }
    None
}

/// A display block starting at `lines[i]`: the block, the lines it used, and any text after the
/// closer on its last line (which becomes a paragraph).
fn display_block_at(lines: &[&str], i: usize) -> Option<(Block, usize, Option<String>)> {
    let t = lines[i].trim_start();
    // (the text after the opener, the closer, whether the closer belongs to the content)
    let (first, close, keep_close): (&str, String, bool) = if let Some(r) = t.strip_prefix("$$") {
        (r, "$$".into(), false)
    } else if let Some(r) = t.strip_prefix("\\[") {
        (r, "\\]".into(), false)
    } else {
        let env = math_env_open(t)?;
        (t, format!("\\end{{{env}}}"), true)
    };
    let finish = |body: String, line: &str, at: usize, used: usize| {
        let cut = if keep_close { at + close.len() } else { at };
        let mut src = body;
        src.push_str(&line[..cut]);
        let rest = line[at + close.len()..].trim();
        let trailing = (!rest.is_empty()).then(|| rest.to_string());
        Some((Block::DisplayMath { src, end: MathEnd::Closed }, used, trailing))
    };
    // The closer is searched for after the opener, so `$$` does not close itself.
    let search_from = if keep_close { "\\begin{".len() } else { 0 };
    if let Some(at) = find_closer(&first[search_from..], &close) {
        return finish(String::new(), first, at + search_from, 1);
    }
    let mut body = first.to_string();
    let mut j = i + 1;
    while j < lines.len() {
        let l = lines[j];
        if l.trim().is_empty() {
            return Some((Block::DisplayMath { src: body, end: MathEnd::Blank }, j - i, None));
        }
        body.push('\n');
        if let Some(at) = find_closer(l, &close) {
            return finish(body, l, at, j - i + 1);
        }
        body.push_str(l);
        j += 1;
    }
    Some((Block::DisplayMath { src: body, end: MathEnd::Eof }, j - i, None))
}

fn parse_blocks(lines: &[&str], depth: usize) -> Vec<Block> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() {
            i += 1;
            continue;
        }
        if let Some(fence) = fence_open(line) {
            // GitHub's ```` ```math ```` fence is display maths, not code.
            let is_math = line.trim_start()[fence.len()..].trim() == "math";
            let mut body = Vec::new();
            i += 1;
            while i < lines.len() && !fence_closes(lines[i], &fence) {
                body.push(lines[i].to_string());
                i += 1;
            }
            let closed = i < lines.len();
            if closed {
                i += 1; // the closing fence
            }
            if is_math {
                let end = if closed { MathEnd::Closed } else { MathEnd::Eof };
                out.push(Block::DisplayMath { src: body.join("\n"), end });
            } else {
                out.push(Block::Code(body));
            }
            continue;
        }
        if is_thematic_break(line) {
            out.push(Block::Rule);
            i += 1;
            continue;
        }
        if let Some((level, text)) = atx_heading(line) {
            out.push(Block::Heading { level, text });
            i += 1;
            continue;
        }
        if let Some((block, consumed, trailing)) = display_block_at(lines, i) {
            out.push(block);
            i += consumed.max(1);
            if let Some(t) = trailing {
                // Text after the closer starts a paragraph, which runs on through the lines after.
                let mut para = vec![t];
                while i < lines.len() && !lines[i].trim().is_empty() && !opens_display(lines[i]) {
                    para.push(lines[i].to_string());
                    i += 1;
                }
                let refs: Vec<&str> = para.iter().map(String::as_str).collect();
                out.push(Block::Para(join_paragraph(&refs)));
            }
            continue;
        }
        if line.trim_start().starts_with('>') && depth < MAX_BLOCK_DEPTH {
            let mut inner: Vec<String> = Vec::new();
            while i < lines.len() && lines[i].trim_start().starts_with('>') {
                let t = lines[i].trim_start();
                let rest = t.strip_prefix('>').unwrap_or(t);
                inner.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
                i += 1;
            }
            let refs: Vec<&str> = inner.iter().map(String::as_str).collect();
            out.push(Block::Quote(parse_blocks(&refs, depth + 1)));
            continue;
        }
        if let Some(table) = table_at(lines, i) {
            let consumed = table.1;
            out.push(table.0);
            i += consumed;
            continue;
        }
        if marker_at(line).is_some() && depth < MAX_BLOCK_DEPTH {
            let (block, consumed) = parse_list(lines, i, depth);
            out.push(block);
            i += consumed;
            continue;
        }
        // A paragraph: consecutive lines until a blank line or the start of another block.
        let mut para: Vec<&str> = Vec::new();
        while i < lines.len() {
            let l = lines[i];
            if l.trim().is_empty() {
                break;
            }
            if !para.is_empty()
                && (fence_open(l).is_some()
                    || is_thematic_break(l)
                    || atx_heading(l).is_some()
                    || opens_display(l)
                    || l.trim_start().starts_with('>')
                    || marker_at(l).is_some()
                    || table_at(lines, i).is_some())
            {
                break;
            }
            para.push(l);
            i += 1;
        }
        out.push(Block::Para(join_paragraph(&para)));
    }
    out
}

/// Soft line breaks join; a hard break — two trailing spaces, or a trailing backslash — is kept as
/// a `\n` for the layout to honour.
///
/// Joining is CommonMark's rule and it is the right one here: a model that wraps its own prose at
/// eighty columns must not have those breaks baked into a pane that is sixty-six columns wide. `Y`
/// still copies the source, so nothing is lost by the join.
fn join_paragraph(lines: &[&str]) -> String {
    let mut out = String::new();
    for (n, l) in lines.iter().enumerate() {
        let hard = l.ends_with("  ") || l.trim_end().ends_with('\\');
        let mut t = l.trim();
        if hard {
            t = t.trim_end_matches('\\').trim_end();
        }
        if n > 0 && !out.ends_with('\n') {
            out.push(' ');
        }
        out.push_str(t);
        if hard && n + 1 < lines.len() {
            out.push('\n');
        }
    }
    out
}

fn fence_open(line: &str) -> Option<String> {
    let t = line.trim_start();
    for f in ["```", "~~~"] {
        if t.starts_with(f) {
            return Some(f.to_string());
        }
    }
    None
}

fn fence_closes(line: &str, fence: &str) -> bool {
    let t = line.trim();
    t.starts_with(fence) && t.trim_end_matches(fence.chars().next().unwrap()).is_empty()
}

fn is_thematic_break(line: &str) -> bool {
    let t = line.trim();
    for c in ['-', '*', '_'] {
        let stripped: String = t.chars().filter(|x| *x != ' ').collect();
        if stripped.len() >= 3 && stripped.chars().all(|x| x == c) {
            return true;
        }
    }
    false
}

fn atx_heading(line: &str) -> Option<(u8, String)> {
    let t = line.trim_start();
    let hashes = t.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &t[hashes..];
    if !rest.is_empty() && !rest.starts_with(' ') {
        return None;
    }
    Some((
        hashes as u8,
        rest.trim().trim_end_matches('#').trim_end().to_string(),
    ))
}

/// `(indent, marker width, ordered, number)` for a list item line.
fn marker_at(line: &str) -> Option<(usize, usize, bool, u64)> {
    let indent = leading_spaces(line);
    let t = line.trim_start();
    let mut chars = t.chars();
    let first = chars.next()?;
    if matches!(first, '-' | '*' | '+') {
        // A thematic break is not a list item, and `***` matches both.
        if is_thematic_break(line) {
            return None;
        }
        if chars.next() == Some(' ') {
            return Some((indent, 2, false, 0));
        }
        return None;
    }
    if first.is_ascii_digit() {
        let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.len() > 9 {
            return None;
        }
        let after = &t[digits.len()..];
        if (after.starts_with(". ") || after.starts_with(") ")) && digits.parse::<u64>().is_ok() {
            return Some((
                indent,
                digits.len() + 2,
                true,
                digits.parse::<u64>().unwrap_or(1),
            ));
        }
    }
    None
}

fn parse_list(lines: &[&str], start: usize, depth: usize) -> (Block, usize) {
    let (base_indent, _, ordered, first_num) = marker_at(lines[start]).expect("caller checked");
    let mut items: Vec<Vec<Block>> = Vec::new();
    let mut i = start;
    while i < lines.len() {
        let Some((indent, marker_w, this_ordered, _)) = marker_at(lines[i]) else {
            break;
        };
        if indent != base_indent || this_ordered != ordered {
            break;
        }
        let content_indent = indent + marker_w;
        let mut body: Vec<String> = vec![drop_chars(lines[i].trim_start(), marker_w)];
        i += 1;
        // Continuation: blank lines, anything indented into the item, and lazy continuation lines
        // that do not themselves start a block.
        while i < lines.len() {
            let l = lines[i];
            if l.trim().is_empty() {
                // A blank line only continues the item if something indented follows it.
                let next_belongs = lines
                    .get(i + 1)
                    .map(|n| !n.trim().is_empty() && leading_spaces(n) >= content_indent)
                    .unwrap_or(false);
                if !next_belongs {
                    break;
                }
                body.push(String::new());
                i += 1;
                continue;
            }
            if leading_spaces(l) >= content_indent {
                body.push(drop_chars(l, content_indent));
                i += 1;
                continue;
            }
            if marker_at(l).is_some() || atx_heading(l).is_some() || is_thematic_break(l) {
                break;
            }
            body.push(l.trim_start().to_string());
            i += 1;
        }
        let refs: Vec<&str> = body.iter().map(String::as_str).collect();
        items.push(parse_blocks(&refs, depth + 1));
    }
    (
        Block::List {
            ordered,
            start: first_num,
            items,
        },
        i - start,
    )
}

/// Leading whitespace, **counted in characters, not bytes.**
///
/// Byte arithmetic here is a panic waiting for a non-breaking space: `l[1..]` on a line whose
/// indent is one two-byte character is not a character boundary, and the surface would abort
/// mid-frame on a reply nobody would think to test with.
fn leading_spaces(s: &str) -> usize {
    s.chars().take_while(|c| c.is_whitespace()).count()
}

fn drop_chars(s: &str, n: usize) -> String {
    s.chars().skip(n).collect()
}

/// A GitHub-style table: a header row, a delimiter row, then body rows.
fn table_at(lines: &[&str], i: usize) -> Option<(Block, usize)> {
    let head_line = lines[i];
    if !head_line.contains('|') {
        return None;
    }
    let delim = lines.get(i + 1)?;
    if !delim.contains('|') || !delim.trim().chars().all(|c| matches!(c, '-' | ':' | '|' | ' ')) {
        return None;
    }
    if !delim.contains('-') {
        return None;
    }
    let head = split_row(head_line);
    if head.len() < 2 {
        return None;
    }
    let mut rows = Vec::new();
    let mut source = vec![head_line.to_string(), delim.to_string()];
    let mut n = i + 2;
    while n < lines.len() && lines[n].contains('|') && !lines[n].trim().is_empty() {
        rows.push(split_row(lines[n]));
        source.push(lines[n].to_string());
        n += 1;
    }
    Some((Block::Table { head, rows, source }, n - i))
}

/// The cells of a table row. A `|` inside an inline formula (`$|x|$`, `$\|v\|$`) or escaped as
/// `\|` does not split the row: cutting a formula in half would put its `$` on screen.
fn split_row(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').filter(|r| !r.ends_with('\\')).unwrap_or(t);
    let chars: Vec<char> = t.chars().collect();
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() {
            cell.push(c);
            cell.push(chars[i + 1]);
            i += 2;
            continue;
        }
        if c == '$' {
            // The span to the next unescaped `$`, kept whole when it reads as maths.
            let mut j = i + 1;
            while j < chars.len() && !(chars[j] == '$' && chars[j - 1] != '\\') {
                j += 1;
            }
            if j < chars.len() {
                let content: String = chars[i + 1..j].iter().collect();
                if content.contains('|') && math::looks_like_inline_maths(&content) {
                    cell.extend(&chars[i..=j]);
                    i = j + 1;
                    continue;
                }
            } else if matches!(chars.get(i + 1), Some('|' | '\\')) {
                // Unclosed, and `$|` or `$\` is never a price: a formula still arriving (or whose
                // closer was dropped). The rest of the row stays with it rather than being cut.
                cell.extend(&chars[i..]);
                break;
            }
        }
        if c == '|' {
            cells.push(cell.trim().to_string());
            cell.clear();
        } else {
            cell.push(c);
        }
        i += 1;
    }
    cells.push(cell.trim().to_string());
    cells
}

// ── layout ──────────────────────────────────────────────────────────────────────────────────────

/// `live` reaches only the last block: an unclosed span anywhere else is not still arriving, the
/// reply has moved past it.
/// One blank line between blocks — except a list that follows its own lead-in sentence, which is
/// one thing and reads as one thing. This is also what makes a nested list sit directly under its
/// parent item instead of a row below it.
fn block_gap(blocks: &[Block], n: usize) -> Option<Line<'static>> {
    let tight = n > 0 && matches!(blocks[n], Block::List { .. }) && matches!(blocks[n - 1], Block::Para(_));
    (n > 0 && !tight).then(|| Line::from(""))
}

fn layout_blocks(blocks: &[Block], ctx: &Ctx, width: usize, live: bool) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    for (n, b) in blocks.iter().enumerate() {
        let tail = live && n + 1 == blocks.len();
        out.extend(block_gap(blocks, n));
        out.extend(layout_block(b, ctx, width, tail));
    }
    out
}

fn layout_block(block: &Block, ctx: &Ctx, width: usize, live: bool) -> Vec<Line<'static>> {
    match block {
        Block::Heading { level, text } => {
            // Accent is §B2's structure role. Bold at the top two levels gives three visible ranks
            // — bold accent, accent, prose — without a second colour.
            let mut style = ctx.accent();
            if *level <= 2 {
                style = style.add_modifier(Modifier::BOLD);
            }
            let runs = inline(text, ctx, style, 0, live);
            wrap_runs(&runs, width)
                .into_iter()
                .map(Line::from)
                .collect()
        }
        Block::Para(text) => {
            let mut out = Vec::new();
            let hards: Vec<&str> = text.split('\n').collect();
            for (k, hard) in hards.iter().enumerate() {
                let runs = inline(hard, ctx, ctx.base, 0, live && k + 1 == hards.len());
                out.extend(wrap_runs(&runs, width).into_iter().map(Line::from));
            }
            out
        }
        Block::Code(body) => {
            let indent = "  ";
            let inner = width.saturating_sub(2).max(MIN_WIDTH);
            let mut out = Vec::new();
            for l in body {
                // Code is **hard-wrapped, never re-flowed**. Word wrapping a code line moves tokens
                // across lines and changes what the reader believes the program says.
                for chunk in hard_chunks(l, inner) {
                    out.push(Line::from(vec![
                        Span::raw(indent),
                        Span::styled(chunk, ctx.code()),
                    ]));
                }
            }
            out
        }
        Block::Quote(inner) => {
            let body = layout_blocks(inner, ctx, width.saturating_sub(2).max(MIN_WIDTH), live);
            body.into_iter()
                .map(|line| {
                    let mut spans = vec![Span::styled(
                        format!("{} ", crate::chrome::QUOTE_RULE),
                        ctx.theme.dimmer(),
                    )];
                    // Weight 2 for the quoted text: dimming is load-bearing (§B2), and a quotation
                    // is by definition not the speaker's own current words.
                    spans.extend(line.spans.into_iter().map(|s| {
                        let dimmed = s.style.fg.is_none() || s.style.fg == ctx.base.fg;
                        if dimmed {
                            Span::styled(s.content.into_owned(), ctx.theme.dim().patch(strip_fg(s.style)))
                        } else {
                            Span::styled(s.content.into_owned(), s.style)
                        }
                    }));
                    Line::from(spans)
                })
                .collect()
        }
        Block::List {
            ordered,
            start,
            items,
        } => {
            let mut out = Vec::new();
            for (n, item) in items.iter().enumerate() {
                let marker = if *ordered {
                    format!("{}. ", start + n as u64)
                } else {
                    "• ".to_string()
                };
                let mw = marker.chars().count();
                let tail = live && n + 1 == items.len();
                let body = layout_blocks(item, ctx, width.saturating_sub(mw).max(MIN_WIDTH), tail);
                for (r, line) in body.into_iter().enumerate() {
                    let prefix = if r == 0 {
                        Span::styled(marker.clone(), ctx.accent())
                    } else {
                        Span::raw(" ".repeat(mw))
                    };
                    let mut spans = vec![prefix];
                    spans.extend(line.spans.into_iter().map(|s| Span::styled(s.content.into_owned(), s.style)));
                    out.push(Line::from(spans));
                }
            }
            out
        }
        // **Not `─`.** The compaction marker is `─ compacted · 47 turns → summary ─` in accent, and
        // a model-requested rule drawn in the same glyph is one `·` away from forging it. A run of
        // middle dots at weight 3 separates without borrowing the harness's vocabulary — and `─` is
        // reserved anyway, so this is the glyph *and* the reason it is not the other one.
        Block::Rule => vec![Line::from(Span::styled(
            "·".repeat(width),
            ctx.theme.dimmer(),
        ))],
        Block::DisplayMath { src, end } => {
            let inner = width.saturating_sub(2).max(MIN_WIDTH);
            if *end == MathEnd::Eof && live {
                // Still arriving. Reserve the rows the formula will take, so nothing below it moves
                // when it lands, and show only the placeholder.
                // Counted the way the landed block will be drawn (2D rows, or its one-line form
                // word-wrapped), so the estimate and the final height agree for the same source.
                let rows = display_lines(&math::typeset_uncached(src, true), inner, ctx.maths()).len().max(1);
                let mut out = vec![Line::from(vec![
                    Span::raw("  "),
                    Span::styled(MATHS_PLACEHOLDER, ctx.placeholder()),
                ])];
                out.extend((1..rows).map(|_| Line::from("")));
                return out;
            }
            display_lines(&math::display(src), inner, ctx.maths())
        }
        Block::Table { head, rows, source } => layout_table(head, rows, source, ctx, width, live),
    }
}

/// A display formula as indented lines: its 2D rows when they fit, otherwise its one-line form
/// word-wrapped. Never a hard chop through the middle of a 2D layout.
fn display_lines(f: &Formula, inner: usize, style: Style) -> Vec<Line<'static>> {
    if let Some(rows) = clipped_lines(f, inner, style, vec![Span::raw("  ")]) {
        return rows.into_iter().map(Line::from).collect();
    }
    let (lines, two_d) = f.fit(inner);
    let mut out = Vec::new();
    for l in lines {
        if two_d {
            out.push(Line::from(vec![Span::raw("  "), Span::styled(l.clone(), style)]));
            continue;
        }
        for row in wrap_runs(&[Run::plain(l.clone(), style)], inner) {
            let mut spans = vec![Span::raw("  ")];
            spans.extend(row);
            out.push(Line::from(spans));
        }
    }
    out
}

/// A matrix wider than `inner`, as rows cut at the pane's edge with the clip marker on its
/// baseline row in the dim weight, each row after `lead`. `None` when it fits or is not a matrix.
fn clipped_lines(f: &Formula, inner: usize, style: Style, lead: Vec<Span<'static>>) -> Option<Vec<Vec<Span<'static>>>> {
    let (rows, mark) = f.clipped(inner)?;
    let dim = style.add_modifier(Modifier::DIM);
    Some(
        rows.into_iter()
            .enumerate()
            .map(|(k, r)| {
                let mut spans = lead.clone();
                let w = r.width();
                spans.push(Span::styled(r, style));
                if k == mark {
                    spans.push(Span::raw(" ".repeat(inner.saturating_sub(w + 1))));
                    spans.push(Span::styled(math::CLIP_MARKER.to_string(), dim));
                }
                spans
            })
            .collect(),
    )
}

/// Remove the foreground from a style, keeping its modifiers — so quoting a **bold** run keeps the
/// bold and takes the quote's weight.
fn strip_fg(s: Style) -> Style {
    Style {
        fg: None,
        ..s
    }
}

/// A table as aligned columns. **No rules, and no box drawing.**
///
/// §B2's premise is that a border delineates an interactive region. A table drawn with `┌─┬─┐` is a
/// border the model asked for, inside the conversation, complete with a top-left label position —
/// which is exactly the shape of a §B2 region and exactly what must not be forgeable. Columns and
/// a bold header carry the same information and borrow none of it.
///
/// When the columns cannot fit even at their minimum, the **source** is shown instead of a squashed
/// grid. §B11: *a broken grid is worse than an honest refusal*, and this is the same call one scale
/// down.
fn layout_table(
    head: &[String],
    rows: &[Vec<String>],
    source: &[String],
    ctx: &Ctx,
    width: usize,
    live: bool,
) -> Vec<Line<'static>> {
    const GAP: usize = 2;
    const MIN_COL: usize = 6;
    let ncols = head.len();
    if ncols == 0 || ncols * (MIN_COL + GAP) > width + GAP {
        return table_source(source, ctx, width, live);
    }
    let mut widths: Vec<usize> = head.iter().map(|h| cols(h)).collect();
    for r in rows {
        for (i, c) in r.iter().take(ncols).enumerate() {
            widths[i] = widths[i].max(cols(c));
        }
    }
    // Shrink the widest column until it fits. Proportional scaling would round every column into
    // being slightly wrong; taking from the widest keeps narrow columns intact, which is what makes
    // a shrunken table still readable.
    let avail = width;
    let total = |w: &[usize]| w.iter().sum::<usize>() + GAP * (ncols - 1);
    let mut guard = 0;
    while total(&widths) > avail && guard < 10_000 {
        guard += 1;
        let (i, _) = widths
            .iter()
            .enumerate()
            .max_by_key(|(_, w)| **w)
            .expect("cols > 0");
        if widths[i] <= MIN_COL {
            break;
        }
        widths[i] -= 1;
    }
    if total(&widths) > avail {
        return table_source(source, ctx, width, live);
    }

    let mut out = Vec::new();
    let header_style = ctx.base.add_modifier(Modifier::BOLD);
    // Only the row still arriving holds an unclosed formula back.
    out.extend(table_row(head, &widths, ctx, header_style, live && rows.is_empty()));
    for (n, r) in rows.iter().enumerate() {
        out.extend(table_row(r, &widths, ctx, ctx.base, live && n + 1 == rows.len()));
    }
    out
}

/// A table too wide for the pane, as the lines the model wrote -- with its maths typeset, because
/// LaTeX source never reaches the screen, even inside a table that could not be laid out.
fn table_source(source: &[String], ctx: &Ctx, width: usize, live: bool) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(2).max(MIN_WIDTH);
    let mut out = Vec::new();
    for (n, l) in source.iter().enumerate() {
        let runs = inline_maths_only(l, ctx, ctx.code(), live && n + 1 == source.len());
        for row in wrap_runs(&runs, inner) {
            let mut spans = vec![Span::raw("  ")];
            spans.extend(row);
            out.push(Line::from(spans));
        }
    }
    out
}

fn table_row(cells: &[String], widths: &[usize], ctx: &Ctx, style: Style, live: bool) -> Vec<Line<'static>> {
    const GAP: usize = 2;
    // Each cell wraps inside its own column, so a long cell grows the row rather than the table.
    let wrapped: Vec<Vec<Vec<Span<'static>>>> = widths
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let text = cells.get(i).map(String::as_str).unwrap_or("");
            let runs = inline(text, ctx, style, 0, live && i + 1 == widths.len().min(cells.len().max(1)));
            wrap_runs(&runs, *w)
        })
        .collect();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
    let mut out = Vec::new();
    for r in 0..height {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (i, col) in wrapped.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(" ".repeat(GAP)));
            }
            let row = col.get(r);
            let used: usize = row
                .map(|s| s.iter().map(|sp| cols(&sp.content)).sum())
                .unwrap_or(0);
            if let Some(s) = row {
                spans.extend(s.iter().cloned());
            }
            if i + 1 < widths.len() {
                spans.push(Span::raw(" ".repeat(widths[i].saturating_sub(used))));
            }
        }
        out.push(Line::from(spans));
    }
    out
}

/// Split a string into `width`-column chunks without moving anything across a word boundary it did
/// not already cross. Used for code, where re-flowing would misrepresent the source.
fn hard_chunks(s: &str, width: usize) -> Vec<String> {
    let w = width.max(1);
    if s.chars().count() <= w {
        return vec![s.to_string()];
    }
    let chars: Vec<char> = s.chars().collect();
    chars.chunks(w).map(|c| c.iter().collect()).collect()
}

// ── inline ──────────────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Run {
    text: String,
    style: Style,
    /// A multi-row display formula met mid-paragraph. The wrapper breaks the paragraph around it
    /// when it fits, and word-wraps `text` (its one-line form) when it does not.
    block: Option<Arc<Formula>>,
    /// Kept on one line when it fits: an inline formula.
    atomic: bool,
}

impl Run {
    fn plain(text: impl Into<String>, style: Style) -> Run {
        Run { text: text.into(), style, block: None, atomic: false }
    }

    fn formula(text: impl Into<String>, style: Style) -> Run {
        Run { text: text.into(), style, block: None, atomic: true }
    }
}

/// Characters that can **begin** an inline construct. A paragraph containing none of them has no
/// inline markup in it, which is the majority of the prose in any reply.
const INLINE_TRIGGERS: [char; 8] = ['*', '_', '`', '[', '<', '$', '~', '\\'];

fn inline(src: &str, ctx: &Ctx, style: Style, depth: usize, live: bool) -> Vec<Run> {
    // **The fast path, and it is most of the text in most replies.** Without it every paragraph
    // pays for a `Vec<char>` — four bytes per character, allocated and copied — plus a per-
    // character match, on every frame, in order to discover that there was nothing to find.
    if !src.contains(INLINE_TRIGGERS) {
        return vec![Run::plain(src, style)];
    }
    let mut out: Vec<Run> = Vec::new();
    let mut chars: Vec<char> = src.chars().collect();
    if live {
        // The front of a `\(` or `\begin{` still arriving is held back, not shown as text.
        let keep = held_tail(&chars);
        chars.truncate(keep);
    }
    let mut lit = String::new();
    let mut i = 0;
    macro_rules! flush {
        () => {
            if !lit.is_empty() {
                out.push(Run::plain(std::mem::take(&mut lit), style));
            }
        };
    }
    'scan: while i < chars.len() {
        let c = chars[i];
        if depth < MAX_INLINE_DEPTH && !ctx.exhausted() {
            // Maths is tried **before** the escape rule, because `\(` is a delimiter and `(` is
            // punctuation — reading it as an escaped paren would eat the opening of every
            // `\(…\)` expression and leave the closing one on screen.
            if opens_maths(&chars, i) {
                if let Some((runs, next)) = maths(&chars, i, ctx, style, live) {
                    flush!();
                    out.extend(runs);
                    i = next;
                    continue 'scan;
                }
            }
        }
        // A symbol command with no delimiters around it (`the rate \eta is small`) is still LaTeX,
        // and LaTeX never reaches the screen as source. Only where a path could not be meant:
        // `C:\theta`, `dir\alpha` and `./\beta` are left alone.
        if c == '\\' && depth < MAX_INLINE_DEPTH {
            let k = chars[i + 1..].iter().take_while(|c| c.is_ascii_alphabetic()).count();
            let before_ok = i == 0 || !(chars[i - 1].is_alphanumeric() || matches!(chars[i - 1], ':' | '\\' | '/' | '.' | '~' | '_'));
            let after_ok = chars.get(i + 1 + k).is_none_or(|c| !(c.is_alphanumeric() || matches!(c, '{' | '_' | '^' | '\\' | '/' | ':')));
            if k > 0 && before_ok && after_ok {
                let name: String = chars[i + 1..i + 1 + k].iter().collect();
                if let Some(g) = math::bare_symbol(&name) {
                    flush!();
                    out.push(Run::formula(g, ctx.maths().add_modifier(style.add_modifier)));
                    i += 1 + k;
                    continue 'scan;
                }
            }
        }
        // A backslash escape is markdown's own way of saying "this character is literal", and
        // honouring it is what lets a model write `\*` and mean an asterisk.
        if c == '\\' && i + 1 < chars.len() && is_punct(chars[i + 1]) {
            lit.push(chars[i + 1]);
            i += 2;
            continue 'scan;
        }
        if depth < MAX_INLINE_DEPTH && !ctx.exhausted() {
            if c == '`' {
                match code_span(&chars, i, ctx) {
                    Ok((body, next)) => {
                        flush!();
                        out.push(Run::plain(body, ctx.code()));
                        i = next;
                        continue 'scan;
                    }
                    Err(ticks) => {
                        // Skip the whole unclosed run. Advancing one character would re-count it.
                        for _ in 0..ticks {
                            lit.push('`');
                        }
                        i += ticks;
                        continue 'scan;
                    }
                }
            }
            if c == '!' || c == '[' {
                if let Some((runs, next)) = link(&chars, i, ctx, style, depth) {
                    flush!();
                    out.extend(runs);
                    i = next;
                    continue 'scan;
                }
            }
            if c == '<' {
                if let Some((runs, next)) = autolink(&chars, i, ctx, style) {
                    flush!();
                    out.extend(runs);
                    i = next;
                    continue 'scan;
                }
            }
            // Longest delimiter first: `***x***` must not be read as `*` opening on `**x**`.
            for (delim, modifier) in [
                ("***", Modifier::BOLD | Modifier::ITALIC),
                ("___", Modifier::BOLD | Modifier::ITALIC),
                ("**", Modifier::BOLD),
                ("__", Modifier::BOLD),
                ("~~", Modifier::CROSSED_OUT),
                ("*", Modifier::ITALIC),
                ("_", Modifier::ITALIC),
            ] {
                if !delim.starts_with(c) {
                    continue;
                }
                if let Some((inner, next)) = delimited(&chars, i, delim, ctx) {
                    flush!();
                    out.extend(inline(&inner, ctx, style.add_modifier(modifier), depth + 1, false));
                    i = next;
                    continue 'scan;
                }
            }
        }
        lit.push(c);
        i += 1;
    }
    flush!();
    out
}

fn is_punct(c: char) -> bool {
    "\\`*_{}[]()#+-.!|~<>$".contains(c)
}

/// A delimited run — `**bold**`. Returns the inner text and the index just past the closing run.
///
/// The flanking rules are what stop `snake_case_names` becoming italic and `2 * 3 * 4` becoming a
/// bold multiplication: an opening delimiter must be followed by a non-space, a closing one must be
/// preceded by a non-space, and `_` additionally must sit at a word boundary on the outside.
fn delimited(chars: &[char], i: usize, delim: &str, ctx: &Ctx) -> Option<(String, usize)> {
    let d: Vec<char> = delim.chars().collect();
    let n = d.len();
    if chars.len() < i + n || chars[i..i + n] != d[..] {
        return None;
    }
    // Not a longer run of the same character: `***x***` must not be seen as `*` + `**x**`.
    if chars.get(i + n) == Some(&d[0]) {
        return None;
    }
    let after = *chars.get(i + n)?;
    if after.is_whitespace() {
        return None;
    }
    if d[0] == '_' {
        let before = if i == 0 { ' ' } else { chars[i - 1] };
        if before.is_alphanumeric() {
            return None;
        }
    }
    let mut j = i + n;
    while j + n <= chars.len() {
        if !ctx.spend(1) {
            return None;
        }
        if chars[j..j + n] == d[..] && chars.get(j + n) != Some(&d[0]) {
            let before = chars[j - 1];
            if before.is_whitespace() {
                j += 1;
                continue;
            }
            if d[0] == '_' {
                let outside = chars.get(j + n).copied().unwrap_or(' ');
                if outside.is_alphanumeric() {
                    j += 1;
                    continue;
                }
            }
            let inner: String = chars[i + n..j].iter().collect();
            if inner.is_empty() {
                return None;
            }
            return Some((inner, j + n));
        }
        j += 1;
    }
    None
}

/// A code span, and the index just past it.
///
/// `Err(ticks)` rather than `None` on failure: a run of backticks with no closer must be **skipped
/// whole**, not retried one character in. Retrying is what turns a line of backticks into a
/// quadratic — the tick count itself is a scan, so each retry re-counts nearly the whole run.
fn code_span(chars: &[char], i: usize, ctx: &Ctx) -> Result<(String, usize), usize> {
    let ticks = chars[i..].iter().take_while(|c| **c == '`').count();
    if !ctx.spend(ticks) {
        return Err(ticks);
    }
    let mut j = i + ticks;
    while j < chars.len() {
        if !ctx.spend(1) {
            return Err(ticks);
        }
        if chars[j] == '`' {
            let run = chars[j..].iter().take_while(|c| **c == '`').count();
            if run == ticks {
                let inner: String = chars[i + ticks..j].iter().collect();
                if inner.is_empty() {
                    return Err(ticks);
                }
                return Ok((inner.trim_matches(' ').to_string(), j + run));
            }
            j += run;
            continue;
        }
        j += 1;
    }
    Err(ticks)
}

/// `[text](url)` and `![alt](url)`.
///
/// # The target is always shown, and that is a security decision rather than a style one
///
/// A link is the one markdown construct that lets the author make text `X` point somewhere the
/// reader cannot see. In a terminal there is nothing to click, so hiding the target buys nothing
/// and costs the reader the only fact that matters about it. `[the docs](http://evil.example)`
/// renders as `the docs (http://evil.example)`, in weight 2, always.
fn link(
    chars: &[char],
    i: usize,
    ctx: &Ctx,
    style: Style,
    depth: usize,
) -> Option<(Vec<Run>, usize)> {
    // Both scans below are charged. `[[[[[…` is a line of openers, each of which would otherwise
    // walk to the end of the block looking for its `]`.
    let image = chars[i] == '!';
    let open = if image { i + 1 } else { i };
    if chars.get(open) != Some(&'[') {
        return None;
    }
    let mut j = open + 1;
    let mut depth_b = 1;
    while j < chars.len() && depth_b > 0 {
        if !ctx.spend(1) {
            return None;
        }
        match chars[j] {
            '[' => depth_b += 1,
            ']' => depth_b -= 1,
            _ => {}
        }
        if depth_b > 0 {
            j += 1;
        }
    }
    if depth_b != 0 || chars.get(j + 1) != Some(&'(') {
        return None;
    }
    let text: String = chars[open + 1..j].iter().collect();
    let mut k = j + 2;
    let mut paren = 1;
    while k < chars.len() && paren > 0 {
        if !ctx.spend(1) {
            return None;
        }
        match chars[k] {
            '(' => paren += 1,
            ')' => paren -= 1,
            _ => {}
        }
        if paren > 0 {
            k += 1;
        }
    }
    if paren != 0 {
        return None;
    }
    let target: String = chars[j + 2..k].iter().collect();
    let target = target.split_whitespace().next().unwrap_or("").to_string();
    let mut runs = Vec::new();
    if !text.is_empty() {
        runs.extend(inline(
            &text,
            ctx,
            style.add_modifier(Modifier::UNDERLINED),
            depth + 1,
            false,
        ));
    }
    if !target.is_empty() && target != text {
        if !runs.is_empty() {
            runs.push(Run::plain(" ", style));
        }
        runs.push(Run::plain(format!("({target})"), ctx.theme.dim()));
    } else if runs.is_empty() {
        runs.push(Run::plain(target, ctx.theme.dim()));
    }
    Some((runs, k + 1))
}

fn autolink(chars: &[char], i: usize, ctx: &Ctx, style: Style) -> Option<(Vec<Run>, usize)> {
    let close = chars[i..].iter().position(|c| *c == '>')? + i;
    if !ctx.spend(close - i) {
        return None;
    }
    let inner: String = chars[i + 1..close].iter().collect();
    let is_url = ["http://", "https://", "mailto:", "ftp://"]
        .iter()
        .any(|s| inner.starts_with(s));
    if !is_url || inner.contains(' ') {
        return None;
    }
    Some((
        vec![Run::plain(inner, style.patch(ctx.theme.dim()).add_modifier(Modifier::UNDERLINED))],
        close + 1,
    ))
}

/// `$…$`, `\(…\)`, and display spans met mid-paragraph: `$$…$$`, `\[…\]`.
///
/// # Never the source
///
/// A closed span is typeset by [`crate::math`], which never returns source: it parses, or repairs
/// and parses, or paraphrases. An **unclosed** span is decided here:
///
/// | unclosed span | streaming tail | finished reply |
/// |---|---|---|
/// | `$…` that could still be maths | placeholder | literal: it was a dollar sign |
/// | `$…` that has turned into prose | literal | literal |
/// | `\(…`, `$$…`, `\[…` | placeholder | typeset as if closed: the closer was the slip |
///
/// A span never crosses a hard line break, so an unpaired `$` cannot swallow the rest of a reply.
fn maths(chars: &[char], i: usize, ctx: &Ctx, style: Style, live: bool) -> Option<(Vec<Run>, usize)> {
    // A maths environment met mid-paragraph: `… we get \begin{pmatrix}…\end{pmatrix}.` The span
    // keeps its `\begin`/`\end`, which are the formula.
    let env_close: Vec<char>;
    let begin_env = chars[i] == '\\' && chars.get(i + 1) == Some(&'b');
    if begin_env {
        let rest: String = chars[i..chars.len().min(i + 40)].iter().collect();
        let env = math_env_open(&rest)?;
        env_close = format!("\\end{{{env}}}").chars().collect();
    } else {
        env_close = Vec::new();
    }
    // Two dollars is the longer delimiter and must be tried first, or `$$x$$` reads as an empty
    // `$$` span and then `x` between two stray dollars.
    let (open_len, close, display): (usize, &[char], bool) = match (chars[i], chars.get(i + 1)) {
        _ if begin_env => (0, &env_close, true),
        ('$', Some('$')) => (2, &['$', '$'], true),
        ('$', _) => (1, &['$'], false),
        (_, Some('[')) => (2, &['\\', ']'], true),
        _ => (2, &['\\', ')'], false),
    };
    let start = i + open_len;
    // An environment's `\begin{…}` is skipped before its `\end` is looked for.
    let mut j = if begin_env { start + 7 } else { start };
    let mut unterminated = false;
    let end = loop {
        if !ctx.spend(1) {
            return None;
        }
        if j + close.len() > chars.len() {
            unterminated = true;
            break chars.len();
        }
        if chars[j..j + close.len()] == *close {
            // `\$` inside a span is an escaped dollar, not the closer: `$\$5$`.
            let slashes = chars[start..j].iter().rev().take_while(|c| **c == '\\').count();
            if close[0] != '$' || slashes % 2 == 0 {
                break j;
            }
        }
        if chars[j] == '\n' {
            return None;
        }
        j += 1;
    };
    let mut content: String = chars[start.min(end)..end].iter().collect();
    if begin_env && !unterminated {
        content.extend(close.iter());
    }
    let single_dollar = open_len == 1;
    let mut consumed = if unterminated { chars.len() } else { end + close.len() };
    if unterminated {
        // The first character of the closer arrives a token before the second.
        while !begin_env && content.ends_with(close[0]) {
            content.pop();
        }
        if live {
            let still_maths = if single_dollar {
                math::could_become_inline_maths(&content)
            } else {
                !prose_like(&content)
            };
            if !still_maths {
                return None;
            }
            if display {
                // Reserve the rows the formula will take when it lands, as a block does, so the
                // paragraph does not grow under the reader when the closer arrives.
                let rows = math::typeset_uncached(&content, true).rows.len().max(1);
                if rows > 1 {
                    let mut held = vec![MATHS_PLACEHOLDER.to_string()];
                    held.resize(rows, String::new());
                    let f = Formula {
                        rows: held,
                        width: 1,
                        linear: vec![MATHS_PLACEHOLDER.to_string()],
                        fidelity: math::Fidelity::Paraphrased,
                        typst: None,
                        clip_row: None,
                    };
                    let run = Run { text: MATHS_PLACEHOLDER.into(), style: ctx.placeholder(), block: Some(Arc::new(f)), atomic: false };
                    return Some((vec![run], chars.len()));
                }
            }
            return Some((vec![Run::plain(MATHS_PLACEHOLDER, ctx.placeholder())], chars.len()));
        }
        // Finished, and never closed. A lone `$` before prose or a number was a dollar sign; one
        // before a command was maths whose closer went missing, and it runs to where the prose
        // starts again. An unclosed `\[` with nothing after it was an escaped bracket, and an
        // empty `$$` is dropped; anything else was maths whose closer went missing.
        if single_dollar {
            let n = math::unclosed_inline_extent(&content)?;
            content = content.chars().take(n).collect();
            consumed = start + n;
        } else if content.trim().is_empty() {
            if open_len == 2 && chars[i] == '$' {
                return Some((Vec::new(), chars.len()));
            }
            return None;
        }
    } else if single_dollar && !math::looks_like_inline_maths(&content) {
        return None;
    }
    // Lighter than the prose around it -- see `Ctx::maths` -- and keeping any emphasis it sits in.
    let st = ctx.maths().add_modifier(style.add_modifier);
    if display {
        let f = math::display(&content);
        if f.rows.len() > 1 {
            let text = f.linear.join("  ");
            return Some((vec![Run { text, style: st, block: Some(f), atomic: false }], consumed));
        }
        return Some((vec![Run::formula(f.rows[0].clone(), st)], consumed));
    }
    let f = math::inline(&content);
    Some((vec![Run::formula(f.rows[0].clone(), st)], consumed))
}

/// Whether an unclosed `\(`, `\[` or `$$` has turned out to be prose, which means it was never a
/// delimiter: three or more words of four letters or more, outside commands and braces.
fn prose_like(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    let (mut i, mut depth, mut words) = (0, 0usize, 0);
    while i < chars.len() {
        match chars[i] {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            '\\' => {
                i += 1;
                while i < chars.len() && chars[i].is_ascii_alphabetic() {
                    i += 1;
                }
                continue;
            }
            c if c.is_ascii_alphabetic() => {
                let st = i;
                while i < chars.len() && chars[i].is_ascii_alphabetic() {
                    i += 1;
                }
                if depth == 0 && i - st >= 4 {
                    words += 1;
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    words >= 3
}

// ── wrapping ────────────────────────────────────────────────────────────────────────────────────

/// Wrap styled runs to `width`, breaking at spaces and hard-splitting a word that cannot fit.
///
/// Wrapping happens **here** and not in `Paragraph`, for the reason `render::transcript_lines`
/// already documents: the scrollbar needs the true line count, and a scrollbar sized from unwrapped
/// lines lies by exactly the amount of prose on screen.
fn wrap_runs(runs: &[Run], width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(MIN_WIDTH);
    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    let mut row = RowBuilder::default();
    let mut pending_space: Option<Style> = None;

    for run in runs {
        // A multi-row formula mid-paragraph is set off on its own rows, as display maths is.
        if let Some(f) = &run.block {
            if let Some(clipped) = clipped_lines(f, width.saturating_sub(2), run.style, vec![Span::raw("  ")]) {
                if row.col > 0 {
                    rows.push(row.finish());
                }
                pending_space = None;
                rows.extend(clipped);
                continue;
            }
            let (lines, two_d) = f.fit(width.saturating_sub(2));
            if two_d {
                if row.col > 0 {
                    rows.push(row.finish());
                }
                pending_space = None;
                for l in lines {
                    rows.push(vec![Span::raw("  "), Span::styled(l.clone(), run.style)]);
                }
                continue;
            }
        }
        // Inline maths moves to the next line whole rather than breaking at its own spaces, unless
        // it is wider than the pane.
        let toks = if run.atomic && cols(&run.text) <= width {
            vec![Token::Word(run.text.clone())]
        } else {
            tokenize(&run.text)
        };
        for tok in toks {
            match tok {
                Token::Space => {
                    if row.col > 0 {
                        pending_space = Some(run.style);
                    }
                }
                Token::Word(word) => {
                    let mut rest = word;
                    loop {
                        let len = cols(&rest);
                        let sp = usize::from(pending_space.is_some() && row.col > 0);
                        if row.col + sp + len <= width {
                            if let Some(st) = pending_space.take().filter(|_| sp == 1) {
                                row.push(" ", st);
                            }
                            pending_space = None;
                            row.push(&rest, run.style);
                            break;
                        }
                        if row.col > 0 {
                            rows.push(row.finish());
                            pending_space = None;
                            continue;
                        }
                        // At column zero and still too long: a URL, a hash, a path. Hard-split
                        // rather than overflow the pane — the alternative is a line the renderer
                        // believes is `width` wide and the terminal truncates, which puts the
                        // scrollbar's arithmetic and the screen out of agreement. Split by columns,
                        // and never between a letter and the accent combined onto it.
                        let (head, tail) = split_at_cols(&rest, width);
                        row.push(&head, run.style);
                        rows.push(row.finish());
                        rest = tail;
                        if rest.is_empty() {
                            break;
                        }
                    }
                }
            }
        }
    }
    let last = row.finish();
    if !last.is_empty() || rows.is_empty() {
        rows.push(last);
    }
    rows
}

/// The longest prefix of `s` at most `width` columns wide (at least one character), and the rest.
fn split_at_cols(s: &str, width: usize) -> (String, String) {
    let mut w = 0;
    let mut cut = s.len();
    for (i, c) in s.char_indices() {
        let cw = c.width().unwrap_or(0);
        if cw > 0 && w + cw > width && w > 0 {
            cut = i;
            break;
        }
        w += cw;
    }
    (s[..cut].to_string(), s[cut..].to_string())
}

/// One wrapped row, **coalescing adjacent text of the same style into one `Span`**.
///
/// # Why this is not a micro-optimisation
///
/// A span per word is a `String` allocation per word, on every frame, for the whole transcript —
/// `transcript_lines` lays out everything in order to hand the scrollbar an honest line count.
/// `tests/markdown_cost.rs` measured the difference against the flat path it replaced: word-level
/// spans were **8.9× the flat renderer** on a full context window of prose, and almost all of the
/// gap was allocation rather than parsing. A paragraph is one style, so coalescing brings it back
/// to roughly one span per line, which is what the flat path always produced.
///
/// It is also fewer diffs for ratatui to walk, which is the thing K4's flicker target is measured
/// against.
#[derive(Default)]
struct RowBuilder {
    spans: Vec<Span<'static>>,
    pending: String,
    style: Style,
    col: usize,
}

impl RowBuilder {
    fn push(&mut self, text: &str, style: Style) {
        if !self.pending.is_empty() && style != self.style {
            self.flush();
        }
        self.style = style;
        self.pending.push_str(text);
        self.col += cols(text);
    }

    fn flush(&mut self) {
        if !self.pending.is_empty() {
            self.spans
                .push(Span::styled(std::mem::take(&mut self.pending), self.style));
        }
    }

    fn finish(&mut self) -> Vec<Span<'static>> {
        self.flush();
        self.col = 0;
        std::mem::take(&mut self.spans)
    }
}

enum Token {
    Space,
    Word(String),
}

fn tokenize(s: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut word = String::new();
    for c in s.chars() {
        if c.is_whitespace() {
            if !word.is_empty() {
                out.push(Token::Word(std::mem::take(&mut word)));
            }
            out.push(Token::Space);
        } else {
            word.push(c);
        }
    }
    if !word.is_empty() {
        out.push(Token::Word(word));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> Theme {
        Theme::default_truecolor()
    }

    fn text_of(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn render(src: &str, width: usize) -> Vec<Line<'static>> {
        let theme = t();
        render_prose(src, width, &theme, Style::default())
    }

    #[test]
    fn a_heading_loses_its_hashes_and_takes_the_accent() {
        let theme = t();
        let lines = render_prose("# Retrieval", 40, &theme, Style::default());
        assert_eq!(text_of(&lines), vec!["Retrieval".to_string()]);
        assert_eq!(lines[0].spans[0].style.fg, Some(theme.accent()));
        assert!(lines[0].spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn emphasis_and_strong_become_attributes_not_colours() {
        let theme = t();
        let base = Style::default().fg(theme.speech());
        let lines = render_prose("plain **strong** and *em* and `code`", 60, &theme, base);
        let spans = &lines[0].spans;
        let find = |needle: &str| spans.iter().find(|s| s.content.contains(needle)).unwrap();
        assert!(find("strong").style.add_modifier.contains(Modifier::BOLD));
        assert!(find("em").style.add_modifier.contains(Modifier::ITALIC));
        // Code takes foreground weight 1, which is a declared colour and not a fourth state one.
        assert_eq!(find("code").style.fg, Some(Color::Reset));
        // And the delimiters are gone.
        let joined = text_of(&lines).join("");
        assert!(!joined.contains('*') && !joined.contains('`'), "{joined}");
    }

    #[test]
    fn no_span_this_module_produces_carries_a_background_or_reverse_video() {
        // REVERSED is the one that would pass §B13's `cell.bg == Reset` walk and still paint a
        // solid slab. See the module header.
        let theme = t();
        let src = "# H\n\ntext **b** *i* `c`\n\n```\nfenced\n```\n\n> quote\n\n- a\n- b\n\n---\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
        for line in render_prose(src, 60, &theme, Style::default()) {
            for s in &line.spans {
                assert_eq!(s.style.bg, None, "{:?} carries a background", s.content);
                assert!(
                    !s.style.add_modifier.contains(Modifier::REVERSED),
                    "{:?} is reverse video, which is a background fill by another name",
                    s.content
                );
            }
        }
    }

    #[test]
    fn a_list_keeps_its_items_apart_and_nests() {
        let lines = render("- one\n- two\n  - nested\n", 40);
        let text = text_of(&lines);
        assert_eq!(text[0], "• one");
        assert_eq!(text[1], "• two");
        assert_eq!(text[2], "  • nested");
    }

    #[test]
    fn an_ordered_list_counts_from_where_it_started() {
        let text = text_of(&render("3. three\n4. four\n", 40));
        assert_eq!(text[0], "3. three");
        assert_eq!(text[1], "4. four");
    }

    #[test]
    fn code_is_hard_wrapped_and_never_reflowed() {
        // Re-flowing a code line moves tokens across lines and changes what the reader believes the
        // program says.
        let long = "let x = some_function(argument_one, argument_two, argument_three);";
        let text = text_of(&render(&format!("```\n{long}\n```"), 30));
        let joined: String = text.join("").replace(' ', "");
        assert!(joined.contains("some_function(argument_one,"), "{text:?}");
    }

    #[test]
    fn a_link_always_shows_where_it_goes() {
        let text = text_of(&render("see [the docs](https://evil.example/x)", 80)).join("");
        assert!(text.contains("the docs"), "{text}");
        assert!(
            text.contains("https://evil.example/x"),
            "a link that hides its target is a phishing primitive: {text}"
        );
    }

    #[test]
    fn a_paragraph_wraps_and_the_line_count_is_the_renderers_answer() {
        let words = "alpha beta gamma delta epsilon zeta eta theta".repeat(3);
        let lines = render(&words, 24);
        assert!(lines.len() > 1);
        for l in &lines {
            let w: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
            assert!(w <= 24, "a wrapped line is {w} columns wide at width 24: {l:?}");
        }
    }

    #[test]
    fn a_very_long_word_is_split_rather_than_overflowing_the_pane() {
        let lines = render(&"x".repeat(200), 30);
        for l in &lines {
            let w: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
            assert!(w <= 30, "{w} columns at width 30");
        }
        let joined: String = text_of(&lines).join("");
        assert_eq!(joined.chars().filter(|c| *c == 'x').count(), 200, "content was lost");
    }

    #[test]
    fn snake_case_and_arithmetic_are_not_emphasis() {
        // The flanking rules, and the two sentences that made them necessary.
        let a = text_of(&render("call get_user_name to fetch it", 60)).join("");
        assert!(a.contains("get_user_name"), "{a}");
        let b = text_of(&render("2 * 3 * 4 is twenty-four", 60)).join("");
        assert!(b.contains("2 * 3 * 4"), "{b}");
    }

    #[test]
    fn a_table_is_columns_and_carries_no_box_drawing() {
        let src = "| tool | lines |\n|---|---|\n| read | 48 |\n| edit | 3 |\n";
        let text = text_of(&render(src, 60));
        let joined = text.join("\n");
        for c in joined.chars() {
            assert!(
                !('\u{2500}'..='\u{257F}').contains(&c),
                "a table drew {c:?}; a border inside the conversation is a §B2 region the model \
                 asked for"
            );
        }
        assert!(joined.contains("tool"), "{joined}");
        assert!(joined.contains("read"), "{joined}");
        assert!(!joined.contains('|'), "the pipes should be gone: {joined}");
    }

    #[test]
    fn a_table_too_wide_for_the_pane_is_shown_as_source_not_squashed() {
        let src = "| a | b | c | d | e | f | g | h |\n|---|---|---|---|---|---|---|---|\n| 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 |\n";
        let text = text_of(&render(src, 20)).join("\n");
        assert!(
            text.contains('|'),
            "a table that cannot fit must be shown as what the model wrote — §B11's instinct one \
             scale down: {text}"
        );
    }

    #[test]
    fn a_horizontal_rule_is_not_the_compaction_marker() {
        let text = text_of(&render("above\n\n---\n\nbelow", 40)).join("\n");
        assert!(!text.contains('─'), "a model-requested rule drew the compaction glyph: {text}");
        assert!(text.contains('·'), "{text}");
    }

    #[test]
    fn maths_always_renders_and_never_shows_its_source() {
        let a = text_of(&render(r"the area is $\pi r^2$ exactly", 60)).join("");
        assert!(a.contains("πr²"), "{a}");
        // **Inverted on 2026-10-09 by Matthew's requirement: LaTeX never renders raw.** This used
        // to assert that `$\hat{x}$` stayed visibly its source, because a combining accent is zero
        // columns wide. The wrap arithmetic now counts display columns, so the accent is drawn on
        // its letter and no expression is left showing as source.
        let b = text_of(&render(r"consider $\hat{x}$ here", 80)).join("");
        assert!(b.contains("x\u{302}"), "{b}");
        assert!(!b.contains('$') && !b.contains('\\'), "source reached the pane: {b}");
    }

    #[test]
    fn display_maths_over_several_lines_is_one_typeset_block() {
        let src = "Then\n$$\n\\frac{a}{b}\n= c\n$$\nso.";
        let text = text_of(&render(src, 60));
        let joined = text.join("\n");
        assert!(joined.contains('⎯'), "the fraction did not stack:\n{joined}");
        assert!(!joined.contains('$') && !joined.contains('\\'), "{joined}");
        assert!(text[0].contains("Then") && joined.contains("so."), "{joined}");
    }

    #[test]
    fn a_price_is_not_an_equation() {
        let text = text_of(&render("it costs $5 and $10 with tax", 60)).join("");
        assert_eq!(text, "it costs $5 and $10 with tax");
    }

    #[test]
    fn a_block_quote_takes_the_rule_and_the_second_weight() {
        let theme = t();
        let lines = render_prose("> quoted\n", 40, &theme, Style::default());
        let text = text_of(&lines);
        assert!(text[0].starts_with(crate::chrome::QUOTE_RULE), "{text:?}");
        assert!(text[0].contains("quoted"));
        let body = lines[0].spans.iter().find(|s| s.content.contains("quoted")).unwrap();
        assert_eq!(body.style.fg, Some(theme.tone(marlowe_view::Tone::Dim)));
    }

    #[test]
    fn rendering_is_a_pure_function_of_its_arguments() {
        // K4's flicker rows depend on this: frame N+1 must be a re-render of frame N, cell for
        // cell. A cache keyed on anything that moves would break it silently.
        let src = "# Head\n\nbody **bold** `code`\n\n- a\n- b\n";
        let theme = t();
        let a = render_prose(src, 44, &theme, Style::default());
        let b = render_prose(src, 44, &theme, Style::default());
        assert_eq!(text_of(&a), text_of(&b));
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.spans.len(), y.spans.len());
            for (p, q) in x.spans.iter().zip(y.spans.iter()) {
                assert_eq!(p.style, q.style);
            }
        }
    }

    #[test]
    fn plain_prose_survives_untouched() {
        // The commonest reply in the product is a paragraph with no markup in it at all, and the
        // renderer must not be detectable in that case.
        let src = "I read the Dockerfile. The pinned version was dropped when it was rebuilt.";
        assert_eq!(text_of(&render(src, 100)), vec![src.to_string()]);
    }
}
