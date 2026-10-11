//! `read`'s line-number prefix (`cat -n` style: right-aligned number, tab) and its inverse, so the
//! edit tools can recognise a pasted `read` window.

/// Width of the right-aligned line-number column `read` prints, then a tab.
pub const LINE_NUMBER_WIDTH: usize = 6;

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

/// The 1-based line `offset` (a byte offset into `text`) falls on.
pub fn line_at(text: &str, offset: usize) -> usize {
    text[..offset.min(text.len())].matches('\n').count() + 1
}

/// Lines in `s` (a final line without a terminator counts).
pub fn count_lines(s: &str) -> usize {
    s.lines().count()
}
