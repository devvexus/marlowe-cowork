//! Text helpers the research stages share: `[C12]` citation markers, sentence spans, quote
//! location that tolerates whitespace and typography but always returns the stored document's
//! own bytes, and passages cut around a quote. Pure; used by the crew's research v2 and by the
//! projects' Extract, Verify and Report.

use crate::{DocRef, Quote};

/// A `[C12]` or `[C12, C15]` citation marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    /// Byte span of the whole `[...]`.
    pub start: usize,
    pub end: usize,
    pub ids: Vec<u32>,
}

/// Claim-id citation markers in `text`, in order. `[C3][C4]` is two markers.
pub fn markers(text: &str) -> Vec<Marker> {
    let mut out = Vec::new();
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'[' {
            if let Some(close) = text[i..].find(']') {
                let inner = &text[i + 1..i + close];
                let parts: Vec<&str> = inner.split([',', ';']).map(str::trim).collect();
                let ids: Option<Vec<u32>> = parts
                    .iter()
                    .map(|p| {
                        let d = p.strip_prefix('C').or_else(|| p.strip_prefix('c'))?;
                        (!d.is_empty() && d.chars().all(|c| c.is_ascii_digit())).then(|| d.parse().ok()).flatten()
                    })
                    .collect();
                if let Some(ids) = ids.filter(|v| !v.is_empty()) {
                    out.push(Marker { start: i, end: i + close + 1, ids });
                    i += close + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

/// Words a sentence never ends after.
const ABBREVIATIONS: [&str; 14] =
    ["e.g", "i.e", "al", "fig", "figs", "eq", "vs", "cf", "approx", "no", "sec", "ref", "etc", "et"];

/// Sentence spans of one line of prose: a sentence ends at `.`, `!` or `?` followed by
/// whitespace, after any citation markers that follow the stop.
pub fn sentences(line: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0;
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    let mut k = 0;
    while k < chars.len() {
        let (i, c) = chars[k];
        if matches!(c, '.' | '!' | '?') {
            let word_start = line[..i].rfind(|ch: char| ch.is_whitespace() || ch == '(').map_or(start, |p| p + 1);
            let word = line[word_start..i].to_ascii_lowercase();
            let abbrev = c == '.'
                && (ABBREVIATIONS.contains(&word.as_str())
                    || (word.chars().count() == 1 && word.chars().all(char::is_alphabetic)));
            let mut end = i + c.len_utf8();
            // Citation markers right after the stop belong to this sentence.
            loop {
                let rest = &line[end..];
                let trimmed = rest.trim_start();
                if trimmed.starts_with("[C") || trimmed.starts_with("[c") {
                    if let Some(m) = markers(trimmed).first().filter(|m| m.start == 0) {
                        end += rest.len() - trimmed.len() + m.end;
                        continue;
                    }
                }
                break;
            }
            let next_ws = line[end..].chars().next().is_none_or(char::is_whitespace);
            if !abbrev && next_ws {
                if line[start..end].trim().len() > 1 {
                    out.push(trim_span(line, start, end));
                }
                start = end;
                while k < chars.len() && chars[k].0 < end {
                    k += 1;
                }
                continue;
            }
        }
        k += 1;
    }
    if line[start..].trim().len() > 1 {
        out.push(trim_span(line, start, line.len()));
    }
    out
}

fn trim_span(s: &str, start: usize, end: usize) -> (usize, usize) {
    let seg = &s[start..end];
    let lead = seg.len() - seg.trim_start().len();
    let trail = seg.len() - seg.trim_end().len();
    (start + lead, end - trail)
}

/// Typography folded for matching: curly quotes, dashes, non-breaking spaces.
fn fold(c: char) -> char {
    match c {
        '\u{2018}' | '\u{2019}' | '\u{201B}' | '\u{2032}' => '\'',
        '\u{201C}' | '\u{201D}' | '\u{201F}' | '\u{2033}' => '"',
        '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
        c if c.is_whitespace() => ' ',
        c => c,
    }
}

/// `text` with typography folded and whitespace runs collapsed, plus each output char's byte
/// span in `text`.
fn folded(text: &str) -> (Vec<char>, Vec<(usize, usize)>) {
    let mut chars: Vec<char> = Vec::with_capacity(text.len());
    let mut spans: Vec<(usize, usize)> = Vec::with_capacity(text.len());
    for (i, c) in text.char_indices() {
        let f = fold(c);
        if f == ' ' && chars.last() == Some(&' ') {
            if let Some(last) = spans.last_mut() {
                last.1 = i + c.len_utf8();
            }
            continue;
        }
        chars.push(f);
        spans.push((i, i + c.len_utf8()));
    }
    (chars, spans)
}

/// Finds `quote` in the stored document: verbatim first, then with whitespace and typography
/// folded, then the longest fragment of an elided (`...`) quote. The returned quote is always
/// the document's own bytes, so [`Quote::matches`] holds for it.
pub fn locate(doc: &DocRef, text: &str, quote: &str) -> Option<Quote> {
    let q = quote.trim().trim_matches(['"', '\'', '\u{201C}', '\u{201D}', '«', '»']).trim();
    if q.chars().count() < 8 {
        return None;
    }
    if let Some(found) = Quote::locate(doc, text, q) {
        return Some(found);
    }
    if let Some(found) = locate_folded(doc, text, q) {
        return Some(found);
    }
    let longest = q
        .split("...")
        .flat_map(|p| p.split('…'))
        .map(str::trim)
        .max_by_key(|p| p.chars().count())
        .filter(|p| p.chars().count() >= 30 && p.len() < q.len())?;
    Quote::locate(doc, text, longest).or_else(|| locate_folded(doc, text, longest))
}

fn locate_folded(doc: &DocRef, text: &str, q: &str) -> Option<Quote> {
    let (hay, spans) = folded(text);
    let (needle, _) = folded(q.trim());
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    let at = hay.windows(needle.len()).position(|w| w == needle.as_slice())?;
    let start = spans[at].0;
    let end = spans[at + needle.len() - 1].1;
    let exact = &text[start..end];
    let mut found = Quote::locate(doc, &text[start..], exact)?;
    found.locator.start += start;
    found.locator.end += start;
    Some(found)
}

/// At most `max` chars of `s`, cut at a char boundary, `…` when cut.
pub fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", s[..i].trim_end()),
        None => s.to_string(),
    }
}

/// The quote at `start..end` of `text` with about `around` bytes either side, on char
/// boundaries, the quote marked with « ».
pub fn passage(text: &str, start: usize, end: usize, around: usize) -> String {
    let mut a = start.saturating_sub(around);
    while a > 0 && !text.is_char_boundary(a) {
        a -= 1;
    }
    let mut b = (end + around).min(text.len());
    while b < text.len() && !text.is_char_boundary(b) {
        b += 1;
    }
    let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    format!(
        "{}{} «{}» {}{}",
        if a > 0 { "…" } else { "" },
        squash(&text[a..start]),
        squash(&text[start..end]),
        squash(&text[end..b]),
        if b < text.len() { "…" } else { "" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_and_sentences_keep_citations_with_their_sentence() {
        let t = "LoRA matches full tuning [C1, C2]. It uses less memory, e.g. 3x less. [C3] Done";
        let m = markers(t);
        assert_eq!(m.iter().map(|m| m.ids.clone()).collect::<Vec<_>>(), [vec![1, 2], vec![3]]);
        let s: Vec<&str> = sentences(t).into_iter().map(|(a, b)| &t[a..b]).collect();
        assert_eq!(s, ["LoRA matches full tuning [C1, C2].", "It uses less memory, e.g. 3x less. [C3]", "Done"]);
        assert!(markers("[1] and [see C3] and [C]").is_empty());
    }

    #[test]
    fn quotes_locate_through_typography_and_elision_but_stay_verbatim() {
        let doc = DocRef::of(b"d");
        let text = "Intro.\n\nWe find that rank\u{a0}8  is \u{201C}enough\u{201D} for GLUE \u{2014} mostly. Then more text follows here and keeps going for a while.";
        let q = locate(&doc, text, "rank 8 is \"enough\" for GLUE - mostly").unwrap();
        assert!(q.matches(text));
        assert!(q.text.starts_with("rank") && q.text.ends_with("mostly"));
        let q = locate(
            &doc,
            text,
            "\"We find that rank 8 is ... Then more text follows here and keeps going for a while.\"",
        )
        .unwrap();
        assert!(q.matches(text) && q.text.starts_with("Then more"));
        assert!(locate(&doc, text, "rank 16 is enough").is_none());
        let p = passage(text, q.locator.start, q.locator.end, 10);
        assert!(p.starts_with('…') && p.contains("«Then more text follows here and keeps going for a while.»"), "{p}");
    }
}
