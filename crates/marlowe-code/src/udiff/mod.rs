//! Text edits and unified diffs for `rename_symbol` / WorkspaceEdit (design 3.2).
//!
//! A WorkspaceEdit arrives as positioned [`TextEdit`]s; [`apply_edits`] turns them into each
//! file's new text, refusing overlaps rather than guessing an order. The change is shown as a
//! [`unified`] diff, and the diff is what gets applied: [`parse`] and [`apply`] replay it onto the
//! original and must reproduce the new text exactly, so what the model and the user read is
//! byte for byte what lands on disk. [`apply`] matches context strictly (no fuzz), so a stale
//! diff fails instead of landing somewhere else.

use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DiffError {
    #[error("edit {0}..{1} is outside the text ({2} bytes) or splits a character")]
    OutOfRange(usize, usize, usize),
    #[error("edits overlap at byte {0}")]
    Overlap(usize),
    #[error("malformed diff at line {0}: {1}")]
    Malformed(usize, String),
    #[error("hunk {0} does not match the file at line {1}")]
    Mismatch(usize, usize),
}

/// Replace bytes `start..end` with `new_text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub start: usize,
    pub end: usize,
    pub new_text: String,
}

/// Applies non-overlapping edits (any order; inserts at one point keep their given order).
pub fn apply_edits(text: &str, edits: &[TextEdit]) -> Result<String, DiffError> {
    let mut order: Vec<usize> = (0..edits.len()).collect();
    order.sort_by_key(|&i| (edits[i].start, edits[i].end, i));
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for i in order {
        let e = &edits[i];
        if e.start > e.end || e.end > text.len() || !text.is_char_boundary(e.start) || !text.is_char_boundary(e.end) {
            return Err(DiffError::OutOfRange(e.start, e.end, text.len()));
        }
        if e.start < at {
            return Err(DiffError::Overlap(e.start));
        }
        out.push_str(&text[at..e.start]);
        out.push_str(&e.new_text);
        at = e.end;
    }
    out.push_str(&text[at..]);
    Ok(out)
}

/// Lines with their terminators.
fn lines(s: &str) -> Vec<&str> {
    s.split_inclusive('\n').collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Keep,
    Del,
    Add,
}

/// A line diff: common prefix and suffix trimmed, Myers in between.
fn diff_ops(a: &[&str], b: &[&str]) -> Vec<Op> {
    let pre = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suf = a[pre..].iter().rev().zip(b[pre..].iter().rev()).take_while(|(x, y)| x == y).count();
    let (am, bm) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);
    let mut ops = vec![Op::Keep; pre];
    ops.extend(myers(am, bm));
    ops.extend(std::iter::repeat_n(Op::Keep, suf));
    ops
}

/// Myers' O((N+M)D) shortest edit script. Past a large D it falls back to delete-all/add-all,
/// which is still a correct diff.
fn myers(a: &[&str], b: &[&str]) -> Vec<Op> {
    const MAX_D: usize = 4_000;
    let (n, m) = (a.len() as isize, b.len() as isize);
    if n == 0 || m == 0 {
        let mut ops = vec![Op::Del; a.len()];
        ops.extend(std::iter::repeat_n(Op::Add, b.len()));
        return ops;
    }
    let max = (n + m) as usize;
    let off = max as isize;
    let mut v = vec![0isize; 2 * max + 2];
    let mut trace: Vec<Vec<isize>> = Vec::new();
    let mut found = None;
    'outer: for d in 0..=(max.min(MAX_D) as isize) {
        trace.push(v.clone());
        let mut k = -d;
        while k <= d {
            let idx = (k + off) as usize;
            let mut x = if k == -d || (k != d && v[idx - 1] < v[idx + 1]) { v[idx + 1] } else { v[idx - 1] + 1 };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[idx] = x;
            if x >= n && y >= m {
                found = Some(d);
                break 'outer;
            }
            k += 2;
        }
    }
    let Some(dmax) = found else {
        let mut ops = vec![Op::Del; a.len()];
        ops.extend(std::iter::repeat_n(Op::Add, b.len()));
        return ops;
    };
    // Backtrack.
    let mut ops = Vec::new();
    let (mut x, mut y) = (n, m);
    for d in (1..=dmax).rev() {
        let v = &trace[d as usize];
        let k = x - y;
        let idx = |k: isize| (k + off) as usize;
        let prev_k = if k == -d || (k != d && v[idx(k - 1)] < v[idx(k + 1)]) { k + 1 } else { k - 1 };
        let prev_x = v[idx(prev_k)];
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            ops.push(Op::Keep);
            x -= 1;
            y -= 1;
        }
        if x == prev_x {
            ops.push(Op::Add);
            y -= 1;
        } else {
            ops.push(Op::Del);
            x -= 1;
        }
    }
    while x > 0 && y > 0 {
        ops.push(Op::Keep);
        x -= 1;
        y -= 1;
    }
    ops.reverse();
    ops
}

const NO_EOL: &str = "\\ No newline at end of file\n";

fn push_line(out: &mut String, tag: char, line: &str) {
    out.push(tag);
    out.push_str(line);
    if !line.ends_with('\n') {
        out.push('\n');
        out.push_str(NO_EOL);
    }
}

/// A unified diff of `old` -> `new` for file `label` with `context` lines around each change;
/// empty when they are equal.
pub fn unified(label: &str, old: &str, new: &str, context: usize) -> String {
    if old == new {
        return String::new();
    }
    let (a, b) = (lines(old), lines(new));
    let ops = diff_ops(&a, &b);
    // Positions of each op in a and b.
    let mut pos = Vec::with_capacity(ops.len());
    let (mut i, mut j) = (0usize, 0usize);
    for op in &ops {
        pos.push((i, j));
        match op {
            Op::Keep => {
                i += 1;
                j += 1;
            }
            Op::Del => i += 1,
            Op::Add => j += 1,
        }
    }
    let changed: Vec<usize> = ops.iter().enumerate().filter(|(_, o)| **o != Op::Keep).map(|(k, _)| k).collect();
    let mut out = format!("--- a/{label}\n+++ b/{label}\n");
    let mut k = 0;
    while k < changed.len() {
        let start = changed[k].saturating_sub(context);
        let mut end = changed[k];
        while k < changed.len() && changed[k] <= end + 2 * context {
            end = changed[k];
            k += 1;
        }
        let end = (end + context + 1).min(ops.len());
        let (a0, b0) = pos[start];
        let a_len = ops[start..end].iter().filter(|o| **o != Op::Add).count();
        let b_len = ops[start..end].iter().filter(|o| **o != Op::Del).count();
        let a_start = if a_len == 0 { a0 } else { a0 + 1 };
        let b_start = if b_len == 0 { b0 } else { b0 + 1 };
        let _ = writeln!(out, "@@ -{a_start},{a_len} +{b_start},{b_len} @@");
        for (idx, op) in ops.iter().enumerate().take(end).skip(start) {
            let (ai, bi) = pos[idx];
            match op {
                Op::Keep => push_line(&mut out, ' ', a[ai]),
                Op::Del => push_line(&mut out, '-', a[ai]),
                Op::Add => push_line(&mut out, '+', b[bi]),
            }
        }
    }
    out
}

/// One file's hunks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePatch {
    pub old_path: String,
    pub new_path: String,
    pub hunks: Vec<Hunk>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// 1-based first old line (0 when the hunk has no old lines).
    pub old_start: usize,
    pub old_len: usize,
    pub new_start: usize,
    pub new_len: usize,
    /// (tag, text with its newline unless "no newline at end of file" followed it).
    pub lines: Vec<(char, String)>,
}

fn strip_ab(p: &str) -> String {
    let p = p.split('\t').next().unwrap_or(p).trim_end();
    p.strip_prefix("a/").or_else(|| p.strip_prefix("b/")).unwrap_or(p).to_string()
}

fn range(s: &str, at: usize) -> Result<(usize, usize), DiffError> {
    let bad = || DiffError::Malformed(at, format!("bad range `{s}`"));
    let (start, len) = match s.split_once(',') {
        Some((a, b)) => (a.parse().map_err(|_| bad())?, b.parse().map_err(|_| bad())?),
        None => (s.parse().map_err(|_| bad())?, 1),
    };
    Ok((start, len))
}

/// Parses a unified diff (one or more files).
pub fn parse(text: &str) -> Result<Vec<FilePatch>, DiffError> {
    let src: Vec<&str> = text.split_inclusive('\n').collect();
    let mut files: Vec<FilePatch> = Vec::new();
    let mut i = 0;
    while i < src.len() {
        let line = src[i];
        if let Some(old) = line.strip_prefix("--- ") {
            let new = src
                .get(i + 1)
                .and_then(|l| l.strip_prefix("+++ "))
                .ok_or_else(|| DiffError::Malformed(i + 2, "`---` without `+++`".into()))?;
            files.push(FilePatch { old_path: strip_ab(old), new_path: strip_ab(new), hunks: Vec::new() });
            i += 2;
            continue;
        }
        if let Some(rest) = line.strip_prefix("@@ ") {
            let file = files.last_mut().ok_or_else(|| DiffError::Malformed(i + 1, "hunk before a file header".into()))?;
            let spec = rest.split(" @@").next().unwrap_or("");
            let mut parts = spec.split_whitespace();
            let (o, n) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            let (old_start, old_len) = range(o.trim_start_matches('-'), i + 1)?;
            let (new_start, new_len) = range(n.trim_start_matches('+'), i + 1)?;
            let mut hunk = Hunk { old_start, old_len, new_start, new_len, lines: Vec::new() };
            let (mut seen_old, mut seen_new) = (0, 0);
            i += 1;
            while i < src.len() && (seen_old < old_len || seen_new < new_len || src[i].starts_with('\\')) {
                let l = src[i];
                if l.starts_with('\\') {
                    if let Some(last) = hunk.lines.last_mut() {
                        if last.1.ends_with('\n') {
                            last.1.pop();
                        }
                    }
                    i += 1;
                    continue;
                }
                let tag = l.chars().next().unwrap_or(' ');
                let body = if l.is_empty() { "" } else { &l[1..] };
                // A bare newline is an empty context line.
                let (tag, body) = if l == "\n" { (' ', "\n") } else { (tag, body) };
                match tag {
                    ' ' => {
                        seen_old += 1;
                        seen_new += 1;
                    }
                    '-' => seen_old += 1,
                    '+' => seen_new += 1,
                    _ => return Err(DiffError::Malformed(i + 1, format!("unexpected `{}`", l.trim_end()))),
                }
                hunk.lines.push((tag, body.to_string()));
                i += 1;
            }
            if seen_old != old_len || seen_new != new_len {
                return Err(DiffError::Malformed(i, "hunk is shorter than its header".into()));
            }
            file.hunks.push(hunk);
            continue;
        }
        i += 1;
    }
    Ok(files)
}

/// Applies `patch` to `old` at the hunks' stated lines, matching every context and removed line
/// exactly.
pub fn apply(old: &str, patch: &FilePatch) -> Result<String, DiffError> {
    let a = lines(old);
    let mut out = String::with_capacity(old.len());
    let mut at = 0usize;
    for (h_idx, h) in patch.hunks.iter().enumerate() {
        let start = if h.old_len == 0 { h.old_start } else { h.old_start.saturating_sub(1) };
        if start < at || start > a.len() {
            return Err(DiffError::Mismatch(h_idx + 1, h.old_start));
        }
        for l in &a[at..start] {
            out.push_str(l);
        }
        let mut cur = start;
        for (tag, text) in &h.lines {
            match tag {
                ' ' | '-' => {
                    if a.get(cur) != Some(&text.as_str()) {
                        return Err(DiffError::Mismatch(h_idx + 1, cur + 1));
                    }
                    if *tag == ' ' {
                        out.push_str(text);
                    }
                    cur += 1;
                }
                _ => out.push_str(text),
            }
        }
        at = cur;
    }
    for l in &a[at..] {
        out.push_str(l);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(old: &str, new: &str) {
        let d = unified("f.rs", old, new, 3);
        let p = parse(&d).unwrap();
        if old == new {
            assert!(p.is_empty());
            return;
        }
        assert_eq!(p.len(), 1, "{d}");
        assert_eq!(apply(old, &p[0]).unwrap(), new, "diff:\n{d}");
    }

    #[test]
    fn edits_apply_in_position_order_and_refuse_overlaps() {
        let t = "fn old() { old(); }\n";
        let e = |s, e, n: &str| TextEdit { start: s, end: e, new_text: n.into() };
        assert_eq!(apply_edits(t, &[e(11, 14, "new"), e(3, 6, "new")]).unwrap(), "fn new() { new(); }\n");
        assert_eq!(apply_edits(t, &[e(3, 6, "x"), e(4, 8, "y")]), Err(DiffError::Overlap(4)));
        assert!(matches!(apply_edits(t, &[e(3, 99, "x")]), Err(DiffError::OutOfRange(..))));
        assert!(matches!(apply_edits("é", &[e(1, 2, "x")]), Err(DiffError::OutOfRange(..))));
        assert_eq!(apply_edits("ab", &[e(1, 1, "1"), e(1, 1, "2")]).unwrap(), "a12b");
    }

    #[test]
    fn unified_diffs_round_trip_through_parse_and_apply() {
        let base: String = (1..=40).map(|i| format!("line {i}\n")).collect();
        let mut changed = base.replace("line 5\n", "line five\n").replace("line 30\n", "");
        changed.push_str("tail\n");
        roundtrip(&base, &changed);
        roundtrip(&base, &base);
        roundtrip("", "a\nb\n");
        roundtrip("a\nb\n", "");
        roundtrip("a\nb", "a\nc");
        roundtrip("a\nb\n", "a\nb");
        roundtrip("x\r\ny\r\n", "x\r\nz\r\n");
        roundtrip("same\n\nblank lines\n\n", "same\n\nchanged\n\n");
    }

    #[test]
    fn the_diff_reads_like_git() {
        let d = unified("src/a.rs", "a\nb\nc\n", "a\nB\nc\n", 3);
        assert_eq!(d, "--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n");
    }

    #[test]
    fn a_stale_diff_fails_instead_of_landing_elsewhere() {
        let d = unified("f", "a\nb\nc\n", "a\nX\nc\n", 1);
        let p = &parse(&d).unwrap()[0];
        assert!(matches!(apply("a\nq\nc\n", p), Err(DiffError::Mismatch(1, 2))));
    }

    #[test]
    fn far_apart_changes_make_separate_hunks() {
        let base: String = (1..=50).map(|i| format!("L{i}\n")).collect();
        let new = base.replace("L3\n", "three\n").replace("L45\n", "forty-five\n");
        let d = unified("f", &base, &new, 3);
        assert_eq!(d.matches("@@ -").count(), 2, "{d}");
        roundtrip(&base, &new);
    }
}
