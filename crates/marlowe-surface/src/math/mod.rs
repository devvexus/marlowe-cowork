//! **LaTeX maths, typeset on the character grid.**
//!
//! Three rules, and each is enforced here rather than hoped for.
//!
//! 1. **Full maths.** Stacked fractions, big operators with limits, radicals with indices,
//!    matrices and `cases`/`aligned` with brackets built to their height, accents, braces, fonts,
//!    `\left…\right`. Display maths is laid out in two dimensions ([`layout`]); inline maths keeps
//!    one line so a paragraph keeps its line height.
//! 2. **Never raw.** A span renders only once it parses ([`parse::strict`]). If it does not, one
//!    deterministic [`repair`] pass gets a single try. If that fails too, the result is a plain
//!    paraphrase from the lenient parser: `\foo{x}` reads `foo(x)`. No path returns the source.
//! 3. **Smooth.** Rendering is a pure function of the source, memoised by it, so a formula is laid
//!    out once and every later frame (scrolling included) is a lookup. Holding an unclosed span
//!    back until it closes is the caller's job (`markdown.rs`), behind a one-row placeholder.
//!
//! # The grid first, images on top
//!
//! The grid works on every terminal, adds no dependency, and lays a formula out in microseconds
//! (`tests/maths_corpus.rs` prints the p95). Where the terminal speaks Sixel, `marlowe-typeset`
//! has the helper process (`marlowe-typeset.exe`, Typst) draw a display formula as an image inside
//! the exact cells its grid layout reserved; [`Formula::typst`] is that path's input, produced
//! from this same parse ([`typst`]), so an image is drawn exactly when the grid would have typeset.
//!
//! A matrix wider than the pane is drawn clipped at the pane's edge with [`CLIP_MARKER`]
//! ([`Formula::clipped`]); any other display too wide is set as one line per row and wrapped.
//!
//! # Safety
//!
//! Every emitted character passes `marlowe_contract::text::is_renderable` and is not a
//! [`crate::chrome`] glyph. That is checked on the output, not just on the tables. The fraction bar
//! is `⎯` (U+23AF) because `─` is reserved for borders.

mod gallery;
mod layout;
mod parse;
mod repair;
mod symbols;
mod typst;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use unicode_width::UnicodeWidthStr;

pub use gallery::GALLERY;
use layout::Mode;

/// How faithfully a formula was rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fidelity {
    /// The source parsed as written.
    Typeset,
    /// One repair pass fixed a slip (an unclosed brace, a stray `$`) and it then parsed.
    Repaired,
    /// It did not parse even after repair. This is a plain-text reading of it, never the source.
    Paraphrased,
}

/// A rendered formula.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Formula {
    /// The rows to draw. One row for inline maths; the 2D layout for display maths.
    pub rows: Vec<String>,
    /// Display columns of the widest row.
    pub width: usize,
    /// The one-line form, one line per row of a top-level alignment. Used when [`Self::rows`] is
    /// wider than the pane.
    pub linear: Vec<String>,
    pub fidelity: Fidelity,
    /// The formula as Typst math markup, for the image path (`marlowe-typeset`). Display maths
    /// that parsed (or was repaired) only: a paraphrase is never typeset as though it were maths.
    pub typst: Option<String>,
    /// For a display holding a matrix: the row that carries [`CLIP_MARKER`] when the layout is
    /// wider than the pane. A matrix is clipped there rather than set on one line, because its
    /// one-line form (`[a, b; c, d]`) wrapped over several rows reads as noise.
    pub clip_row: Option<usize>,
}

/// Marks where a matrix too wide for the pane was cut. U+22EF is the harness's own glyph and is
/// refused in model text ([`crate::chrome`]); here the harness draws it, only at the clipped edge
/// of a formula, never at the start of a line where a tool line has it.
pub const CLIP_MARKER: char = '\u{22EF}';

impl Formula {
    /// The rows if they fit in `avail` columns, else the linear lines. `true` means 2D rows.
    pub fn fit(&self, avail: usize) -> (&[String], bool) {
        if self.width <= avail {
            (&self.rows, true)
        } else {
            (&self.linear, false)
        }
    }

    /// A matrix wider than `avail`, cut to fit: the rows clipped to leave two columns, and the row
    /// that takes `" ⋯"` in them. `None` when it fits or is not a matrix.
    pub fn clipped(&self, avail: usize) -> Option<(Vec<String>, usize)> {
        let mark = self.clip_row?;
        if self.width <= avail || avail < 8 {
            return None;
        }
        let rows = self.rows.iter().map(|r| clip_cols(r, avail - 2)).collect();
        Some((rows, mark.min(self.rows.len().saturating_sub(1))))
    }

    /// The rows this display takes in `avail` columns: its 2D rows, clipped or whole, or its
    /// one-line form word-wrapped.
    pub fn rows_in(&self, avail: usize) -> usize {
        if self.clipped(avail).is_some() {
            return self.rows.len().max(1);
        }
        let (lines, two_d) = self.fit(avail);
        if two_d {
            lines.len().max(1)
        } else {
            lines.iter().map(|l| l.width().div_ceil(avail.max(1)).max(1)).sum::<usize>().max(1)
        }
    }
}

/// `s` cut to at most `w` display columns, trailing blanks dropped. A combining mark stays with
/// its letter.
fn clip_cols(s: &str, w: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut out = String::with_capacity(s.len());
    let mut used = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if used + cw > w {
            break;
        }
        used += cw;
        out.push(c);
    }
    out.trim_end().to_string()
}

/// Longer sources go straight to the linear form; a formula this size is not read as a picture.
const MAX_SOURCE: usize = 8_192;
/// A display taller or wider than this reads better as lines.
const MAX_ROWS: usize = 40;
const MAX_COLS: usize = 400;
/// The memo is bounded. Past this it is cleared, which costs one re-layout per formula.
const CACHE_CAP: usize = 2_048;

type Cache = Mutex<HashMap<(bool, String), Arc<Formula>>>;

fn cache() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached(src: &str, display: bool) -> Arc<Formula> {
    let key = (display, src.to_string());
    if let Ok(map) = cache().lock() {
        if let Some(f) = map.get(&key) {
            return Arc::clone(f);
        }
    }
    let f = Arc::new(build(src, display));
    if let Ok(mut map) = cache().lock() {
        if map.len() >= CACHE_CAP {
            map.clear();
        }
        map.insert(key, Arc::clone(&f));
    }
    f
}

/// Inline maths, without its delimiters. Always one line.
pub fn inline(src: &str) -> Arc<Formula> {
    cached(src, false)
}

/// Display maths, without its delimiters.
pub fn display(src: &str) -> Arc<Formula> {
    cached(src, true)
}

/// The rows a display laid out from `partial` takes in `avail` columns. Not memoised. Not a height
/// to reserve while a block streams: a partial's height rises and falls token by token, which is
/// why a held block is one placeholder row (`tests/stream_replay.rs` in `marlowe-tui`).
pub fn reserve_rows(partial: &str, avail: usize) -> usize {
    build(partial, true).rows_in(avail)
}

/// The full pipeline without the memo, so a timing measures layout and not a hash lookup.
#[doc(hidden)]
pub fn typeset_uncached(src: &str, display: bool) -> Formula {
    build(src, display)
}

/// The Typst markup for a display formula, or `None` when it did not parse or has no faithful
/// Typst form. The same answer as [`display`]`(src).typst`, without the layout.
pub fn typst_markup(src: &str) -> Option<String> {
    match parse_with_repair(src) {
        (_, Fidelity::Paraphrased) => None,
        (node, _) => typst::markup(&node),
    }
}

/// Whether `src` parses strictly, with no repair. The corpus test's coverage number.
pub fn parses(src: &str) -> bool {
    parse::strict(src).is_ok()
}

fn parse_with_repair(src: &str) -> (parse::Node, Fidelity) {
    if src.len() > MAX_SOURCE {
        return (parse::Node::Text(parse::strip_syntax(src)), Fidelity::Paraphrased);
    }
    if let Ok(n) = parse::strict(src) {
        return (n, Fidelity::Typeset);
    }
    let fixed = repair::repair(src);
    match parse::strict(&fixed) {
        Ok(n) => (n, Fidelity::Repaired),
        Err(_) => (parse::lenient(&fixed), Fidelity::Paraphrased),
    }
}

fn build(src: &str, display: bool) -> Formula {
    let (node, fidelity) = parse_with_repair(src);
    let linear: Vec<String> = layout::linear_rows(&node).into_iter().map(|s| safe(&s)).collect();
    let mut clip_row = None;
    let rows: Vec<String> = if !display {
        vec![safe(&layout::lay(&node, Mode::Linear).line())]
    } else if fidelity == Fidelity::Paraphrased {
        linear.clone()
    } else {
        let g = layout::lay(&node, Mode::Display);
        if g.height() > MAX_ROWS || g.width() > MAX_COLS {
            linear.clone()
        } else {
            let (rows, dropped) = trim_block(g.lines().iter().map(|s| safe(s)).collect());
            if g.height() > 1 && layout::has_matrix(&node) {
                clip_row = Some(g.baseline().saturating_sub(dropped));
            }
            rows
        }
    };
    let rows = if rows.is_empty() { vec![String::new()] } else { rows };
    let linear = if linear.is_empty() { vec![String::new()] } else { linear };
    let width = rows.iter().map(|r| r.width()).max().unwrap_or(0);
    let typst = if display && fidelity != Fidelity::Paraphrased { typst::markup(&node) } else { None };
    Formula { rows, width, linear, fidelity, typst, clip_row }
}

/// Drop blank rows at the top and bottom, and the indent every row shares. Also returns how many
/// rows came off the top.
fn trim_block(mut rows: Vec<String>) -> (Vec<String>, usize) {
    let mut dropped = 0;
    while rows.len() > 1 && rows.first().is_some_and(|r| r.trim().is_empty()) {
        rows.remove(0);
        dropped += 1;
    }
    while rows.len() > 1 && rows.last().is_some_and(|r| r.trim().is_empty()) {
        rows.pop();
    }
    let indent = rows
        .iter()
        .filter(|r| !r.trim().is_empty())
        .map(|r| r.chars().take_while(|c| *c == ' ').count())
        .min()
        .unwrap_or(0);
    if indent > 0 {
        for r in &mut rows {
            *r = r.chars().skip(indent).collect();
        }
    }
    (rows, dropped)
}

/// The output check: anything the display predicate refuses, or that is harness chrome, becomes
/// `?`. The tables never emit one (a test walks them); this is the guarantee that holds when a
/// future table entry or a model-supplied `\text{…}` does.
fn safe(s: &str) -> String {
    if s.chars().all(|c| marlowe_contract::text::is_renderable(c) && !crate::chrome::is_reserved(c)) {
        return s.to_string();
    }
    s.chars()
        .map(|c| {
            if marlowe_contract::text::is_renderable(c) && !crate::chrome::is_reserved(c) {
                c
            } else {
                '?'
            }
        })
        .collect()
}

/// Whether a closed `$…$` span is maths at all, or a pair of dollar signs in prose.
///
/// `$` is a currency sign far more often than a delimiter. *"it costs $5 and $10"* contains a
/// well-formed `$…$` span whose content is `5 and `; rendering it would print `5and`.
///
/// | rule | the sentence it protects |
/// |---|---|
/// | no leading or trailing space in the content | `$5 to $10` -- content `5 to ` |
/// | no run of three or more letters outside a command | `$5 and $10` -- content `5 and ` |
/// | at least one maths signal, a single letter, or a plain number | `$100–$200` -- content `100–` |
/// | no newline, and a bounded length | a `$` at the top of a reply closing at the end |
///
/// A backslash command settles it before the letter-run test: `\operatorname{softmax}` is a
/// seven-letter run inside a command, and prose does not contain `\` followed by a letter.
pub fn looks_like_inline_maths(content: &str) -> bool {
    if content.is_empty() || content.len() > 400 || content.contains('\n') {
        return false;
    }
    if content.starts_with(' ') || content.ends_with(' ') {
        return false;
    }
    if has_command(content) {
        return true;
    }
    if !letter_runs_ok(content, false) {
        return false;
    }
    // `$5/$10` and `$5-$10` are price ranges: content that ends on an operator is not a formula.
    if content.ends_with(['+', '-', '/', '=', '<', '>', '*', ',', '.', '–', '—']) {
        return false;
    }
    let chars: Vec<char> = content.chars().collect();
    // `$5$`, `$x$`, `$2x$`: nobody writes two dollar signs around a price, and a short
    // alphanumeric token between them is a symbol.
    let token = chars.iter().all(|c| c.is_alphanumeric() || *c == '.');
    // Braces, `&` and `#` are TeX syntax and never part of a price.
    let signal =
        content.contains(['\\', '^', '_', '=', '+', '<', '>', '/', '*', '(', '|', '{', '}', '&', '#']);
    token || signal
}

/// Whether an **unclosed** `$` span, still streaming, could yet turn out to be maths -- in which
/// case the caller holds it behind a placeholder rather than showing a `$` that may be a delimiter.
///
/// Prose disqualifies it as soon as it shows: a space straight after the `$`, a newline, or a
/// finished word of three or more letters. A trailing run that may still become `\max` or `\log`
/// is not yet a word.
pub fn could_become_inline_maths(partial: &str) -> bool {
    if partial.starts_with(' ') || partial.contains('\n') || partial.len() > 400 {
        return false;
    }
    if has_command(partial) {
        return true;
    }
    letter_runs_ok(partial, true)
}

/// A `$` that never closed, in **finished** text, whose content holds a command: the model dropped
/// the closer. Showing the rest of the line as source would break the never-raw rule, and reading
/// all of it as maths would set the prose after it as letters. So the formula is taken to end where
/// prose starts -- at the first standalone word of two or more letters, outside braces and
/// commands, that is not an operator name -- less any trailing punctuation. Returns that length in characters, or
/// `None` when the content holds no command (then the `$` was a dollar sign).
pub fn unclosed_inline_extent(content: &str) -> Option<usize> {
    if !has_command(content) {
        return None;
    }
    let chars: Vec<char> = content.chars().collect();
    let n = chars.len();
    let mut cut = n;
    let mut depth = 0usize;
    let mut i = 0;
    while i < n {
        match chars[i] {
            '\\' => {
                i += 1;
                if i < n && chars[i].is_ascii_alphabetic() {
                    while i < n && chars[i].is_ascii_alphabetic() {
                        i += 1;
                    }
                } else {
                    i += 1;
                }
                continue;
            }
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            c if c.is_whitespace() && depth == 0 => {
                let mut j = i;
                while j < n && chars[j].is_whitespace() {
                    j += 1;
                }
                let mut k = j;
                while k < n && chars[k].is_ascii_alphabetic() {
                    k += 1;
                }
                let word: String = chars[j..k].iter().collect();
                // A standalone word: `be` in `$\alpha be the rate`, not `xy^2` or `ab_i`.
                let standalone = chars.get(k).is_none_or(|c| c.is_whitespace() || matches!(c, ',' | '.' | ';' | ':' | '!' | '?'));
                if word.len() >= 2 && standalone && !symbols::is_function_word(&word) {
                    cut = i;
                    break;
                }
            }
            _ => {}
        }
        i += 1;
    }
    while cut > 0 && (chars[cut - 1].is_whitespace() || matches!(chars[cut - 1], ',' | '.' | ';' | ':')) {
        // `\,` and `\;` are spaces in maths, not punctuation after it.
        if cut >= 2 && chars[cut - 2] == '\\' {
            break;
        }
        cut -= 1;
    }
    let head: String = chars[..cut].iter().collect();
    has_command(&head).then_some(cut)
}

/// The glyph for a symbol command a model wrote in prose with no delimiters around it: `the rate
/// \eta`. Symbols only (Greek, relations, arrows, operators), never a command that takes an
/// argument, and never a one-letter name, so `\n` in prose stays as written.
pub fn bare_symbol(name: &str) -> Option<&'static str> {
    if name.len() < 2 {
        return None;
    }
    symbols::symbol(name).map(|(g, _)| g).filter(|g| !g.is_empty())
}

fn has_command(s: &str) -> bool {
    s.as_bytes().windows(2).any(|w| w[0] == b'\\' && w[1].is_ascii_alphabetic())
}

/// No run of three or more letters outside a command, unless it is an operator name -- or, when
/// `streaming`, the run at the very end could still grow into one.
fn letter_runs_ok(s: &str, streaming: bool) -> bool {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' {
            i += 1;
            while i < chars.len() && chars[i].is_ascii_alphabetic() {
                i += 1;
            }
            continue;
        }
        if chars[i].is_ascii_alphabetic() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_alphabetic() {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let at_end = i == chars.len();
            let ok = word.len() < 3
                || symbols::is_function_word(&word)
                || (streaming && at_end && symbols::is_function_prefix(&word));
            if !ok {
                return false;
            }
            continue;
        }
        i += 1;
    }
    true
}

#[cfg(test)]
mod tests;
