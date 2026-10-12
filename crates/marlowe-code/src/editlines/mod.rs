//! `edit_lines{path, anchor:"line:hash", end, op, text}` (design 2.1 T9, 3.2): the hashed-line
//! edit format, a bench arm for local models. `read` shows each line as `N:hhh|text` when the
//! host has the arm on; an edit names its lines by those anchors, and an anchor whose hash no
//! longer matches the line is refused with fresh anchors, so a model editing from a stale view
//! never lands on the wrong line.

use super::edit::doc::{dominant_eol, to_eol};

/// Hex characters of a line hash.
pub const HASH_LEN: usize = 3;

/// Lines of context a stale-anchor refusal shows on each side.
const STALE_CONTEXT: usize = 3;

/// The hash of one line: blake3 of the line without its terminator or trailing whitespace.
pub fn line_hash(line: &str) -> String {
    let line = line.trim_end_matches(['\n', '\r']).trim_end();
    let h = blake3::hash(line.as_bytes());
    h.to_hex()[..HASH_LEN].to_string()
}

/// `text` with every line prefixed `N:hhh|`, counting from `first_line`.
pub fn anchored(text: &str, first_line: usize) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    for (i, line) in text.split_inclusive('\n').enumerate() {
        out.push_str(&format!("{}:{}|", first_line + i, line_hash(line)));
        out.push_str(line);
    }
    out
}

/// `N:hhh`; line 0 (no hash) means "before the first line".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    pub line: usize,
    pub hash: String,
}

/// What to do at the anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Replace lines `anchor..=end` with `text`.
    Replace,
    InsertAfter,
    InsertBefore,
    /// Delete lines `anchor..=end`.
    Delete,
}

impl Op {
    pub fn parse(s: &str) -> Option<Op> {
        Some(match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "replace" => Op::Replace,
            "insert_after" | "after" | "append" => Op::InsertAfter,
            "insert_before" | "before" | "prepend" => Op::InsertBefore,
            "delete" | "remove" => Op::Delete,
            _ => return None,
        })
    }
}

/// Why an `edit_lines` call did not apply. The file is unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinesError {
    BadAnchor(String),
    OutOfRange { line: usize, total: usize },
    EndBeforeStart { start: usize, end: usize },
    /// The hash does not match the line any more; `view` shows fresh anchors around it.
    Stale { line: usize, given: String, actual: String, view: String },
}

impl LinesError {
    pub fn message(&self) -> String {
        match self {
            LinesError::BadAnchor(a) => format!(
                "`{a}` is not an anchor. Anchors look like `12:a3f` — the line number and hash \
                 `read` printed in front of the line."
            ),
            LinesError::OutOfRange { line, total } => {
                format!("line {line} does not exist: the file has {total} lines. The file is unchanged.")
            }
            LinesError::EndBeforeStart { start, end } => {
                format!("`end` (line {end}) comes before `anchor` (line {start}). The file is unchanged.")
            }
            LinesError::Stale { line, given, actual, view } => format!(
                "line {line} has changed since you read it (hash {given}, now {actual}), so the edit \
                 was refused and the file is unchanged. Current lines:\n{}",
                view.trim_end()
            ),
        }
    }
}

impl Anchor {
    /// Parses `N:hhh` (also `N:hhh|text` as `read` printed it, and a bare `0`).
    pub fn parse(s: &str) -> Result<Anchor, LinesError> {
        let core = s.split('|').next().unwrap_or("").trim();
        let bad = || LinesError::BadAnchor(s.trim().chars().take(40).collect());
        let (n, h) = match core.split_once(':') {
            Some((n, h)) => (n.trim(), h.trim()),
            None if core == "0" => ("0", ""),
            None => return Err(bad()),
        };
        let line: usize = n.parse().map_err(|_| bad())?;
        if line > 0 && (h.len() != HASH_LEN || !h.chars().all(|c| c.is_ascii_hexdigit())) {
            return Err(bad());
        }
        Ok(Anchor { line, hash: h.to_ascii_lowercase() })
    }
}

/// A successful line edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinesApplied {
    pub text: String,
    /// 1-based first line touched in the new text.
    pub first: usize,
    pub added: usize,
    pub removed: usize,
}

fn check(lines: &[&str], a: &Anchor) -> Result<(), LinesError> {
    if a.line == 0 {
        return Ok(());
    }
    if a.line > lines.len() {
        return Err(LinesError::OutOfRange { line: a.line, total: lines.len() });
    }
    let actual = line_hash(lines[a.line - 1]);
    if actual != a.hash {
        let from = a.line.saturating_sub(STALE_CONTEXT).max(1);
        let to = (a.line + STALE_CONTEXT).min(lines.len());
        let view = anchored(&lines[from - 1..to].concat(), from);
        return Err(LinesError::Stale { line: a.line, given: a.hash.clone(), actual, view });
    }
    Ok(())
}

/// Applies one line edit to `text`. `new` follows the file's line endings.
pub fn apply(text: &str, anchor: &Anchor, end: Option<&Anchor>, op: Op, new: &str) -> Result<LinesApplied, LinesError> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    check(&lines, anchor)?;
    if let Some(e) = end {
        check(&lines, e)?;
        if e.line < anchor.line {
            return Err(LinesError::EndBeforeStart { start: anchor.line, end: e.line });
        }
    }
    let eol = dominant_eol(text).as_str();
    let mut new_lines: Vec<String> = if new.is_empty() {
        Vec::new()
    } else {
        let n = to_eol(new, dominant_eol(text));
        let n = n.strip_suffix(eol).unwrap_or(&n).to_string();
        n.split(eol).map(|l| format!("{l}{eol}")).collect()
    };
    let (start, stop) = match op {
        Op::Replace | Op::Delete => {
            if anchor.line == 0 {
                return Err(LinesError::OutOfRange { line: 0, total: lines.len() });
            }
            (anchor.line - 1, end.map_or(anchor.line, |e| e.line))
        }
        Op::InsertAfter => (anchor.line, anchor.line),
        Op::InsertBefore => {
            let at = anchor.line.saturating_sub(1);
            (at, at)
        }
    };
    if op == Op::Delete {
        new_lines.clear();
    }
    let removed = stop - start;
    // The replaced block's last line had no terminator (end of file): neither does the new one.
    let ends_without_eol = stop == lines.len() && lines.last().is_some_and(|l| !l.ends_with('\n'));
    if ends_without_eol && removed > 0 {
        if let Some(last) = new_lines.last_mut() {
            let trimmed = last.strip_suffix(eol).unwrap_or(last).to_string();
            *last = trimmed;
        }
    }
    let mut out = String::with_capacity(text.len() + new.len());
    for l in &lines[..start] {
        out.push_str(l);
    }
    // Inserting after a last line that has no terminator: give it one first.
    if start > 0 && start == lines.len() && !lines[start - 1].ends_with('\n') && !new_lines.is_empty() {
        out.push_str(eol);
    }
    let added = new_lines.len();
    for l in &new_lines {
        out.push_str(l);
    }
    for l in &lines[stop..] {
        out.push_str(l);
    }
    Ok(LinesApplied { text: out, first: start + 1, added, removed })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(text: &str, line: usize) -> Anchor {
        let l = text.split_inclusive('\n').nth(line - 1).unwrap();
        Anchor { line, hash: line_hash(l) }
    }

    #[test]
    fn anchors_round_trip_through_read_output() {
        let text = "fn a() {}\nfn b() {}\n";
        let shown = anchored(text, 1);
        let first = shown.lines().next().unwrap();
        let anchor = Anchor::parse(first).unwrap();
        assert_eq!(anchor, a(text, 1));
        assert!(Anchor::parse("x:abc").is_err() && Anchor::parse("3:zz").is_err());
        assert_eq!(Anchor::parse("0").unwrap().line, 0);
    }

    #[test]
    fn replace_insert_delete() {
        let t = "one\ntwo\nthree\n";
        let r = apply(t, &a(t, 2), None, Op::Replace, "TWO").unwrap();
        assert_eq!(r.text, "one\nTWO\nthree\n");
        let r = apply(t, &a(t, 1), Some(&a(t, 2)), Op::Replace, "x\ny\nz\n").unwrap();
        assert_eq!(r.text, "x\ny\nz\nthree\n");
        assert_eq!((r.added, r.removed), (3, 2));
        assert_eq!(apply(t, &a(t, 3), None, Op::InsertAfter, "four").unwrap().text, "one\ntwo\nthree\nfour\n");
        assert_eq!(apply(t, &a(t, 1), None, Op::InsertBefore, "zero").unwrap().text, "zero\none\ntwo\nthree\n");
        assert_eq!(apply(t, &a(t, 2), Some(&a(t, 3)), Op::Delete, "").unwrap().text, "one\n");
        assert_eq!(apply("", &Anchor::parse("0").unwrap(), None, Op::InsertAfter, "a\n").unwrap().text, "a\n");
        let c = "a\r\nb\r\n";
        assert_eq!(apply(c, &a(c, 1), None, Op::Replace, "x\ny").unwrap().text, "x\r\ny\r\nb\r\n");
        let no_nl = "a\nb";
        assert_eq!(apply(no_nl, &a(no_nl, 2), None, Op::Replace, "B\n").unwrap().text, "a\nB");
        assert_eq!(apply(no_nl, &a(no_nl, 2), None, Op::InsertAfter, "c").unwrap().text, "a\nb\nc\n");
    }

    #[test]
    fn parity_t9_stale_hash_rejected() {
        let t = "one\ntwo\nthree\n";
        let stale = Anchor { line: 2, hash: line_hash("deux") };
        match apply(t, &stale, None, Op::Replace, "x") {
            Err(e @ LinesError::Stale { .. }) => {
                let m = e.message();
                assert!(m.contains(&format!("2:{}|two", line_hash("two"))), "{m}");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(apply(t, &Anchor { line: 9, hash: "abc".into() }, None, Op::Delete, ""), Err(LinesError::OutOfRange { .. })));
        assert!(matches!(apply(t, &a(t, 3), Some(&a(t, 1)), Op::Delete, ""), Err(LinesError::EndBeforeStart { .. })));
    }
}
