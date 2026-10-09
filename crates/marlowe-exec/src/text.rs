//! Text helpers shared by the file tools: numbering, ranges, glob matching, bounded cuts.

/// Width of the right-aligned line-number column `read` prints (`cat -n` style, then a tab).
pub const LINE_NUMBER_WIDTH: usize = 6;

/// How many lines one `read` returns when no range is given.
pub const READ_WINDOW_LINES: usize = 2_000;

/// Byte ceiling on one `read` window, measured on the numbered text actually returned.
pub const READ_WINDOW_BYTES: usize = 32 * 1024;

/// How much of a file `read`/`grep` hold in memory.
pub const MAX_READ_BYTES: u64 = 4 * 1024 * 1024;

/// Consecutive numbered lines in `content` that make it a pasted `read` window.
pub const NUMBERED_CONTENT_LINES: usize = 2;

/// Sites a non-unique `replacing` names before saying "and N more".
pub const MAX_EDIT_SITES_NAMED: usize = 8;

/// Files one walk enumerates.
pub const WALK_FILE_CAP: usize = 2_000;

/// Directories a walk does not descend into unless named as the base.
pub const WALK_SKIP: [&str; 6] = [".git", "target", "node_modules", ".venv", "__pycache__", "dist"];

/// Result lines (matches plus context) `grep` emits before it only counts.
pub const MAX_MATCH_LINES: usize = 200;

/// Bytes kept of one emitted `grep` line.
pub const MAX_MATCH_LINE_BYTES: usize = 512;

/// Largest `grep` `context`; larger values are refused.
pub const MAX_GREP_CONTEXT: i64 = 20;

/// Regex compile ceiling (NFA and lazy DFA).
pub const REGEX_SIZE_LIMIT: usize = 1 << 20;

/// Paths a notice names before "and N more".
pub const NAMES_IN_A_NOTICE: usize = 10;

/// Tokens estimate for `bytes` of text (3 bytes per token, as the context assembler does).
pub fn estimate_tokens_of(bytes: usize) -> u32 {
    u32::try_from(bytes.div_ceil(3)).unwrap_or(u32::MAX)
}

pub fn floor_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

pub fn ceil_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// One emitted line, bounded, with the cut marked.
pub fn clip_line(line: &str) -> String {
    if line.len() <= MAX_MATCH_LINE_BYTES {
        return line.to_string();
    }
    let cut = floor_boundary(line, MAX_MATCH_LINE_BYTES);
    format!("{}…[line continues, {} more bytes]", &line[..cut], line.len() - cut)
}

pub fn count_lines(s: &str) -> usize {
    s.lines().count()
}

/// Head and tail of `text` with the omission stated, when it exceeds `max` bytes.
pub fn head_and_tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let head_end = floor_boundary(text, max / 2);
    let tail_start = ceil_boundary(text, text.len() - max / 2);
    format!(
        "{}\n…[{} bytes omitted from the middle]…\n{}",
        &text[..head_end],
        tail_start - head_end,
        &text[tail_start..]
    )
}

/// `{:>6}\t` in front of every line, counting from the absolute `first_line`. Terminators are
/// preserved so a CRLF file round-trips.
pub fn number_lines(text: &str, first_line: usize) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 8);
    for (i, line) in text.split_inclusive('\n').enumerate() {
        out.push_str(&format!("{:>width$}\t", first_line + i, width = LINE_NUMBER_WIDTH));
        out.push_str(line);
    }
    out
}

/// Inverse of [`number_lines`]: `Some` only if every line carries a well-formed prefix and the
/// numbers run consecutively.
pub fn strip_line_numbers(text: &str) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    let mut expected: Option<usize> = None;
    for line in text.split_inclusive('\n') {
        let spaces = line.len() - line.trim_start_matches(' ').len();
        let rest = &line[spaces..];
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits == 0 {
            return None;
        }
        let after = &rest[digits..];
        if !after.starts_with('\t') {
            return None;
        }
        let padded = spaces + digits == LINE_NUMBER_WIDTH;
        let widened = spaces == 0 && digits > LINE_NUMBER_WIDTH;
        if !padded && !widened {
            return None;
        }
        let n: usize = rest[..digits].parse().ok()?;
        if expected.is_some_and(|e| e != n) {
            return None;
        }
        expected = Some(n.checked_add(1)?);
        out.push_str(&after[1..]);
    }
    Some(out)
}

/// The 1-based line `offset` falls on.
pub fn line_at(text: &str, offset: usize) -> usize {
    text[..offset].matches('\n').count() + 1
}

pub fn take_lines(text: &str, n: usize) -> String {
    text.split_inclusive('\n').take(n).collect()
}

/// Cut to at most `cap` bytes on a line boundary; returns the text and the whole lines kept.
pub fn cut_to_whole_lines(text: &str, cap: usize) -> (String, usize) {
    let hard = floor_boundary(text, cap);
    let end = match text[..hard].rfind('\n') {
        Some(i) => i + 1,
        None => hard,
    };
    let end = if end == 0 { hard } else { end };
    (text[..end].to_string(), text[..end].lines().count())
}

/// `first-last`, 1-based inclusive. Every malformed range is an error, never "the whole file".
/// Returns the text and the first line's absolute number.
pub fn slice_lines(text: &str, range: &str) -> Result<(String, usize), String> {
    let malformed = |detail: &str| {
        Err(format!(
            "`range` must be `first-last`, 1-based and inclusive, e.g. \"20-60\" — {detail}. Omit \
             `range` to read the whole file."
        ))
    };
    let Some((a, b)) = range.split_once('-') else {
        return malformed(&format!("\"{range}\" has no `-`"));
    };
    let (Ok(a), Ok(b)) = (a.trim().parse::<usize>(), b.trim().parse::<usize>()) else {
        return malformed(&format!("\"{range}\" is not two whole numbers"));
    };
    if a == 0 {
        return malformed("lines are numbered from 1, not 0");
    }
    if b < a {
        return malformed(&format!("\"{range}\" runs backwards; did you mean \"{b}-{a}\"?"));
    }
    Ok((
        text.split_inclusive('\n')
            .skip(a - 1)
            .take((b - a).saturating_add(1))
            .collect::<String>(),
        a,
    ))
}

/// `*` and `?` only, case-insensitive. `*` does not cross `/`.
pub fn glob_match(pattern: &str, subject: &str) -> bool {
    fn go(p: &[char], s: &[char]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some('*') => go(&p[1..], s) || (!s.is_empty() && s[0] != '/' && go(p, &s[1..])),
            Some('?') => !s.is_empty() && go(&p[1..], &s[1..]),
            Some(c) => !s.is_empty() && s[0] == *c && go(&p[1..], &s[1..]),
        }
    }
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let s: Vec<char> = subject.to_lowercase().chars().collect();
    go(&p, &s)
}

/// Why `replacing` did not match, phrased so the model can act on it.
pub fn replacing_miss(existing: &str, replacing: &str) -> String {
    if let Some(stripped) = strip_line_numbers(replacing) {
        return match existing.find(&stripped) {
            Some(at) => format!(
                "`replacing` carries the line-number prefix `read` printed ({LINE_NUMBER_WIDTH} \
                 characters, then a tab, in front of every line). `edit` matches the file byte for \
                 byte, so strip them. Stripped, your text IS in the file, starting at line {}.",
                line_at(existing, at)
            ),
            None => format!(
                "`replacing` carries the line-number prefix `read` printed, which `edit` never \
                 accepts — strip it. Even stripped the text was not found, so `read` the file \
                 again and copy the snippet from what comes back. The file is {} bytes, {} lines.",
                existing.len(),
                count_lines(existing)
            ),
        };
    }
    if existing.contains("\r\n") && !replacing.contains('\r') && replacing.contains('\n') {
        let unix = existing.replace("\r\n", "\n");
        if let Some(at) = unix.find(replacing) {
            return format!(
                "`replacing` was not found because the file uses CRLF line endings and your text \
                 uses LF. Apart from that it is there, at line {}. `read` the file and copy the \
                 snippet from what comes back rather than retyping it.",
                line_at(&unix, at)
            );
        }
    }
    let squash = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
    if !replacing.trim().is_empty() && squash(existing).contains(&squash(replacing)) {
        return format!(
            "`replacing` was not found. The file DOES contain that text apart from whitespace, so \
             the indentation or line breaks differ — it must match byte for byte. `read` the file \
             and copy the snippet exactly as it comes back. The file is {} bytes, {} lines.",
            existing.len(),
            count_lines(existing)
        );
    }
    format!(
        "`replacing` was not found in the file, which is {} bytes and {} lines. It must match \
         byte for byte, including indentation. `read` the file first and copy the snippet from \
         what it returns.",
        existing.len(),
        count_lines(existing)
    )
}
