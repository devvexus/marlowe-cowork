//! A **strict subset** of YAML, for `SKILL.md` front matter. ADR-051.
//!
//! # Why not a YAML crate
//!
//! CONTRACTS §7.1 pins the manifest as *"`SKILL.md` per the open Agent Skills standard —
//! **unmodified where the standard specifies it**"*, and the standard specifies YAML. That is an
//! argument FOR a real parser, and it was weighed rather than waved away.
//!
//! What decided it against: the shape §7.1 pins is eight keys of scalars and string lists, and a
//! general YAML parser brings anchors, aliases, merge keys, tags, multi-document streams and
//! implicit typing — a large surface reached by a file the user dropped into a directory, in
//! service of constructs the pinned shape never uses.
//!
//! # The failure this design refuses to have
//!
//! A subset parser that *guesses* at what it does not understand is worse than either option: it
//! would diverge from the standard silently, and a skill authored against a real YAML
//! implementation would load here meaning something else. That is this repository's standing
//! failure family — a mechanism that reports success while answering a different question.
//!
//! So **every construct outside the subset is a named load error**, never a best effort:
//! anchors, aliases, tags, block scalars, flow maps, nested sequences and multi-document markers
//! each refuse by name and cite the line. The subset is closed under refusal, which means a file
//! that loads here means what a YAML parser would say it means, or it does not load at all.
//!
//! # What IS in the subset
//!
//! * Block maps, nested by indentation (spaces only — a tab in the indent is an error, as it is
//!   in YAML itself).
//! * Block sequences (`- item`) and flow sequences of scalars (`[a, "b", 'c']`).
//! * Plain, single-quoted and double-quoted scalars. `\n`, `\t`, `\\`, `\"` and `\uXXXX` in
//!   double quotes; `''` in single quotes.
//! * `#` comments, outside quotes.
//! * Blank lines.
//!
//! Everything else refuses.

use std::collections::BTreeMap;
use std::fmt;

/// A parsed front-matter value. Deliberately three cases — the pinned shape has no others.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Scalar(String),
    Seq(Vec<String>),
    Map(BTreeMap<String, Value>),
}

impl Value {
    pub fn as_scalar(&self) -> Option<&str> {
        match self {
            Value::Scalar(s) => Some(s),
            _ => None,
        }
    }

    /// A sequence, **or a single scalar read as a one-element sequence**.
    ///
    /// YAML itself does not do this coercion and neither does this function by accident: it is
    /// only offered to callers that want it, and `trigger_phrases: "write a report"` is a
    /// spelling a skill author will reach for. Returning `None` there would refuse a file that a
    /// real YAML parser accepts, which is the divergence this module exists to avoid.
    pub fn as_seq(&self) -> Option<Vec<String>> {
        match self {
            Value::Seq(v) => Some(v.clone()),
            Value::Scalar(s) => Some(vec![s.clone()]),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&BTreeMap<String, Value>> {
        match self {
            Value::Map(m) => Some(m),
            _ => None,
        }
    }

    /// The name of this shape, for an error that has to say what it found.
    pub fn kind(&self) -> &'static str {
        match self {
            Value::Scalar(_) => "a scalar",
            Value::Seq(_) => "a list",
            Value::Map(_) => "a block",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub detail: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.detail)
    }
}

impl std::error::Error for ParseError {}

fn err<T>(line: usize, detail: impl Into<String>) -> Result<T, ParseError> {
    Err(ParseError { line, detail: detail.into() })
}

/// Split a `SKILL.md` into its front matter and its body.
///
/// The opening `---` must be the first line. **A file with no front matter is an error, not an
/// empty manifest**: a skill whose capability block silently defaulted would be a skill running
/// under a declaration nobody wrote.
pub fn split(source: &str) -> Result<(&str, &str), ParseError> {
    // A UTF-8 BOM ahead of `---` is common from Windows editors and is not the author's mistake.
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let rest = match source.strip_prefix("---\n").or_else(|| source.strip_prefix("---\r\n")) {
        Some(r) => r,
        None => {
            return err(
                1,
                "a SKILL.md must open with `---` on its first line, followed by YAML front \
                 matter. A file with no front matter has no capability declaration, and a \
                 defaulted declaration is one nobody wrote",
            )
        }
    };

    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed == "---" || trimmed == "..." {
            return Ok((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    err(
        rest.split('\n').count() + 1,
        "the front matter is never closed: expected a `---` line after the header block",
    )
}

/// Parse the subset. See the module header for exactly what that is.
pub fn parse(front: &str) -> Result<BTreeMap<String, Value>, ParseError> {
    let mut lines: Vec<(usize, usize, String)> = Vec::new(); // (line no, indent, content)
    for (i, raw) in front.split('\n').enumerate() {
        let no = i + 1;
        let raw = raw.trim_end_matches('\r');

        if raw.trim().is_empty() {
            continue;
        }
        let indent = raw.len() - raw.trim_start().len();
        if raw[..indent].contains('\t') {
            return err(no, "a tab in the indentation. YAML forbids it; use spaces");
        }
        let content = strip_comment(raw.trim_start());
        let content = content.trim_end();
        if content.is_empty() {
            continue; // a whole-line comment
        }
        if content == "---" || content == "..." {
            return err(no, "a second document marker inside the front matter");
        }
        lines.push((no, indent, content.to_string()));
    }

    let mut cursor = 0usize;
    let map = parse_map(&lines, &mut cursor, 0)?;
    if cursor != lines.len() {
        let (no, _, _) = &lines[cursor];
        return err(*no, "unexpected content after the end of the top-level block");
    }
    Ok(map)
}

/// Drop a `#` comment, respecting quotes.
///
/// **`#` only opens a comment when it follows whitespace or starts the line** — that is YAML's own
/// rule, and without it `signature: "ed25519:aa#bb"` would lose half its value.
fn strip_comment(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut quote: Option<u8> = None;
    let mut prev_space = true;
    for (i, &b) in bytes.iter().enumerate() {
        match quote {
            Some(q) => {
                if b == q {
                    quote = None;
                }
            }
            None => {
                if b == b'"' || b == b'\'' {
                    quote = Some(b);
                } else if b == b'#' && prev_space {
                    return &s[..i];
                }
            }
        }
        prev_space = b == b' ' || b == b'\t';
    }
    s
}

fn parse_map(
    lines: &[(usize, usize, String)],
    cursor: &mut usize,
    indent: usize,
) -> Result<BTreeMap<String, Value>, ParseError> {
    let mut out: BTreeMap<String, Value> = BTreeMap::new();

    while *cursor < lines.len() {
        let (no, ind, content) = &lines[*cursor];
        if *ind < indent {
            break;
        }
        if *ind > indent {
            return err(*no, "unexpected indentation: this line is deeper than its block");
        }
        if content.starts_with("- ") || content == "-" {
            return err(
                *no,
                "a list item where a `key: value` was expected. A top-level sequence is not part \
                 of the manifest shape",
            );
        }

        let (key, rest) = split_key(content, *no)?;
        if out.contains_key(&key) {
            return err(
                *no,
                format!(
                    "`{key}` is declared twice. YAML implementations disagree about which one \
                     wins, so a duplicate key is refused rather than resolved"
                ),
            );
        }
        *cursor += 1;

        if rest.is_empty() {
            // A nested block or a block sequence follows, at a deeper indent.
            let child_indent = match lines.get(*cursor) {
                Some((_, i, _)) if *i > indent => *i,
                _ => {
                    // `key:` with nothing under it is YAML's null. The pinned shape has no
                    // nullable field, so it becomes an empty block and the typed layer above
                    // decides whether that is acceptable for this key.
                    out.insert(key, Value::Map(BTreeMap::new()));
                    continue;
                }
            };
            let next_is_seq = lines[*cursor].2.starts_with("- ") || lines[*cursor].2 == "-";
            let value = if next_is_seq {
                Value::Seq(parse_block_seq(lines, cursor, child_indent)?)
            } else {
                Value::Map(parse_map(lines, cursor, child_indent)?)
            };
            out.insert(key, value);
        } else {
            out.insert(key, parse_inline(rest, *no)?);
        }
    }

    Ok(out)
}

fn parse_block_seq(
    lines: &[(usize, usize, String)],
    cursor: &mut usize,
    indent: usize,
) -> Result<Vec<String>, ParseError> {
    let mut out = Vec::new();
    while *cursor < lines.len() {
        let (no, ind, content) = &lines[*cursor];
        if *ind < indent {
            break;
        }
        if *ind > indent {
            return err(*no, "unexpected indentation inside a list");
        }
        let Some(item) = content.strip_prefix("- ") else {
            if content == "-" {
                return err(
                    *no,
                    "a list item with no value. A nested list or block under `-` is outside the \
                     manifest shape",
                );
            }
            break;
        };
        let item = item.trim();
        if item.contains(": ") || item.ends_with(':') {
            return err(
                *no,
                "a `key: value` inside a list. The manifest shape has no lists of blocks",
            );
        }
        out.push(scalar(item, *no)?);
        *cursor += 1;
    }
    Ok(out)
}

fn split_key(content: &str, no: usize) -> Result<(String, &str), ParseError> {
    // The key ends at the first `:` that is followed by a space or by end of line. A quoted key
    // is outside the subset and refuses below, so scanning for a bare `:` is sufficient here.
    let bytes = content.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b':' && (i + 1 == bytes.len() || bytes[i + 1] == b' ') {
            let key = content[..i].trim();
            if key.is_empty() {
                return err(no, "an empty key");
            }
            if key.starts_with('"') || key.starts_with('\'') {
                return err(no, "a quoted key. The manifest shape uses plain keys only");
            }
            if key.starts_with('&') || key.starts_with('*') {
                return err(no, "a YAML anchor or alias, which is outside the subset");
            }
            return Ok((key.to_string(), content[i + 1..].trim()));
        }
    }
    err(
        no,
        "expected `key: value`. A bare scalar at block level is not part of the manifest shape",
    )
}

fn parse_inline(rest: &str, no: usize) -> Result<Value, ParseError> {
    match rest.as_bytes()[0] {
        b'[' => Ok(Value::Seq(parse_flow_seq(rest, no)?)),
        b'{' => err(
            no,
            "a flow mapping `{...}`. Write the block form instead — the manifest shape uses \
             indented blocks",
        ),
        b'|' | b'>' => err(
            no,
            "a block scalar (`|` or `>`). Folding and chomping have several spellings that \
             differ in trailing whitespace, and a description whose bytes depend on which one \
             was meant is not a description this can pin",
        ),
        b'&' | b'*' => err(no, "a YAML anchor or alias, which is outside the subset"),
        b'!' => err(no, "a YAML tag, which is outside the subset"),
        _ => Ok(Value::Scalar(scalar(rest, no)?)),
    }
}

fn parse_flow_seq(rest: &str, no: usize) -> Result<Vec<String>, ParseError> {
    let inner = match rest.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        Some(i) => i,
        None => {
            return err(
                no,
                "a `[` list that does not close on the same line. Multi-line flow sequences are \
                 outside the subset; use the `- item` form",
            )
        }
    };
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    let mut item = String::new();
    let mut quote: Option<char> = None;
    for c in inner.chars() {
        match quote {
            Some(q) => {
                item.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => {
                    quote = Some(c);
                    item.push(c);
                }
                ',' => {
                    out.push(scalar(item.trim(), no)?);
                    item.clear();
                }
                '[' | ']' | '{' | '}' => {
                    return err(no, "a nested list or map inside a `[...]` list")
                }
                _ => item.push(c),
            },
        }
    }
    if quote.is_some() {
        return err(no, "an unterminated quoted string");
    }
    if !item.trim().is_empty() {
        out.push(scalar(item.trim(), no)?);
    }
    Ok(out)
}

/// One scalar: plain, `'single'` or `"double"`.
fn scalar(s: &str, no: usize) -> Result<String, ParseError> {
    if let Some(inner) = s.strip_prefix('"') {
        let Some(inner) = inner.strip_suffix('"') else {
            return err(no, "an unterminated double-quoted string");
        };
        return unescape(inner, no);
    }
    if let Some(inner) = s.strip_prefix('\'') {
        let Some(inner) = inner.strip_suffix('\'') else {
            return err(no, "an unterminated single-quoted string");
        };
        // In YAML the only escape inside single quotes is `''` for a literal quote.
        return Ok(inner.replace("''", "'"));
    }
    if s.starts_with('&') || s.starts_with('*') {
        return err(no, "a YAML anchor or alias, which is outside the subset");
    }
    Ok(s.to_string())
}

fn unescape(s: &str, no: usize) -> Result<String, ParseError> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some('0') => out.push('\0'),
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                if hex.len() != 4 {
                    return err(no, "a `\\u` escape with fewer than four hex digits");
                }
                let cp = u32::from_str_radix(&hex, 16)
                    .map_err(|_| ParseError { line: no, detail: format!("`\\u{hex}` is not hex") })?;
                match char::from_u32(cp) {
                    Some(c) => out.push(c),
                    None => return err(no, format!("`\\u{hex}` is not a character")),
                }
            }
            Some(other) => return err(no, format!("unknown escape `\\{other}`")),
            None => return err(no, "a trailing backslash"),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact block CONTRACTS §7.1 pins, comments and all.
    const PINNED: &str = "---\n\
name: pdf-report\n\
description: Generate a cited PDF report from a findings set.\n\
# standard fields above; Marlowe extension below\n\
x-marlowe:\n\
\x20 capability:\n\
\x20   paths:  [\"./out/**\"]\n\
\x20   hosts:  []\n\
\x20   creds:  []\n\
\x20 consequence: reversible          # REQUIRED. Absent => Irreversible. See 7.3.\n\
\x20 trigger_phrases: [\"write a report\", \"make a pdf\"]   # embedded; the prose is NOT\n\
\x20 signature: \"ed25519:...\"\n\
---\n\
Body text.\n";

    #[test]
    fn the_pinned_example_parses_to_the_pinned_shape() {
        let (front, body) = split(PINNED).expect("the pinned example splits");
        assert_eq!(body.trim(), "Body text.");
        let m = parse(front).expect("the pinned example parses");

        assert_eq!(m["name"].as_scalar(), Some("pdf-report"));
        assert_eq!(
            m["description"].as_scalar(),
            Some("Generate a cited PDF report from a findings set.")
        );

        let x = m["x-marlowe"].as_map().expect("x-marlowe is a block");
        assert_eq!(x["consequence"].as_scalar(), Some("reversible"));
        assert_eq!(
            x["trigger_phrases"].as_seq().unwrap(),
            vec!["write a report".to_string(), "make a pdf".to_string()]
        );
        // The `#` inside the quoted signature value must survive; only a `#` after whitespace
        // opens a comment.
        assert_eq!(x["signature"].as_scalar(), Some("ed25519:..."));

        let cap = x["capability"].as_map().expect("capability is a block");
        assert_eq!(cap["paths"].as_seq().unwrap(), vec!["./out/**".to_string()]);
        assert!(cap["hosts"].as_seq().unwrap().is_empty());
        assert!(cap["creds"].as_seq().unwrap().is_empty());
    }

    #[test]
    fn a_hash_inside_a_quoted_value_is_not_a_comment() {
        let m = parse("signature: \"ed25519:aa#bb\"\n").unwrap();
        assert_eq!(m["signature"].as_scalar(), Some("ed25519:aa#bb"));
    }

    #[test]
    fn a_block_sequence_is_read_as_a_list() {
        let m = parse("trigger_phrases:\n  - write a report\n  - \"make a pdf\"\n").unwrap();
        assert_eq!(
            m["trigger_phrases"].as_seq().unwrap(),
            vec!["write a report".to_string(), "make a pdf".to_string()]
        );
    }

    /// Every construct outside the subset refuses **by name**. A parser that guessed here would
    /// diverge from a real YAML implementation silently, which is the whole thing this design
    /// refuses to do.
    #[test]
    fn every_construct_outside_the_subset_refuses_and_says_which() {
        let cases: &[(&str, &str)] = &[
            ("anchor", "base: &a value\n"),
            ("alias", "copy: *a\n"),
            ("tag", "when: !!timestamp 2026-01-01\n"),
            ("block scalar", "description: |\n  two\n  lines\n"),
            ("flow map", "capability: {paths: []}\n"),
            ("nested flow", "paths: [[a, b]]\n"),
            ("duplicate key", "name: a\nname: b\n"),
            ("tab indent", "x-marlowe:\n\tconsequence: inert\n"),
            ("unterminated quote", "description: \"open\n"),
            ("bare scalar", "just-a-string\n"),
            ("list of blocks", "items:\n  - name: a\n"),
            ("second document", "name: a\n---\nname: b\n"),
        ];
        for (label, src) in cases {
            assert!(
                parse(src).is_err(),
                "{label} parsed instead of refusing: a construct outside the subset must be a \
                 named error, never a guess"
            );
        }
    }

    /// The control for the test above: the same parser accepts the shapes that ARE in the
    /// subset. Without it, `parse` returning `Err` unconditionally would pass every case.
    #[test]
    fn the_subset_itself_still_parses() {
        assert!(parse("name: a\n").is_ok());
        assert!(parse("a: 'single'\nb: \"double\"\nc: plain\n").is_ok());
        assert!(parse("l: [1, 2]\n").is_ok());
        assert!(parse("l:\n  - 1\n  - 2\n").is_ok());
        assert!(parse("m:\n  n:\n    o: p\n").is_ok());
        assert!(parse("").is_ok(), "empty front matter is a shape, not a parse error");
    }

    #[test]
    fn a_file_with_no_front_matter_is_an_error_rather_than_an_empty_manifest() {
        let e = split("# Just a markdown file\n").unwrap_err();
        assert!(e.detail.contains("must open with `---`"));
        assert!(split("---\nname: a\n").is_err(), "unclosed front matter must refuse");
    }

    #[test]
    fn escapes_in_a_double_quoted_scalar() {
        let m = parse("s: \"a\\nb\\u0041\\\"c\"\n").unwrap();
        assert_eq!(m["s"].as_scalar(), Some("a\nbA\"c"));
        assert!(parse("s: \"a\\qb\"\n").is_err(), "an unknown escape refuses");
    }
}
