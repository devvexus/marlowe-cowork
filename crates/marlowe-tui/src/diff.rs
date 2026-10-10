//! Inline diffs (U1): hunks in the approval modal, under edit results, in the transcript view and
//! in the `/diff` viewer. Palette tokens only: green adds, red removes, dim context, and the
//! changed run inside a changed line in bold. Never a background fill.
//!
//! A diff comes from one of three places:
//!
//! - a unified diff in text ([`parse_unified`]): the `/diff` reply, a tool result that carries one.
//!   A tool's `summary` on the wire may have more than one line; the first is the tool line, the
//!   rest is its detail (a diff, or output for Ctrl+O).
//! - a tool call's arguments ([`from_tool_call`]): `edit`, `multi_edit`, `write`, which is what the
//!   approval modal and a replayed transcript have.
//! - two strings ([`from_replace`]), which has no file line numbers to show.
//!
//! Everything shown is file content or model text, so it goes through `text::clean`.

use marlowe_surface::chrome::Ink;
use marlowe_surface::Theme;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use serde_json::Value;

use crate::text;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Context,
    Add,
    Del,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: Kind,
    pub text: String,
    /// Line numbers, when the diff knows them.
    pub old: Option<u32>,
    pub new: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Hunk {
    /// The `@@ -a,b +c,d @@ context` header, when the diff has real line numbers.
    pub header: Option<String>,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileDiff {
    pub path: String,
    pub new_file: bool,
    pub deleted: bool,
    pub binary: bool,
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    pub fn stats(&self) -> (u32, u32) {
        let mut add = 0;
        let mut del = 0;
        for l in self.hunks.iter().flat_map(|h| &h.lines) {
            match l.kind {
                Kind::Add => add += 1,
                Kind::Del => del += 1,
                Kind::Context => {}
            }
        }
        (add, del)
    }

    /// The first line of the first hunk on the new side, for a `path:line` link.
    pub fn first_line(&self) -> Option<u32> {
        self.hunks.iter().flat_map(|h| &h.lines).find(|l| l.kind != Kind::Del).and_then(|l| l.new)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Diff {
    pub files: Vec<FileDiff>,
}

impl Diff {
    pub fn stats(&self) -> (u32, u32) {
        self.files.iter().map(FileDiff::stats).fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
    }

    pub fn is_empty(&self) -> bool {
        self.files.iter().all(|f| f.hunks.is_empty() && !f.binary && !f.new_file && !f.deleted)
    }
}

/// Whether text reads as a unified diff: a hunk header, or `---`/`+++` file headers.
pub fn looks_like_diff(s: &str) -> bool {
    let mut minus = false;
    for l in s.lines() {
        if l.starts_with("@@ ") && l[3..].starts_with('-') {
            return true;
        }
        if l.starts_with("diff --git ") {
            return true;
        }
        if l.starts_with("--- ") {
            minus = true;
        } else if minus && l.starts_with("+++ ") {
            return true;
        } else {
            minus = false;
        }
    }
    false
}

/// Parses a unified diff (git or plain). Anything before the first file header is ignored.
pub fn parse_unified(s: &str) -> Option<Diff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut cur: Option<FileDiff> = None;
    let (mut old_no, mut new_no) = (0u32, 0u32);
    let mut in_hunk = false;
    let flush = |files: &mut Vec<FileDiff>, cur: &mut Option<FileDiff>| {
        if let Some(f) = cur.take() {
            files.push(f);
        }
    };
    let lines: Vec<&str> = s.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i];
        if let Some(rest) = l.strip_prefix("diff --git ") {
            flush(&mut files, &mut cur);
            let path = rest.rsplit_once(" b/").map(|(_, b)| b).unwrap_or(rest).trim().to_string();
            cur = Some(FileDiff { path, ..Default::default() });
            in_hunk = false;
        } else if l.starts_with("new file mode") {
            if let Some(f) = cur.as_mut() {
                f.new_file = true;
            }
        } else if l.starts_with("deleted file mode") {
            if let Some(f) = cur.as_mut() {
                f.deleted = true;
            }
        } else if l.starts_with("Binary files ") || l.starts_with("GIT binary patch") {
            if let Some(f) = cur.as_mut() {
                f.binary = true;
            }
        } else if l.starts_with("--- ") && !in_hunk_body(&lines, i, in_hunk) {
            // A plain diff has no `diff --git` line: the `---`/`+++` pair opens a file.
            let next = lines.get(i + 1).copied().unwrap_or("");
            if next.starts_with("+++ ") {
                let old = path_of(&l[4..]);
                let new = path_of(&next[4..]);
                let start_new = cur.as_ref().is_none_or(|f| !f.hunks.is_empty() || f.path.is_empty());
                if start_new {
                    flush(&mut files, &mut cur);
                    cur = Some(FileDiff::default());
                }
                if let Some(f) = cur.as_mut() {
                    if old == "/dev/null" {
                        f.new_file = true;
                    }
                    if new == "/dev/null" {
                        f.deleted = true;
                    }
                    f.path = if new == "/dev/null" { old } else { new };
                }
                in_hunk = false;
                i += 1;
            }
        } else if let Some(rest) = l.strip_prefix("@@ ") {
            if let Some((o, n)) = hunk_starts(rest) {
                old_no = o;
                new_no = n;
                if cur.is_none() {
                    cur = Some(FileDiff::default());
                }
                if let Some(f) = cur.as_mut() {
                    f.hunks.push(Hunk { header: Some(l.trim_end().to_string()), lines: Vec::new() });
                }
                in_hunk = true;
            }
        } else if in_hunk {
            let Some(f) = cur.as_mut() else { i += 1; continue };
            let Some(h) = f.hunks.last_mut() else { i += 1; continue };
            if let Some(t) = l.strip_prefix('+') {
                h.lines.push(DiffLine { kind: Kind::Add, text: t.to_string(), old: None, new: Some(new_no) });
                new_no += 1;
            } else if let Some(t) = l.strip_prefix('-') {
                h.lines.push(DiffLine { kind: Kind::Del, text: t.to_string(), old: Some(old_no), new: None });
                old_no += 1;
            } else if l.starts_with('\\') {
                // "\ No newline at end of file"
            } else {
                let t = l.strip_prefix(' ').unwrap_or(l);
                h.lines.push(DiffLine { kind: Kind::Context, text: t.to_string(), old: Some(old_no), new: Some(new_no) });
                old_no += 1;
                new_no += 1;
            }
        }
        i += 1;
    }
    flush(&mut files, &mut cur);
    let d = Diff { files };
    (!d.files.is_empty() && !d.is_empty()).then_some(d)
}

/// Inside a hunk a `--- ` line is a removed line starting `-- `, not a file header, unless the
/// next line is `+++ `. The caller checks the pair; this only guards the hunk body.
fn in_hunk_body(lines: &[&str], i: usize, in_hunk: bool) -> bool {
    in_hunk && !lines.get(i + 1).is_some_and(|n| n.starts_with("+++ "))
}

/// `a/src/x.rs` or `b/src/x.rs` or `src/x.rs\t2024-..` to `src/x.rs`.
fn path_of(s: &str) -> String {
    let s = s.split('\t').next().unwrap_or(s).trim();
    let s = s.strip_prefix("a/").or_else(|| s.strip_prefix("b/")).unwrap_or(s);
    s.trim_matches('"').to_string()
}

/// The two start lines of `-a,b +c,d @@ ...`.
fn hunk_starts(rest: &str) -> Option<(u32, u32)> {
    let mut parts = rest.split_whitespace();
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let num = |s: &str| s.split(',').next().and_then(|n| n.parse::<u32>().ok());
    Some((num(old)?, num(new)?))
}

/// The most cells the line diff may use before an edit is shown as plain remove-then-add.
const LCS_CELLS: usize = 4_000_000;
const CONTEXT: usize = 3;

/// A diff of two strings, as an edit tool's `old_string`/`new_string` pair. No file line numbers.
pub fn from_replace(path: &str, old: &str, new: &str) -> Diff {
    let a: Vec<&str> = if old.is_empty() { Vec::new() } else { old.lines().collect() };
    let b: Vec<&str> = if new.is_empty() { Vec::new() } else { new.lines().collect() };
    let ops = line_ops(&a, &b);
    Diff {
        files: vec![FileDiff {
            path: path.to_string(),
            new_file: old.is_empty() && !new.is_empty(),
            deleted: false,
            binary: false,
            hunks: hunks_of(&ops, false),
        }],
    }
}

/// A whole new file's content as additions.
pub fn from_write(path: &str, content: &str) -> Diff {
    let lines: Vec<DiffLine> = content
        .lines()
        .enumerate()
        .map(|(i, t)| DiffLine { kind: Kind::Add, text: t.to_string(), old: None, new: Some(i as u32 + 1) })
        .collect();
    Diff {
        files: vec![FileDiff {
            path: path.to_string(),
            new_file: true,
            hunks: vec![Hunk { header: None, lines }],
            ..Default::default()
        }],
    }
}

/// The diff an edit-family tool call stands for. `args` is the call's JSON.
pub fn from_tool_call(tool: &str, args: &Value) -> Option<Diff> {
    let s = |v: &Value, keys: &[&str]| -> Option<String> {
        keys.iter().find_map(|k| v.get(*k).and_then(Value::as_str)).map(str::to_string)
    };
    let path_keys = ["path", "file_path", "file"];
    match tool {
        "edit" | "str_replace" => {
            let path = s(args, &path_keys)?;
            let old = s(args, &["old_string", "old"])?;
            let new = s(args, &["new_string", "new"])?;
            Some(from_replace(&path, &old, &new))
        }
        "multi_edit" => {
            let edits = args.get("edits")?.as_array()?;
            let default_path = s(args, &path_keys);
            let mut files: Vec<FileDiff> = Vec::new();
            for e in edits {
                let path = s(e, &path_keys).or_else(|| default_path.clone())?;
                let old = s(e, &["old_string", "old"])?;
                let new = s(e, &["new_string", "new"])?;
                let d = from_replace(&path, &old, &new);
                match files.iter_mut().find(|f| f.path == path) {
                    Some(f) => f.hunks.extend(d.files.into_iter().flat_map(|f| f.hunks)),
                    None => files.extend(d.files),
                }
            }
            (!files.is_empty()).then_some(Diff { files })
        }
        "write" | "create" => {
            let path = s(args, &path_keys)?;
            let content = s(args, &["content", "text", "contents"])?;
            Some(from_write(&path, &content))
        }
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Keep,
    Del,
    Add,
}

/// Line-level edit script by longest common subsequence, with common head and tail peeled off so
/// a small edit in a long text stays cheap.
fn line_ops(a: &[&str], b: &[&str]) -> Vec<(Op, String)> {
    let head = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let tail = a[head..].iter().rev().zip(b[head..].iter().rev()).take_while(|(x, y)| x == y).count();
    let (am, bm) = (&a[head..a.len() - tail], &b[head..b.len() - tail]);
    let mut out: Vec<(Op, String)> = a[..head].iter().map(|l| (Op::Keep, l.to_string())).collect();
    if am.len().saturating_mul(bm.len()) > LCS_CELLS {
        out.extend(am.iter().map(|l| (Op::Del, l.to_string())));
        out.extend(bm.iter().map(|l| (Op::Add, l.to_string())));
    } else {
        let (n, m) = (am.len(), bm.len());
        // lcs[i][j]: length of the LCS of am[i..] and bm[j..].
        let mut lcs = vec![0u32; (n + 1) * (m + 1)];
        let at = |i: usize, j: usize| i * (m + 1) + j;
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                lcs[at(i, j)] = if am[i] == bm[j] { lcs[at(i + 1, j + 1)] + 1 } else { lcs[at(i + 1, j)].max(lcs[at(i, j + 1)]) };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n || j < m {
            if i < n && j < m && am[i] == bm[j] {
                out.push((Op::Keep, am[i].to_string()));
                i += 1;
                j += 1;
            } else if i < n && (j == m || lcs[at(i + 1, j)] >= lcs[at(i, j + 1)]) {
                out.push((Op::Del, am[i].to_string()));
                i += 1;
            } else {
                out.push((Op::Add, bm[j].to_string()));
                j += 1;
            }
        }
    }
    out.extend(a[a.len() - tail..].iter().map(|l| (Op::Keep, l.to_string())));
    out
}

/// Groups an edit script into hunks with [`CONTEXT`] lines around each change.
fn hunks_of(ops: &[(Op, String)], numbered: bool) -> Vec<Hunk> {
    let changed: Vec<usize> = ops.iter().enumerate().filter(|(_, (o, _))| *o != Op::Keep).map(|(i, _)| i).collect();
    if changed.is_empty() {
        return Vec::new();
    }
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for &c in &changed {
        let (lo, hi) = (c.saturating_sub(CONTEXT), (c + CONTEXT + 1).min(ops.len()));
        match ranges.last_mut() {
            Some(last) if lo <= last.1 => last.1 = hi,
            _ => ranges.push((lo, hi)),
        }
    }
    ranges
        .into_iter()
        .map(|(lo, hi)| {
            let lines = ops[lo..hi]
                .iter()
                .map(|(o, t)| DiffLine {
                    kind: match o {
                        Op::Keep => Kind::Context,
                        Op::Del => Kind::Del,
                        Op::Add => Kind::Add,
                    },
                    text: t.clone(),
                    old: None,
                    new: None,
                })
                .collect();
            Hunk { header: numbered.then(String::new), lines }
        })
        .collect()
}

/// What [`render`] produced.
pub struct Rendered {
    pub lines: Vec<Line<'static>>,
    /// Rows left out by the cap.
    pub hidden: usize,
}

/// How to lay a diff out.
#[derive(Debug, Clone, Copy)]
pub struct Layout {
    pub width: usize,
    /// At most this many rows (`None`: all).
    pub max_rows: Option<usize>,
    /// Wrap long lines (the transcript view) instead of cutting them with `…`.
    pub wrap: bool,
    /// The file header rows.
    pub headers: bool,
}

impl Layout {
    pub fn inline(width: usize) -> Self {
        Self { width, max_rows: Some(24), wrap: false, headers: true }
    }

    pub fn full(width: usize) -> Self {
        Self { width, max_rows: None, wrap: true, headers: true }
    }
}

pub fn render(diff: &Diff, lay: Layout, theme: &Theme) -> Rendered {
    let dimmer = Ink::Dimmer.style(theme);
    let dim = Ink::Dim.style(theme);
    let green = Ink::Green.style(theme);
    let red = Ink::Red.style(theme);
    let mut out: Vec<Line<'static>> = Vec::new();
    let numbered = diff.files.iter().flat_map(|f| &f.hunks).flat_map(|h| &h.lines).any(|l| l.old.is_some() || l.new.is_some());
    let max_no = diff
        .files
        .iter()
        .flat_map(|f| &f.hunks)
        .flat_map(|h| &h.lines)
        .filter_map(|l| l.old.max(l.new))
        .max()
        .unwrap_or(0);
    let gw = max_no.to_string().len().max(2);
    let gutter = if numbered { gw * 2 + 3 } else { 0 };
    let body_w = lay.width.saturating_sub(gutter + 2).max(8);

    for f in &diff.files {
        if lay.headers {
            let (a, d) = f.stats();
            let mut spans = vec![Span::styled(text::clean(&f.path), Ink::Body.style(theme).add_modifier(Modifier::BOLD))];
            if f.new_file {
                spans.push(Span::styled("  new file", dimmer));
            } else if f.deleted {
                spans.push(Span::styled("  deleted", dimmer));
            } else if f.binary {
                spans.push(Span::styled("  binary", dimmer));
            }
            if a > 0 {
                spans.push(Span::styled(format!("  +{a}"), green));
            }
            if d > 0 {
                spans.push(Span::styled(format!(" −{d}"), red));
            }
            out.push(Line::from(spans));
        }
        for h in &f.hunks {
            if let Some(hd) = h.header.as_deref().filter(|s| !s.is_empty()) {
                out.push(Line::from(Span::styled(text::truncate(&text::clean(hd), lay.width), dimmer)));
            }
            let emphasis = emphasis_for(&h.lines);
            for (i, l) in h.lines.iter().enumerate() {
                let (sign, style) = match l.kind {
                    Kind::Add => ('+', green),
                    Kind::Del => ('−', red),
                    Kind::Context => (' ', dim),
                };
                let gut = if numbered {
                    let n = |v: Option<u32>| v.map_or(" ".repeat(gw), |n| format!("{n:>gw$}"));
                    format!("{} {} ", n(l.old), n(l.new))
                } else {
                    String::new()
                };
                let clean = text::clean(&l.text);
                let pieces: Vec<String> = if lay.wrap && text::width(&clean) > body_w {
                    text::wrap(&clean, body_w)
                } else {
                    vec![text::truncate(&clean, body_w)]
                };
                for (k, piece) in pieces.into_iter().enumerate() {
                    let lead = if k == 0 { format!("{sign} ") } else { "  ".to_string() };
                    let g = if k == 0 { gut.clone() } else { " ".repeat(gutter) };
                    let mut spans = vec![Span::styled(g, dimmer), Span::styled(lead, style)];
                    match (k, emphasis.get(i).copied().flatten()) {
                        (0, Some((from, to))) if to > from => spans.extend(emphasised(&piece, from, to, style)),
                        _ => spans.push(Span::styled(piece, style)),
                    }
                    out.push(Line::from(spans));
                }
            }
        }
    }
    match lay.max_rows {
        Some(max) if out.len() > max => {
            let hidden = out.len() - max;
            out.truncate(max);
            Rendered { lines: out, hidden }
        }
        _ => Rendered { lines: out, hidden: 0 },
    }
}

/// Splits `s` at char offsets `[from, to)` into plain, bold, plain.
fn emphasised(s: &str, from: usize, to: usize, style: ratatui::style::Style) -> Vec<Span<'static>> {
    let chars: Vec<char> = s.chars().collect();
    let (from, to) = (from.min(chars.len()), to.min(chars.len()));
    let part = |a: usize, b: usize| chars[a..b].iter().collect::<String>();
    let mut v = Vec::new();
    if from > 0 {
        v.push(Span::styled(part(0, from), style));
    }
    v.push(Span::styled(part(from, to), style.add_modifier(Modifier::BOLD)));
    if to < chars.len() {
        v.push(Span::styled(part(to, chars.len()), style));
    }
    v
}

/// For each line of a hunk, the char range that differs from its partner when a run of removed
/// lines is followed by the same number of added lines. Lines that share nothing get none: the
/// whole line already says it.
fn emphasis_for(lines: &[DiffLine]) -> Vec<Option<(usize, usize)>> {
    let mut out = vec![None; lines.len()];
    let mut i = 0;
    while i < lines.len() {
        if lines[i].kind != Kind::Del {
            i += 1;
            continue;
        }
        let d0 = i;
        while i < lines.len() && lines[i].kind == Kind::Del {
            i += 1;
        }
        let a0 = i;
        while i < lines.len() && lines[i].kind == Kind::Add {
            i += 1;
        }
        let (nd, na) = (a0 - d0, i - a0);
        if nd == 0 || nd != na {
            continue;
        }
        for k in 0..nd {
            let (old, new) = (&lines[d0 + k].text, &lines[a0 + k].text);
            let (oc, nc): (Vec<char>, Vec<char>) = (old.chars().collect(), new.chars().collect());
            let pre = oc.iter().zip(&nc).take_while(|(a, b)| a == b).count();
            let suf = oc[pre..].iter().rev().zip(nc[pre..].iter().rev()).take_while(|(a, b)| a == b).count();
            if pre == 0 && suf == 0 {
                continue;
            }
            out[d0 + k] = Some((pre, oc.len() - suf));
            out[a0 + k] = Some((pre, nc.len() - suf));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIT: &str = "diff --git a/src/lib.rs b/src/lib.rs\nindex 1..2 100644\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,4 +1,4 @@ fn main\n fn a() {}\n-fn b() {}\n+fn b2() {}\n fn c() {}\n fn d() {}\ndiff --git a/new.txt b/new.txt\nnew file mode 100644\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+one\n+two\n";

    #[test]
    fn parses_a_git_diff_with_numbers_and_a_new_file() {
        let d = parse_unified(GIT).unwrap();
        assert_eq!(d.files.len(), 2);
        assert_eq!(d.files[0].path, "src/lib.rs");
        assert_eq!(d.files[0].stats(), (1, 1));
        let h = &d.files[0].hunks[0];
        assert_eq!((h.lines[1].kind, h.lines[1].old, h.lines[1].new), (Kind::Del, Some(2), None));
        assert_eq!((h.lines[2].kind, h.lines[2].old, h.lines[2].new), (Kind::Add, None, Some(2)));
        assert!(d.files[1].new_file);
        assert_eq!(d.files[1].path, "new.txt");
        assert_eq!(d.stats(), (3, 1));
    }

    #[test]
    fn a_removed_line_that_starts_with_dashes_is_not_a_file_header() {
        let d = parse_unified("--- a/x.md\n+++ b/x.md\n@@ -1,2 +1,1 @@\n--- not a header\n keep\n").unwrap();
        assert_eq!(d.files.len(), 1);
        assert_eq!(d.files[0].hunks[0].lines[0].text, "-- not a header");
    }

    #[test]
    fn detects_diffs_and_rejects_prose() {
        assert!(looks_like_diff(GIT));
        assert!(looks_like_diff("--- a/x\n+++ b/x\n"));
        assert!(!looks_like_diff("the answer is -3 and +4\n--- a list\n- item"));
    }

    #[test]
    fn a_replace_pair_is_a_small_hunk_with_context() {
        let d = from_replace("a.rs", "one\ntwo\nthree\nfour", "one\ntwo\n3\nfour");
        let h = &d.files[0].hunks[0];
        let kinds: Vec<_> = h.lines.iter().map(|l| l.kind).collect();
        assert_eq!(kinds, vec![Kind::Context, Kind::Context, Kind::Del, Kind::Add, Kind::Context]);
        assert_eq!(d.stats(), (1, 1));
    }

    #[test]
    fn tool_calls_become_diffs() {
        let e = serde_json::json!({"path": "src/x.rs", "old_string": "a\nb", "new_string": "a\nc"});
        assert_eq!(from_tool_call("edit", &e).unwrap().stats(), (1, 1));
        let w = serde_json::json!({"path": "n.txt", "content": "x\ny\nz"});
        let d = from_tool_call("write", &w).unwrap();
        assert!(d.files[0].new_file);
        assert_eq!(d.stats(), (3, 0));
        let m = serde_json::json!({"edits": [
            {"path": "a", "old_string": "1", "new_string": "2"},
            {"path": "b", "old_string": "x", "new_string": "y"},
            {"path": "a", "old_string": "p", "new_string": "q"}]});
        let d = from_tool_call("multi_edit", &m).unwrap();
        assert_eq!(d.files.len(), 2);
        assert_eq!(d.files[0].hunks.len(), 2);
        assert!(from_tool_call("read", &e).is_none());
    }

    #[test]
    fn the_changed_run_is_bold_and_the_rest_is_not() {
        let d = from_replace("a.rs", "let total = a + b;", "let total = a - b;");
        let r = render(&d, Layout::inline(80), &Theme::default_truecolor());
        let bold: Vec<String> = r
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter(|s| s.style.add_modifier.contains(Modifier::BOLD))
            .map(|s| s.content.to_string())
            .collect();
        assert!(bold.contains(&"+".to_string()) && bold.contains(&"-".to_string()), "{bold:?}");
    }

    #[test]
    fn the_cap_reports_what_it_hid() {
        let big: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let d = from_write("big.txt", &big);
        let r = render(&d, Layout::inline(80), &Theme::default_truecolor());
        assert_eq!(r.lines.len(), 24);
        assert_eq!(r.hidden, 101 - 24);
    }
}
