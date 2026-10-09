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
//! pure. **There is no cache here and no memoisation keyed on anything.** Parsing happens on every
//! frame, which is also what makes the scrollbar honest: `render::transcript_lines` is the only
//! thing that knows how many wrapped lines a transcript produced, and it hands that count back to
//! `App::scroll_max` (see `render::draw_conversation`). A cached line count and a freshly wrapped
//! one would be two answers to that question.
//!
//! The cost is measured rather than argued: `tests/markdown_cost.rs` lays out a large transcript
//! against K4's 150 ms first-frame budget.
//!
//! # The model authors this text
//!
//! See [`crate::chrome`]. Interpreting markup hands the model control of visual structure, and the
//! mitigation is structural: harness glyphs are refused in model prose, the horizontal rule is
//! drawn in a glyph the compaction marker does not use, and tables are laid out with no rules at
//! all so that a §B2 border cannot be assembled out of one.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::theme::Theme;

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
    layout_blocks(&blocks, &ctx, width.max(MIN_WIDTH))
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
    /// Display maths on its own line — `$$…$$` or `\[…\]`. Held as source; the decision to render
    /// or refuse is [`crate::latex`]'s and is made at layout.
    DisplayMath(String),
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
            let mut body = Vec::new();
            i += 1;
            while i < lines.len() && !fence_closes(lines[i], &fence) {
                body.push(lines[i].to_string());
                i += 1;
            }
            if i < lines.len() {
                i += 1; // the closing fence
            }
            out.push(Block::Code(body));
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
        if let Some(inner) = display_math(line) {
            out.push(Block::DisplayMath(inner));
            i += 1;
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

fn display_math(line: &str) -> Option<String> {
    let t = line.trim();
    for (open, close) in [("$$", "$$"), ("\\[", "\\]")] {
        if let Some(rest) = t.strip_prefix(open) {
            if let Some(inner) = rest.strip_suffix(close) {
                if !inner.trim().is_empty() {
                    return Some(t.to_string());
                }
            }
        }
    }
    None
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

fn split_row(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|').map(|c| c.trim().to_string()).collect()
}

// ── layout ──────────────────────────────────────────────────────────────────────────────────────

fn layout_blocks(blocks: &[Block], ctx: &Ctx, width: usize) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    for (n, b) in blocks.iter().enumerate() {
        // One blank line between blocks — except a list that follows its own lead-in sentence,
        // which is one thing and reads as one thing. This is also what makes a nested list sit
        // directly under its parent item instead of a row below it.
        let tight = n > 0 && matches!(b, Block::List { .. }) && matches!(blocks[n - 1], Block::Para(_));
        if n > 0 && !tight {
            out.push(Line::from(""));
        }
        out.extend(layout_block(b, ctx, width));
    }
    out
}

fn layout_block(block: &Block, ctx: &Ctx, width: usize) -> Vec<Line<'static>> {
    match block {
        Block::Heading { level, text } => {
            // Accent is §B2's structure role. Bold at the top two levels gives three visible ranks
            // — bold accent, accent, prose — without a second colour.
            let mut style = ctx.accent();
            if *level <= 2 {
                style = style.add_modifier(Modifier::BOLD);
            }
            let runs = inline(text, ctx, style, 0);
            wrap_runs(&runs, width)
                .into_iter()
                .map(Line::from)
                .collect()
        }
        Block::Para(text) => {
            let mut out = Vec::new();
            for hard in text.split('\n') {
                let runs = inline(hard, ctx, ctx.base, 0);
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
            let body = layout_blocks(inner, ctx, width.saturating_sub(2).max(MIN_WIDTH));
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
                let body = layout_blocks(item, ctx, width.saturating_sub(mw).max(MIN_WIDTH));
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
        Block::DisplayMath(src) => {
            let rendered = crate::latex::render_or_source(src);
            let style = match &rendered {
                // **`maths()`, not `base`** -- the same weight the INLINE path gives an equation.
                // Display maths took the surrounding prose's style, so `$x$` stood out and
                // `$$x$$` did not, which is backwards: the display form is the one the author
                // decided was important enough to put on its own line.
                std::borrow::Cow::Owned(_) => ctx.maths(),
                // Refused: this is source, and it is styled as source so the reader can see that.
                std::borrow::Cow::Borrowed(_) => ctx.code(),
            };
            let inner = width.saturating_sub(2).max(MIN_WIDTH);
            hard_chunks(&rendered, inner)
                .into_iter()
                .map(|c| Line::from(vec![Span::raw("  "), Span::styled(c, style)]))
                .collect()
        }
        Block::Table { head, rows, source } => layout_table(head, rows, source, ctx, width),
    }
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
) -> Vec<Line<'static>> {
    const GAP: usize = 2;
    const MIN_COL: usize = 6;
    let cols = head.len();
    if cols == 0 || cols * (MIN_COL + GAP) > width + GAP {
        return layout_block(&Block::Code(source.to_vec()), ctx, width);
    }
    let mut widths: Vec<usize> = head.iter().map(|h| h.chars().count()).collect();
    for r in rows {
        for (i, c) in r.iter().take(cols).enumerate() {
            widths[i] = widths[i].max(c.chars().count());
        }
    }
    // Shrink the widest column until it fits. Proportional scaling would round every column into
    // being slightly wrong; taking from the widest keeps narrow columns intact, which is what makes
    // a shrunken table still readable.
    let avail = width;
    let total = |w: &[usize]| w.iter().sum::<usize>() + GAP * (cols - 1);
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
        return layout_block(&Block::Code(source.to_vec()), ctx, width);
    }

    let mut out = Vec::new();
    let header_style = ctx.base.add_modifier(Modifier::BOLD);
    out.extend(table_row(head, &widths, ctx, header_style));
    for r in rows {
        out.extend(table_row(r, &widths, ctx, ctx.base));
    }
    out
}

fn table_row(cells: &[String], widths: &[usize], ctx: &Ctx, style: Style) -> Vec<Line<'static>> {
    const GAP: usize = 2;
    // Each cell wraps inside its own column, so a long cell grows the row rather than the table.
    let wrapped: Vec<Vec<Vec<Span<'static>>>> = widths
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let text = cells.get(i).map(String::as_str).unwrap_or("");
            let runs = inline(text, ctx, style, 0);
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
                .map(|s| s.iter().map(|sp| sp.content.chars().count()).sum())
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
}

/// Characters that can **begin** an inline construct. A paragraph containing none of them has no
/// inline markup in it, which is the majority of the prose in any reply.
const INLINE_TRIGGERS: [char; 8] = ['*', '_', '`', '[', '<', '$', '~', '\\'];

fn inline(src: &str, ctx: &Ctx, style: Style, depth: usize) -> Vec<Run> {
    // **The fast path, and it is most of the text in most replies.** Without it every paragraph
    // pays for a `Vec<char>` — four bytes per character, allocated and copied — plus a per-
    // character match, on every frame, in order to discover that there was nothing to find.
    if !src.contains(INLINE_TRIGGERS) {
        return vec![Run {
            text: src.to_string(),
            style,
        }];
    }
    let mut out: Vec<Run> = Vec::new();
    let chars: Vec<char> = src.chars().collect();
    let mut lit = String::new();
    let mut i = 0;
    macro_rules! flush {
        () => {
            if !lit.is_empty() {
                out.push(Run {
                    text: std::mem::take(&mut lit),
                    style,
                });
            }
        };
    }
    'scan: while i < chars.len() {
        let c = chars[i];
        if depth < MAX_INLINE_DEPTH && !ctx.exhausted() {
            // Maths is tried **before** the escape rule, because `\(` is a delimiter and `(` is
            // punctuation — reading it as an escaped paren would eat the opening of every
            // `\(…\)` expression and leave the closing one on screen.
            if c == '$' || (c == '\\' && chars.get(i + 1) == Some(&'(')) {
                if let Some((runs, next)) = maths(&chars, i, ctx, style) {
                    flush!();
                    out.extend(runs);
                    i = next;
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
                        out.push(Run {
                            text: body,
                            style: ctx.code(),
                        });
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
                    out.extend(inline(&inner, ctx, style.add_modifier(modifier), depth + 1));
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
        ));
    }
    if !target.is_empty() && target != text {
        if !runs.is_empty() {
            runs.push(Run {
                text: " ".into(),
                style,
            });
        }
        runs.push(Run {
            text: format!("({target})"),
            style: ctx.theme.dim(),
        });
    } else if runs.is_empty() {
        runs.push(Run {
            text: target,
            style: ctx.theme.dim(),
        });
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
        vec![Run {
            text: inner,
            style: style.patch(ctx.theme.dim()).add_modifier(Modifier::UNDERLINED),
        }],
        close + 1,
    ))
}

/// `$…$` and `\(…\)`.
///
/// The refusal path is the important one: an expression [`crate::latex`] cannot represent is
/// emitted **as its own source, delimiters included**, styled as verbatim text — so the reader can
/// see that they are looking at LaTeX rather than at a formula somebody has quietly rearranged.
fn maths(chars: &[char], i: usize, ctx: &Ctx, style: Style) -> Option<(Vec<Run>, usize)> {
    // **`$$...$$` MID-LINE, and this is the bug it fixes.**
    //
    // `display_math` only recognises `$$` when it is the WHOLE line. A model writing
    // `Policy gradient: $$...$$` -- a label, then display maths -- never matched it and fell
    // through to here, where `open` was a single `$`: `start` landed on the SECOND `$`, the loop
    // matched it immediately as the close, the content was empty, `looks_like_inline_maths("")`
    // said no, and the whole expression was emitted as literal source with its delimiters. Three
    // of four formulas in a real reply came out raw.
    //
    // Two dollars is the longer delimiter and must be tried FIRST: the single-`$` arm is a prefix
    // of it and would otherwise always win.
    let (open, close): (&[char], &[char]) = if chars[i] == '$' {
        if chars.get(i + 1) == Some(&'$') {
            (&['$', '$'], &['$', '$'])
        } else {
            (&['$'], &['$'])
        }
    } else {
        (&['\\', '('], &['\\', ')'])
    };
    let start = i + open.len();
    let mut j = start;
    // **An expression still being streamed renders as though it were closed.**
    //
    // A reply arrives token by token, so `$$y = \sigma(Wx` is what the pane holds for as long as
    // the model takes to finish the line. Requiring the closing delimiter meant the raw source sat
    // there and then SNAPPED into a formula when the last two characters landed. Rendering the
    // partial makes the equation appear to build, which is what the rest of the surface already
    // does -- section B5s rule is that motion means Marlowe is working.
    //
    // **Unterminated at end-of-input only, never across a newline.** A `$` in ordinary prose with
    // no partner would otherwise swallow the remainder of the reply, and the newline bail below is
    // what stops it. A partial expression is also frequently mid-token (`ra`, `rac{1`), which
    // `latex::render` refuses -- so it falls back to source, exactly as for anything else it
    // cannot represent. That fallback is what makes this safe rather than clever.
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
            break j;
        }
        if chars[j] == '\n' {
            return None;
        }
        j += 1;
    };
    let mut content: String = chars[start..end].iter().collect();
    // A `$$` span still arriving has seen the FIRST character of its own closing delimiter one
    // frame before the second, so the equation would show a trailing `$` for exactly that frame.
    // It is the close, not content.
    if unterminated {
        while content.ends_with('$') {
            content.pop();
        }
    }
    if open[0] == '$' && !crate::latex::looks_like_inline_maths(&content) {
        return None;
    }
    // The delimiters are only consumed if they were actually there.
    let consumed = if unterminated { chars.len() } else { end + close.len() };
    let source: String = chars[i..consumed].iter().collect();
    // **While it is still arriving, render the longest valid PREFIX rather than nothing.**
    //
    // Rendering only complete expressions was already the rule, and it flickered: a partial
    // alternates between valid and invalid as each token completes, so the pane flipped between a
    // typeset formula and raw source several times per equation. Both states were correct and the
    // transition between them was the problem.
    //
    // Backing off to the longest prefix that renders makes the display MONOTONIC -- it only ever
    // grows -- because a prefix that rendered a frame ago still renders now with more after it.
    // `$$y = \s` shows `y = ` rather than reverting to source, and the half-typed command simply
    // has not appeared yet.
    //
    // **Only for unterminated spans.** A CLOSED expression that will not render is a real refusal
    // -- a matrix, an over-line -- and must stay visibly its source, not be silently truncated to
    // the part that happened to work. That would be the "wrong and plausible" outcome this module
    // exists to prevent, and it is why this is keyed on `unterminated` rather than on failure.
    let rendered = crate::latex::render(&content).or_else(|| {
        if !unterminated {
            return None;
        }
        let cs: Vec<char> = content.chars().collect();
        (1..cs.len())
            .rev()
            .find_map(|n| crate::latex::render(&cs[..n].iter().collect::<String>()))
    });
    let runs = match rendered {
        Some(rendered) => vec![Run {
            text: rendered,
            // Lighter than the prose around it — see `Ctx::maths`. The REFUSED arm below keeps the
            // verbatim style, because unrendered source is not an equation standing out, it is
            // source the reader should recognise as source.
            style: ctx.maths().patch(style.sub_modifier),
        }],
        None => vec![Run {
            text: source,
            style: ctx.code(),
        }],
    };
    Some((runs, consumed))
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
        for tok in tokenize(&run.text) {
            match tok {
                Token::Space => {
                    if row.col > 0 {
                        pending_space = Some(run.style);
                    }
                }
                Token::Word(word) => {
                    let mut rest = word;
                    loop {
                        let len = rest.chars().count();
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
                        // scrollbar's arithmetic and the screen out of agreement.
                        let head: String = rest.chars().take(width).collect();
                        let tail: String = rest.chars().skip(width).collect();
                        row.push(&head, run.style);
                        rows.push(row.finish());
                        rest = tail;
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
        self.col += text.chars().count();
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
    fn maths_renders_when_it_can_and_stays_source_when_it_cannot() {
        let a = text_of(&render(r"the area is $\pi r^2$ exactly", 60)).join("");
        assert!(a.contains("πr²"), "{a}");
        // **The example changed on 2026-08-22, not the property.** It was
        // `$\int_0^\infty e^{-x}dx$`, which now RENDERS: a script with no Unicode glyph degrades
        // to explicit notation rather than being dropped, so the bound survives and there is
        // nothing left to protect. `\hat{x}` is still genuinely unrenderable -- a combining mark
        // occupies zero columns, so the accent would land on the wrong glyph or on none.
        let b = text_of(&render(r"consider $\hat{x}$ here", 80)).join("");
        assert!(
            b.contains(r"$\hat{x}$"),
            "an expression that cannot be shown must stay visibly its source: {b}"
        );
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
