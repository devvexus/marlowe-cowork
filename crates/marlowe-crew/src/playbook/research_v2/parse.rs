//! Lenient readers for what the research roles reply, and the text helpers the stages share:
//! the first JSON value in a reply, `G3: upheld`-style verdict lines, `[C12]` citation markers,
//! sentence spans, and quote location that tolerates whitespace and typography but always
//! returns the stored document's own bytes.

use serde_json::Value;

use marlowe_research::{DocRef, Quote};

/// The first JSON object or array in `text` (code fences and prose around it ignored).
pub fn first_json(text: &str) -> Option<Value> {
    let mut tried = 0;
    for (i, c) in text.char_indices() {
        if c != '{' && c != '[' {
            continue;
        }
        tried += 1;
        if tried > 40 {
            break;
        }
        let mut it = serde_json::Deserializer::from_str(&text[i..]).into_iter::<Value>();
        if let Some(Ok(v)) = it.next() {
            if v.is_object() || v.is_array() {
                return Some(v);
            }
        }
    }
    None
}

/// The first JSON array in `text`, or the first array field of the first object.
pub fn json_items(text: &str) -> Option<Vec<Value>> {
    match first_json(text)? {
        Value::Array(a) => Some(a),
        Value::Object(o) => o.into_iter().find_map(|(_, v)| match v {
            Value::Array(a) => Some(a),
            _ => None,
        }),
        _ => None,
    }
}

/// The first of `names` that is a non-empty string field of `v`, trimmed.
pub fn str_field(v: &Value, names: &[&str]) -> Option<String> {
    names.iter().find_map(|n| v.get(*n).and_then(Value::as_str)).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Unsigned integers in `s`, in order (`C12`, `#3`, `3, 7` all count).
pub fn numbers(s: &str) -> Vec<u32> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in s.chars().chain(std::iter::once(' ')) {
        if c.is_ascii_digit() {
            cur.push(c);
        } else if !cur.is_empty() {
            if let Ok(n) = cur.parse() {
                out.push(n);
            }
            cur.clear();
        }
    }
    out
}

/// A JSON value as a list of ids: `[3, "C7", 9]` or `"3, 7"`.
pub fn id_list(v: Option<&Value>) -> Vec<u32> {
    match v {
        Some(Value::Array(a)) => a
            .iter()
            .flat_map(|x| match x {
                Value::Number(n) => n.as_u64().map(|n| vec![n as u32]).unwrap_or_default(),
                Value::String(s) => numbers(s),
                _ => Vec::new(),
            })
            .collect(),
        Some(Value::String(s)) => numbers(s),
        Some(Value::Number(n)) => n.as_u64().map(|n| vec![n as u32]).unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// One verdict line: `G3: upheld`, `C12. partial: the table says 71.2`, `**4)** refuted — why`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerdictLine {
    pub id: u32,
    /// The verdict word, lower-case.
    pub word: String,
    pub reason: Option<String>,
}

/// Verdict lines whose id carries `prefix` (case-insensitive; a bare number also counts).
/// The first verdict for an id wins.
pub fn verdict_lines(text: &str, prefix: char) -> Vec<VerdictLine> {
    let mut out: Vec<VerdictLine> = Vec::new();
    for line in text.lines() {
        let t = line.trim().trim_start_matches(['-', '*', '#', '>', ' ', '`']);
        let t = t.strip_prefix(prefix).or_else(|| t.strip_prefix(prefix.to_ascii_lowercase())).unwrap_or(t);
        let digits = t.chars().take_while(char::is_ascii_digit).count();
        let Ok(id) = t[..digits].parse::<u32>() else { continue };
        let rest = t[digits..].trim_start_matches(['.', ')', ':', '*', ' ', '-', '—', '–', '`', ']']);
        let word: String =
            rest.chars().take_while(|c| c.is_ascii_alphabetic()).collect::<String>().to_ascii_lowercase();
        if word.is_empty() {
            continue;
        }
        let after = rest[word.len()..].trim_start_matches(['*', '`', ' ']);
        let reason = after
            .trim_start_matches([':', '—', '–', '-', '(', ' ', ','])
            .trim()
            .trim_end_matches(')')
            .trim()
            .to_string();
        if !out.iter().any(|v| v.id == id) {
            out.push(VerdictLine { id, word, reason: (!reason.is_empty()).then_some(reason) });
        }
    }
    out
}

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
    fn json_is_found_inside_prose_and_fences() {
        let v = json_items("Here you go:\n```json\n[{\"claim\": \"a\"}, {\"claim\": \"b\"}]\n```\nDone.").unwrap();
        assert_eq!(v.len(), 2);
        let v = json_items("{\"claims\": [{\"claim\": \"a\"}]}").unwrap();
        assert_eq!(str_field(&v[0], &["text", "claim"]).as_deref(), Some("a"));
        assert!(json_items("no json [here").is_none());
        assert_eq!(id_list(Some(&serde_json::json!([3, "C7", "9, 10"]))), [3, 7, 9, 10]);
    }

    #[test]
    fn verdict_lines_read_leniently() {
        let v = verdict_lines(
            "- **G3:** upheld\nG4. refuted: Table 2 says 71.2\n5) weakened (one dataset)\nG3: refuted\nnoise",
            'G',
        );
        assert_eq!(v.len(), 3);
        assert_eq!((v[0].id, v[0].word.as_str(), v[0].reason.as_deref()), (3, "upheld", None));
        assert_eq!((v[1].id, v[1].word.as_str(), v[1].reason.as_deref()), (4, "refuted", Some("Table 2 says 71.2")));
        assert_eq!((v[2].id, v[2].reason.as_deref()), (5, Some("one dataset")));
        let c = verdict_lines("C12: partial — only for 7B", 'C');
        assert_eq!((c[0].id, c[0].word.as_str(), c[0].reason.as_deref()), (12, "partial", Some("only for 7B")));
    }

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
