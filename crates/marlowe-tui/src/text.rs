//! Text helpers: sanitising, wrapping, number and time formatting.

use marlowe_model::{ModelInfo, ModelRef};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Daemon- or model-supplied text made safe to put in a cell: control characters and reserved
/// chrome glyphs are marked, tabs expanded.
pub fn clean(s: &str) -> String {
    let prepared = marlowe_surface::chrome::prepare_model_text(s);
    prepared.replace('\t', "    ")
}

/// [`clean`], with any maths in it typeset to one line. For model- or user-written titles, which
/// would otherwise put LaTeX source on screen. Not for commands or paths, where `$` is not maths.
pub fn clean_maths(s: &str) -> String {
    marlowe_surface::markdown::maths_one_line(&clean(s))
}

/// Display width of a string.
pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Truncates to `max` display columns, ending in `…` when cut.
pub fn truncate(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > max.saturating_sub(1) {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// Truncates to `max` display columns from the left, starting with `…` when cut: for paths,
/// whose tail is the part that names them.
pub fn truncate_left(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut tail: Vec<char> = Vec::new();
    let mut w = 0;
    for c in s.chars().rev() {
        let cw = c.width().unwrap_or(0);
        if w + cw > max.saturating_sub(1) {
            break;
        }
        tail.push(c);
        w += cw;
    }
    std::iter::once('…').chain(tail.into_iter().rev()).collect()
}

/// Pads with spaces to exactly `w` columns (truncating if longer).
pub fn pad(s: &str, w: usize) -> String {
    let t = truncate(s, w);
    let n = w.saturating_sub(width(&t));
    format!("{t}{}", " ".repeat(n))
}

/// Word-wraps one paragraph (no newlines) to `max` columns. Long words are broken.
fn wrap_paragraph(p: &str, max: usize, out: &mut Vec<String>) {
    let max = max.max(1);
    let mut line = String::new();
    let mut lw = 0;
    for word in p.split(' ') {
        let ww = width(word);
        let sep = usize::from(!line.is_empty());
        if lw + sep + ww <= max {
            if sep == 1 {
                line.push(' ');
            }
            line.push_str(word);
            lw += sep + ww;
            continue;
        }
        if !line.is_empty() {
            out.push(std::mem::take(&mut line));
            lw = 0;
        }
        if ww <= max {
            line.push_str(word);
            lw = ww;
            continue;
        }
        for c in word.chars() {
            let cw = c.width().unwrap_or(0);
            if lw + cw > max {
                out.push(std::mem::take(&mut line));
                lw = 0;
            }
            line.push(c);
            lw += cw;
        }
    }
    out.push(line);
}

/// Wraps text with newlines to `max` columns. A paragraph's leading indent is kept on every
/// line it wraps to.
pub fn wrap(s: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    for p in s.split('\n') {
        let p = p.trim_end_matches('\r');
        let body = p.trim_start_matches(' ');
        let indent = (p.len() - body.len()).min(max / 2);
        if indent == 0 {
            wrap_paragraph(p, max, &mut out);
            continue;
        }
        let start = out.len();
        wrap_paragraph(body, max - indent, &mut out);
        let pad = " ".repeat(indent);
        for l in &mut out[start..] {
            l.insert_str(0, &pad);
        }
    }
    out
}

/// `1234567` -> `1,234,567`.
pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `12400` -> `12.4k`, `1500000` -> `1.5M`.
pub fn compact(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => trim_point(n as f64 / 1_000.0, "k"),
        _ => trim_point(n as f64 / 1_000_000.0, "M"),
    }
}

fn trim_point(v: f64, unit: &str) -> String {
    if v >= 100.0 {
        format!("{v:.0}{unit}")
    } else {
        let s = format!("{v:.1}");
        format!("{}{unit}", s.trim_end_matches(".0"))
    }
}

/// A byte count: `812 B`, `212 KB`, `3.4 MB`.
pub fn bytes(n: u64) -> String {
    match n {
        0..=1_023 => format!("{n} B"),
        1_024..=1_048_575 => format!("{} KB", n / 1_024),
        _ => format!("{:.1} MB", n as f64 / 1_048_576.0),
    }
}

/// Download sizes as the offer and the memory line show them: decimal megabytes, rounded.
pub fn megabytes(n: u64) -> u64 {
    (n + 500_000) / 1_000_000
}

/// A price per million tokens: `$3`, `$0.15`, `free`.
pub fn price(v: f64) -> String {
    if v <= 0.0 {
        "free".into()
    } else if v >= 10.0 {
        format!("${v:.0}")
    } else if v >= 1.0 {
        let s = format!("{v:.2}");
        format!("${}", s.trim_end_matches('0').trim_end_matches('.'))
    } else {
        format!("${v:.2}")
    }
}

/// A dollar cost: `$0.0031`, `$1.24`.
pub fn cost(v: f64) -> String {
    if v < 0.1 {
        format!("${v:.4}")
    } else {
        format!("${v:.2}")
    }
}

/// How long something took: `12s`, `1m05s`.
pub fn took(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m{:02}s", s / 60, s % 60)
    }
}

/// How long ago `then_ms` was, relative to `now_ms`.
pub fn ago(then_ms: i64, now_ms: i64) -> String {
    let s = (now_ms - then_ms).max(0) / 1000;
    match s {
        0..=59 => "just now".into(),
        60..=3_599 => format!("{}m ago", s / 60),
        3_600..=86_399 => format!("{}h ago", s / 3_600),
        _ => format!("{}d ago", s / 86_400),
    }
}

/// A short human name for a model: the display name when it is short, else the last id segment.
pub fn short_model(r: &ModelRef, info: Option<&ModelInfo>) -> String {
    if let Some(i) = info {
        let d = i.display.trim();
        if !d.is_empty() && width(d) <= 22 {
            return d.to_string();
        }
    }
    let id = r.model_id.rsplit('/').next().unwrap_or(&r.model_id);
    truncate(id, 22)
}

/// A path for display: the home directory becomes `~`, separators become `/`.
pub fn short_path(p: &str) -> String {
    let mut s = p.replace('\\', "/");
    for var in ["USERPROFILE", "HOME"] {
        if let Ok(home) = std::env::var(var) {
            let h = home.replace('\\', "/");
            if !h.is_empty() && s.to_lowercase().starts_with(&h.to_lowercase()) {
                s = format!("~{}", &s[h.len()..]);
                break;
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_words_and_breaks_long_ones() {
        assert_eq!(wrap("one two three", 7), vec!["one two", "three"]);
        assert_eq!(wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(wrap("a\n\nb", 10), vec!["a", "", "b"]);
        assert_eq!(wrap("  ab cd", 5), vec!["  ab", "  cd"]);
    }

    #[test]
    fn numbers_format() {
        assert_eq!(thousands(1_204), "1,204");
        assert_eq!(thousands(12), "12");
        assert_eq!(compact(12_400), "12.4k");
        assert_eq!(compact(200_000), "200k");
        assert_eq!(compact(1_000), "1k");
        assert_eq!(price(3.0), "$3");
        assert_eq!(price(0.15), "$0.15");
        assert_eq!(price(0.0), "free");
        assert_eq!(cost(0.0031), "$0.0031");
    }

    #[test]
    fn clean_marks_escapes() {
        let s = clean("a\u{1b}[2Kb\tc");
        assert!(!s.contains('\u{1b}'));
        assert!(s.contains("<U+001B>"));
        assert!(s.contains("b    c"));
    }

    #[test]
    fn truncate_and_pad() {
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate_left("C:/work/valiance", 9), "…valiance");
        assert_eq!(truncate_left("short", 9), "short");
        assert_eq!(pad("ab", 4), "ab  ");
        assert_eq!(width(&pad("abcdef", 4)), 4);
    }
}
