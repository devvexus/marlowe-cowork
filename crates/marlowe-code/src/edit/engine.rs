//! One `old_string` -> `new_string` replacement over a file's text, in three tiers (design 3.2):
//!
//! 1. **exact**: byte-for-byte.
//! 2. **eol**: the snippet's line endings rewritten to the file's (an LF snippet against a CRLF
//!    file, or the reverse).
//! 3. **whitespace**: whole lines compared ignoring trailing whitespace and one *uniform*
//!    indentation shift (every non-blank line of the file is the snippet's line with the same
//!    prefix added, or with the same prefix removed); the replacement is re-indented by that
//!    shift. Lines whose indentation differs non-uniformly do not match.
//!
//! A tier that finds several sites never picks one: unless `replace_all` is set, more than one
//! site is an error naming their lines, and no fuzzier tier is tried. New text follows the file's
//! line endings when the file is uniformly CRLF or uniformly LF. A miss explains itself
//! (line-number prefixes, whitespace) and shows the closest region of the file.

use serde::{Deserialize, Serialize};

use super::doc::{dominant_eol, to_eol, uniformly_crlf, Eol};
use super::numbering::{count_lines, line_at, number_lines, strip_line_numbers, LINE_NUMBER_WIDTH};

/// Sites an ambiguity error names before "and N more".
pub const MAX_EDIT_SITES_NAMED: usize = 8;

/// Lines of the closest region a miss shows.
const DID_YOU_MEAN_LINES: usize = 30;

/// How the snippet was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Exact,
    Eol,
    Whitespace,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Exact => "exact",
            Tier::Eol => "eol",
            Tier::Whitespace => "whitespace",
        }
    }

    /// The clause a result adds for this tier (empty for an exact match).
    pub fn note(self) -> &'static str {
        match self {
            Tier::Exact => "",
            Tier::Eol => "matched after normalising line endings",
            Tier::Whitespace => {
                "matched ignoring indentation and trailing spaces; the replacement was re-indented to match"
            }
        }
    }
}

/// A replacement that applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// The whole new text.
    pub text: String,
    pub tier: Tier,
    /// Sites replaced (1 unless `replace_all`).
    pub count: usize,
    /// The 1-based first line of each site, in the original text.
    pub lines: Vec<usize>,
    /// Lines inserted and removed, summed over the sites.
    pub added: usize,
    pub removed: usize,
}

/// Why a replacement did not apply. The text is unchanged in every case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    EmptyOld,
    NoChange,
    NotFound { hint: String },
    Ambiguous { count: usize, lines: Vec<usize>, tier: Tier },
}

impl EditError {
    /// A short machine-readable kind (fixtures, bench).
    pub fn kind(&self) -> &'static str {
        match self {
            EditError::EmptyOld => "empty_old",
            EditError::NoChange => "no_change",
            EditError::NotFound { .. } => "not_found",
            EditError::Ambiguous { .. } => "ambiguous",
        }
    }

    /// What the model reads.
    pub fn message(&self) -> String {
        match self {
            EditError::EmptyOld => "`old_string` is empty, which would match at the very start of the file \
                                    and insert `new_string` there rather than replace anything. Give the \
                                    exact snippet to replace, or use `write` to replace the whole file."
                .into(),
            EditError::NoChange => "`old_string` and `new_string` are identical, so there is nothing to \
                                    change. The file is unchanged."
                .into(),
            EditError::NotFound { hint } => hint.clone(),
            EditError::Ambiguous { count, lines, tier } => {
                let named =
                    lines.iter().take(MAX_EDIT_SITES_NAMED).map(usize::to_string).collect::<Vec<_>>().join(", ");
                let rest = if lines.len() > MAX_EDIT_SITES_NAMED {
                    format!(" and {} more", lines.len() - MAX_EDIT_SITES_NAMED)
                } else {
                    String::new()
                };
                let how = match tier {
                    Tier::Exact => String::new(),
                    t => format!(" ({})", t.note().split(';').next().unwrap_or("")),
                };
                format!(
                    "`old_string` occurs {count} times{how}, at lines {named}{rest}. `edit` changes ONE \
                     snippet, so it must appear exactly once — add a neighbouring line to make it \
                     unique, or set `replace_all` to change every occurrence. The file is unchanged."
                )
            }
        }
    }
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

/// Replaces `old` with `new` in `text` (every occurrence with `replace_all`, else exactly one).
pub fn apply(text: &str, old: &str, new: &str, replace_all: bool) -> Result<Applied, EditError> {
    if old.is_empty() {
        return Err(EditError::EmptyOld);
    }
    if old == new {
        return Err(EditError::NoChange);
    }
    // 1. exact
    let sites: Vec<usize> = text.match_indices(old).map(|(i, _)| i).collect();
    if !sites.is_empty() {
        let new = follow_file_eol(text, new);
        let spans = sites.iter().map(|&i| (i, i + old.len(), new.clone())).collect();
        return splice(text, spans, Tier::Exact, replace_all);
    }
    // 2. line endings
    if old.contains('\n') {
        let first = dominant_eol(text);
        let second = if first == Eol::Lf { Eol::Crlf } else { Eol::Lf };
        for eol in [first, second] {
            let o = to_eol(old, eol);
            if o == old {
                continue;
            }
            let sites: Vec<usize> = text.match_indices(&o).map(|(i, _)| i).collect();
            if !sites.is_empty() {
                let n = to_eol(new, eol);
                let spans = sites.iter().map(|&i| (i, i + o.len(), n.clone())).collect();
                return splice(text, spans, Tier::Eol, replace_all);
            }
        }
    }
    // 3. whitespace
    let spans = whitespace_sites(text, old, new);
    if !spans.is_empty() {
        return splice(text, spans, Tier::Whitespace, replace_all);
    }
    Err(EditError::NotFound { hint: miss(text, old) })
}

/// `new` with its line endings matched to a file that is uniformly CRLF or uniformly LF.
fn follow_file_eol(text: &str, new: &str) -> String {
    if uniformly_crlf(text) {
        to_eol(new, Eol::Crlf)
    } else if !text.contains("\r\n") && text.contains('\n') && new.contains("\r\n") {
        to_eol(new, Eol::Lf)
    } else {
        new.to_string()
    }
}

/// Replaces non-overlapping `(start, end, replacement)` spans (ascending).
fn splice(text: &str, spans: Vec<(usize, usize, String)>, tier: Tier, replace_all: bool) -> Result<Applied, EditError> {
    let lines: Vec<usize> = spans.iter().map(|(s, _, _)| line_at(text, *s)).collect();
    if spans.len() > 1 && !replace_all {
        return Err(EditError::Ambiguous { count: spans.len(), lines, tier });
    }
    let mut out = String::with_capacity(text.len());
    let (mut at, mut added, mut removed) = (0usize, 0usize, 0usize);
    for (s, e, r) in &spans {
        out.push_str(&text[at..*s]);
        out.push_str(r);
        added += count_lines(r);
        removed += count_lines(&text[*s..*e]);
        at = *e;
    }
    out.push_str(&text[at..]);
    if out == text {
        return Err(EditError::NoChange);
    }
    Ok(Applied { text: out, tier, count: spans.len(), lines, added, removed })
}

/// One line of the text: content `start..end` (no terminator), next line at `next`.
#[derive(Debug, Clone, Copy)]
struct Span {
    start: usize,
    end: usize,
    next: usize,
}

fn line_spans(text: &str) -> Vec<Span> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for line in text.split_inclusive('\n') {
        let next = start + line.len();
        let body = line.strip_suffix('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).unwrap_or(line);
        out.push(Span { start, end: start + body.len(), next });
        start = next;
    }
    out
}

/// A uniform indentation difference: the file's lines are the snippet's with `Add` prepended,
/// or with `Remove` stripped.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Shift {
    Add(String),
    Remove(String),
}

fn lead(s: &str) -> usize {
    s.len() - s.trim_start().len()
}

/// The shift that makes `old_lines` match the file lines in `spans`, if one uniform shift does.
fn window_shift(text: &str, spans: &[Span], old_lines: &[&str]) -> Option<Shift> {
    let mut shift: Option<Shift> = None;
    for (sp, o) in spans.iter().zip(old_lines) {
        let f = text[sp.start..sp.end].trim_end();
        let o = o.trim_end();
        match (f.is_empty(), o.is_empty()) {
            (true, true) => continue,
            (false, false) => {}
            _ => return None,
        }
        let (lf, lo) = (lead(f), lead(o));
        if f[lf..] != o[lo..] {
            return None;
        }
        let (pf, po) = (&f[..lf], &o[..lo]);
        let s = if pf.ends_with(po) {
            Shift::Add(pf[..lf - lo].to_string())
        } else if po.ends_with(pf) {
            Shift::Remove(po[..lo - lf].to_string())
        } else {
            return None;
        };
        let s = match s {
            Shift::Remove(p) if p.is_empty() => Shift::Add(p),
            s => s,
        };
        match &shift {
            None => shift = Some(s),
            Some(prev) if *prev == s => {}
            Some(_) => return None,
        }
    }
    shift
}

fn reindent(new: &str, shift: &Shift, eol: &str) -> String {
    let new_lf = to_eol(new, Eol::Lf);
    if new_lf.is_empty() {
        return String::new();
    }
    let ends = new_lf.ends_with('\n');
    let body = new_lf.strip_suffix('\n').unwrap_or(&new_lf);
    let lines: Vec<String> = body
        .split('\n')
        .map(|l| {
            if l.trim().is_empty() {
                return l.to_string();
            }
            match shift {
                Shift::Add(p) => format!("{p}{l}"),
                Shift::Remove(p) => match l.strip_prefix(p.as_str()) {
                    Some(rest) => rest.to_string(),
                    None => {
                        let lw = &l[..lead(l)];
                        if p.starts_with(lw) {
                            l[lw.len()..].to_string()
                        } else {
                            l.to_string()
                        }
                    }
                },
            }
        })
        .collect();
    let mut out = lines.join(eol);
    if ends {
        out.push_str(eol);
    }
    out
}

/// Whitespace-tier sites as `(start, end, replacement)`, non-overlapping and ascending.
fn whitespace_sites(text: &str, old: &str, new: &str) -> Vec<(usize, usize, String)> {
    let old_lf = to_eol(old, Eol::Lf);
    let old_ends_nl = old_lf.ends_with('\n');
    let body = old_lf.strip_suffix('\n').unwrap_or(&old_lf);
    let old_lines: Vec<&str> = body.split('\n').collect();
    if old_lines.iter().all(|l| l.trim().is_empty()) {
        return Vec::new();
    }
    let spans = line_spans(text);
    let k = old_lines.len();
    if k > spans.len() {
        return Vec::new();
    }
    let eol = dominant_eol(text).as_str();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + k <= spans.len() {
        if let Some(shift) = window_shift(text, &spans[i..i + k], &old_lines) {
            let last = spans[i + k - 1];
            let at_eof_without_eol = last.next == last.end;
            let end = if old_ends_nl { last.next } else { last.end };
            let mut r = reindent(new, &shift, eol);
            if old_ends_nl && at_eof_without_eol {
                if let Some(t) = r.strip_suffix(eol) {
                    r = t.to_string();
                }
            }
            out.push((spans[i].start, end, r));
            i += k;
        } else {
            i += 1;
        }
    }
    out
}

/// Why `old` was not found, phrased so the model can act on it, with the closest region.
fn miss(text: &str, old: &str) -> String {
    if let Some(stripped) = strip_line_numbers(old) {
        return match text.find(&stripped) {
            Some(at) => format!(
                "`old_string` carries the line-number prefix `read` printed ({LINE_NUMBER_WIDTH} \
                 characters, then a tab, in front of every line). `edit` matches the file byte for \
                 byte, so strip them. Stripped, your text IS in the file, starting at line {}.",
                line_at(text, at)
            ),
            None => format!(
                "`old_string` carries the line-number prefix `read` printed, which `edit` never \
                 accepts — strip it. Even stripped the text was not found, so `read` the file \
                 again and copy the snippet from what comes back. The file is {} bytes, {} lines.",
                text.len(),
                count_lines(text)
            ),
        };
    }
    let squash = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut msg = if !old.trim().is_empty() && squash(text).contains(&squash(old)) {
        format!(
            "`old_string` was not found. The file DOES contain that text apart from whitespace, but \
             the indentation or line breaks differ in a way that is not one uniform shift — it \
             must match byte for byte. `read` the file and copy the snippet exactly as it comes \
             back. The file is {} bytes, {} lines.",
            text.len(),
            count_lines(text)
        )
    } else {
        format!(
            "`old_string` was not found in the file, which is {} bytes and {} lines. It must match \
             byte for byte, including indentation. `read` the file first and copy the snippet \
             from what it returns.",
            text.len(),
            count_lines(text)
        )
    };
    if let Some((first, snippet, pct)) = closest(text, old) {
        let last = first + count_lines(&snippet).saturating_sub(1);
        msg.push_str(&format!(
            "\nDid you mean lines {first}-{last} ({pct}% similar)? They read:\n{}",
            number_lines(&snippet, first).trim_end()
        ));
        if let Some(words) = word_slip(old, &snippet, first) {
            msg.push('\n');
            msg.push_str(&words);
        }
    }
    msg
}

/// Identifier-like words of a line, in order.
fn words(line: &str) -> Vec<&str> {
    line.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| !w.is_empty()).collect()
}

/// When the snippet and the closest region differ on exactly one line, and there only by a word
/// or two (an invented `pub`, `u32` for `i32`), names those words: "copy it exactly" alone does
/// not tell a model what it got wrong. `None` for anything looser, where the region is the hint.
fn word_slip(old: &str, region: &str, first: usize) -> Option<String> {
    let old_lf = to_eol(old, Eol::Lf);
    let a: Vec<&str> = old_lf.trim_end_matches('\n').split('\n').collect();
    let b: Vec<&str> = region.lines().collect();
    if a.len() != b.len() {
        return None;
    }
    let mut differing = a.iter().zip(&b).enumerate().filter(|(_, (x, y))| x.trim() != y.trim());
    let (i, (mine, theirs)) = differing.next()?;
    if differing.next().is_some() {
        return None;
    }
    let (wa, wb) = (words(mine), words(theirs));
    let distinct = |xs: &[&str], ys: &[&str]| {
        let mut v: Vec<String> = xs.iter().filter(|w| !ys.contains(w)).map(|w| format!("`{w}`")).collect();
        v.sort();
        v.dedup();
        v
    };
    let (extra, missing) = (distinct(&wa, &wb), distinct(&wb, &wa));
    let shared = wa.iter().filter(|w| wb.contains(w)).count();
    if extra.len() + missing.len() == 0 || extra.len() > 2 || missing.len() > 2 || shared * 2 < wa.len().max(wb.len()) {
        return None;
    }
    let line = first + i;
    Some(match (extra.is_empty(), missing.is_empty()) {
        (false, true) => format!("You wrote {} on a line the file has without it (line {line}).", extra.join(" and ")),
        (true, false) => format!("The file's line {line} has {}, which your text leaves out.", missing.join(" and ")),
        _ => format!("You wrote {} where the file's line {line} has {}.", extra.join(" and "), missing.join(" and ")),
    })
}

/// Sorted character bigrams of `s`.
fn bigrams(s: &str) -> Vec<(char, char)> {
    let cs: Vec<char> = s.chars().collect();
    let mut v: Vec<(char, char)> = cs.windows(2).map(|w| (w[0], w[1])).collect();
    v.sort_unstable();
    v
}

/// Dice similarity of two trimmed lines over character bigrams, 0..=1.
fn similarity(a: &str, b: &str) -> f64 {
    let (a, b) = (a.trim(), b.trim());
    if a == b {
        return 1.0;
    }
    if a.chars().count() < 2 || b.chars().count() < 2 {
        return 0.0;
    }
    let (x, y) = (bigrams(a), bigrams(b));
    let (mut i, mut j, mut common) = (0usize, 0usize, 0usize);
    while i < x.len() && j < y.len() {
        match x[i].cmp(&y[j]) {
            std::cmp::Ordering::Equal => {
                common += 1;
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
    2.0 * common as f64 / (x.len() + y.len()) as f64
}

/// The region of `text` most like `old`: its first line, its text and a similarity percentage.
/// Candidates are aligned on the snippet's longest line; `None` below 60%.
fn closest(text: &str, old: &str) -> Option<(usize, String, u32)> {
    let old_lf = to_eol(old, Eol::Lf);
    let old_lines: Vec<&str> = old_lf.trim_end_matches('\n').split('\n').collect();
    let (anchor_i, anchor) = old_lines.iter().enumerate().max_by_key(|(_, l)| l.trim().len())?;
    if anchor.trim().len() < 3 {
        return None;
    }
    let file: Vec<&str> = text.split_inclusive('\n').collect();
    if file.is_empty() || file.len() > 200_000 {
        return None;
    }
    let mut cands: Vec<(f64, usize)> = file
        .iter()
        .enumerate()
        .map(|(j, l)| (similarity(anchor, l), j))
        .filter(|(s, _)| *s >= 0.5)
        .collect();
    cands.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    cands.truncate(5);
    let k = old_lines.len().min(DID_YOU_MEAN_LINES);
    let mut best: Option<(f64, usize)> = None;
    for (_, j) in cands {
        let start = j.saturating_sub(anchor_i);
        let end = (start + k).min(file.len());
        let pairs: Vec<f64> =
            (start..end).zip(old_lines.iter()).map(|(fi, o)| similarity(o, file[fi])).collect();
        let score = pairs.iter().sum::<f64>() / k as f64;
        if best.is_none_or(|(b, _)| score > b) {
            best = Some((score, start));
        }
    }
    let (score, start) = best?;
    if score < 0.6 {
        return None;
    }
    let end = (start + k).min(file.len());
    Some((start + 1, file[start..end].concat(), (score * 100.0).round() as u32))
}
