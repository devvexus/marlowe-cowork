//! Failure-aware output summaries with the full log on disk (design 2.1 T32). Track B.
//!
//! A command's output reaches the model at most [`BUDGET`] bytes at a time. Within it:
//!
//! - Output that fits is returned as it is, whatever its line lengths (progress-bar redraws and
//!   colour codes removed).
//! - Longer output keeps its head (what was run, versions), its tail (where failures and totals
//!   print), and, for a failed command, the lines that say why: error, panic, traceback and test
//!   failure blocks from the middle, each with its neighbours, in file order, until the budget is
//!   spent. Runs of identical lines collapse. Every gap is marked with the lines it hides.
//! - `raw: true` skips the selection and cuts the middle (head and tail only).
//!
//! Whenever something was left out the caller adds `[full output: <path>, N lines]`: the log of
//! the whole run, which `read` (with `offset`/`limit`) or `grep` can open.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use regex::Regex;

/// Most bytes of command output one result carries.
pub const BUDGET: usize = 30_000;

/// A line longer than this is cut, but only in output that is over the budget anyway.
const MAX_LINE: usize = 400;

/// What [`summarize`] and [`middle_cut`] return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub text: String,
    /// Lines in the whole output.
    pub total_lines: usize,
    /// Something was left out: point at the full log.
    pub cut: bool,
}

/// Output as the model should read it: `\r\n` and bare `\r` redraws resolved (a progress bar
/// keeps its last state), ANSI escape sequences removed, trailing whitespace trimmed.
pub fn clean(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for (i, line) in raw.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let line = line.strip_suffix('\r').unwrap_or(line);
        let line = line.rsplit('\r').find(|s| !s.is_empty()).unwrap_or("");
        strip_ansi(line, &mut out);
    }
    let trimmed = out.trim_end().len();
    out.truncate(trimmed);
    out
}

fn strip_ansi(line: &str, out: &mut String) {
    if !line.contains('\x1b') {
        out.push_str(line);
        return;
    }
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            // CSI: ESC [ params final-byte(@..~)
            Some('[') => {
                chars.next();
                for n in chars.by_ref() {
                    if ('@'..='~').contains(&n) {
                        break;
                    }
                }
            }
            // OSC: ESC ] ... BEL or ESC \
            Some(']') => {
                chars.next();
                while let Some(n) = chars.next() {
                    if n == '\x07' {
                        break;
                    }
                    if n == '\x1b' {
                        chars.next();
                        break;
                    }
                }
            }
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
}

fn salient() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(error|errors|failed|failure|failures|fatal|panic|panicked|exception|traceback|segfault|segmentation fault|assertion|assertionerror|unresolved|undefined|cannot|could not|not found|permission denied|timed out|timeout|no such file|abort(ed)?)\b|^E\s|^FAILED|^---- .* ----$|^\s*(thread '.*' panicked|stack backtrace)|\berror\[E\d+\]|^npm ERR!|^\s*at .+:\d+:\d+\)?$",
        )
        .expect("static pattern")
    })
}

fn warning() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\b(warn|warning|warnings|deprecated)\b").expect("static pattern"))
}

fn clip(line: &str) -> String {
    if line.len() <= MAX_LINE {
        return line.to_string();
    }
    let mut cut = MAX_LINE;
    while !line.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}… [{} more bytes]", &line[..cut], line.len() - cut)
}

/// Lines with runs of four or more identical lines collapsed to one plus a count.
fn collapse_repeats(lines: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let mut j = i + 1;
        while j < lines.len() && lines[j] == lines[i] {
            j += 1;
        }
        let run = j - i;
        out.push(lines[i].to_string());
        if run >= 4 {
            out.push(format!("[... the line above repeated {} more times]", run - 1));
        } else {
            for _ in 1..run {
                out.push(lines[i].to_string());
            }
        }
        i = j;
    }
    out
}

/// `text` (already [`clean`]ed) within `budget` bytes, chosen as described in the module docs.
/// `failed` says the command did not succeed, which spends the middle on failure blocks instead
/// of warnings.
pub fn summarize(text: &str, failed: bool, budget: usize) -> Summary {
    let all: Vec<&str> = text.split('\n').collect();
    let total_lines = if text.is_empty() { 0 } else { all.len() };
    // Output that fits comes back whole, long lines included (a minified JSON file, a one-line
    // compiler error): the log is deleted when nothing was cut, so nothing may be.
    if text.len() <= budget {
        return Summary { text: text.to_string(), total_lines, cut: false };
    }
    let lines = collapse_repeats(&all);
    let size = |i: usize| lines[i].len().min(MAX_LINE + 24) + 1;
    let total: usize = (0..lines.len()).map(size).sum();
    if total <= budget {
        let text = lines.iter().map(|l| clip(l)).collect::<Vec<_>>().join("\n");
        // A clipped line is a cut too: the log must stay for the bytes `clip` replaced.
        let clipped = lines.iter().any(|l| l.len() > MAX_LINE);
        return Summary { text, total_lines, cut: clipped || total_lines != lines.len() };
    }

    // Reserve room for the gap markers, then split the rest: a failure wants its tail and its
    // blocks; a success wants a long head and tail.
    let usable = budget.saturating_sub(1_500);
    let (head_b, tail_b) = if failed { (usable / 10, usable * 45 / 100) } else { (usable * 30 / 100, usable * 55 / 100) };
    let mid_b = usable - head_b - tail_b;

    let mut keep: BTreeSet<usize> = BTreeSet::new();
    let mut used = 0usize;
    let mut head_end = 0usize;
    while head_end < lines.len() && used + size(head_end) <= head_b {
        keep.insert(head_end);
        used += size(head_end);
        head_end += 1;
    }
    let mut tail_start = lines.len();
    let mut tail_used = 0usize;
    while tail_start > head_end && tail_used + size(tail_start - 1) <= tail_b {
        tail_start -= 1;
        keep.insert(tail_start);
        tail_used += size(tail_start);
    }

    // Blocks from the middle: the matching line, one line before, and up to eight after it (to
    // the next blank line).
    let re = if failed { salient() } else { warning() };
    let mut mid_used = 0usize;
    let mut blocks_dropped = 0usize;
    let mut i = head_end;
    while i < tail_start {
        if !re.is_match(&lines[i]) {
            i += 1;
            continue;
        }
        let from = i.saturating_sub(1).max(head_end);
        let mut to = i;
        while to + 1 < tail_start && to - i < 8 && !lines[to + 1].trim().is_empty() {
            to += 1;
        }
        let cost: usize = (from..=to).filter(|k| !keep.contains(k)).map(size).sum();
        if mid_used + cost > mid_b {
            blocks_dropped += 1;
            i = to + 1;
            continue;
        }
        keep.extend(from..=to);
        mid_used += cost;
        i = to + 1;
    }

    // What the blocks left unused goes back as more context: two lines before the tail for every
    // one after the head.
    let mut spent: usize = keep.iter().map(|&k| size(k)).sum();
    let (mut lo, mut hi) = (head_end, tail_start);
    loop {
        let mut grew = false;
        for step in 0..3 {
            if lo >= hi {
                break;
            }
            let at = if step < 2 { hi - 1 } else { lo };
            if keep.contains(&at) {
                if step < 2 { hi -= 1 } else { lo += 1 }
                grew = true;
            } else if spent + size(at) <= usable {
                keep.insert(at);
                spent += size(at);
                if step < 2 { hi -= 1 } else { lo += 1 }
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    let mut out = String::with_capacity(budget);
    let mut prev: Option<usize> = None;
    for &k in &keep {
        if let Some(p) = prev {
            if k > p + 1 {
                out.push_str(&format!("[... {} lines omitted ...]\n", k - p - 1));
            }
        }
        out.push_str(&clip(&lines[k]));
        out.push('\n');
        prev = Some(k);
    }
    if let Some(p) = prev {
        if p + 1 < lines.len() {
            out.push_str(&format!("[... {} lines omitted ...]\n", lines.len() - p - 1));
        }
    }
    if blocks_dropped > 0 {
        let what = if failed { "more error blocks" } else { "more warning blocks" };
        out.push_str(&format!("[... {blocks_dropped} {what} did not fit; they are in the full output ...]\n"));
    }
    out.truncate(out.trim_end().len());
    if out.len() > budget {
        // Markers and clipped lines pushed it over: fall back to the plain cut.
        return middle_cut(text, budget);
    }
    Summary { text: out, total_lines, cut: true }
}

/// `raw: true`: the first and last halves of the budget and nothing chosen between.
pub fn middle_cut(text: &str, budget: usize) -> Summary {
    let total_lines = if text.is_empty() { 0 } else { text.split('\n').count() };
    if text.len() <= budget {
        return Summary { text: text.to_string(), total_lines, cut: false };
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let half = budget.saturating_sub(120) / 2;
    let mut head = 0usize;
    let mut used = 0usize;
    while head < lines.len() && used + lines[head].len() + 1 <= half {
        used += lines[head].len() + 1;
        head += 1;
    }
    let mut tail = lines.len();
    let mut used = 0usize;
    while tail > head && used + lines[tail - 1].len() + 1 <= half {
        tail -= 1;
        used += lines[tail].len() + 1;
    }
    let mut out = lines[..head].join("\n");
    out.push_str(&format!("\n[... {} lines omitted ...]\n", tail - head));
    out.push_str(&lines[tail..].join("\n"));
    // A single enormous line leaves head == tail == 0 lines and the whole text still over budget.
    if out.len() > budget + 200 {
        let keep = budget / 2;
        let mut h = keep.min(text.len());
        while !text.is_char_boundary(h) {
            h -= 1;
        }
        let mut t = text.len().saturating_sub(keep);
        while !text.is_char_boundary(t) {
            t += 1;
        }
        out = format!("{}\n[... {} bytes omitted ...]\n{}", &text[..h], t - h, &text[t..]);
    }
    Summary { text: out, total_lines, cut: true }
}

/// The pointer appended to a cut result.
pub fn pointer(path: &std::path::Path, total_lines: usize) -> String {
    format!("[full output: {}, {} lines]", path.display().to_string().replace('\\', "/"), total_lines)
}

#[cfg(test)]
mod tests;
