//! Lenient readers for what the research roles reply, and the text helpers the stages share:
//! the first JSON value in a reply, `G3: upheld`-style verdict lines, `[C12]` citation markers,
//! sentence spans, and quote location that tolerates whitespace and typography but always
//! returns the stored document's own bytes.

use serde_json::Value;

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

pub use marlowe_research::text::{clip, locate, markers, passage, sentences};

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


}
