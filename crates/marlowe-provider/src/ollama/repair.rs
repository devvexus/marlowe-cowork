//! Repair of the tool-call arguments small local models emit.
//!
//! Local calls fail in a handful of ways, all seen in the wild: arguments as a Python dict
//! (`{'a': True}`), a JSON string wrapped in prose or a code fence, a trailing comma, a raw
//! newline inside a string, an unescaped quote inside code, a value of the wrong type (`"20"`
//! for an integer), arguments nested one level too deep (`{"arguments": {...}}`), or a stream
//! cut off by the output limit. [`parse_lenient`] reads the first group, [`repair_args`] fixes
//! the shape against the tool's schema, and every fix is named ([`Fix`]) and counted
//! ([`RepairStats`]) so the bench can report the share of calls that needed one.
//!
//! One rule outranks the rest: **a call cut off mid-value is never completed.** A `write` whose
//! `content` stopped at `compute(` would otherwise run with half a file. Truncated arguments
//! stay a raw string and the loop tells the model the call was cut off. Only closers missing
//! after a complete value (`{"a": "b"` without `}`) are supplied.
//!
//! Inputs and fixtures: the case list in `tests/fixtures/repair_corpus.json` includes cases
//! adapted from FlashAgent (MIT, github.com/flashback7766/FlashAgent @ 0b08afc,
//! `crates/llm/src/repair.rs` and `parse.rs`); see THIRD_PARTY.md. The code is Marlowe's own.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use marlowe_model::ToolSpec;
use serde_json::{Map, Number, Value};

/// One named correction. The string form is what [`RepairStats`] counts under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Fix {
    /// Python single-quoted strings.
    SingleQuotes,
    /// `True`/`False`/`None`/`undefined`.
    PythonLiterals,
    TrailingComma,
    /// Members or elements separated by whitespace only.
    MissingComma,
    UnquotedKey,
    /// A bare word as a value (`"command": ls`).
    BareValue,
    /// A raw newline, tab or other control character inside a string.
    RawControl,
    /// An escape JSON does not have (`\d`, `\ `), kept literally.
    BadEscape,
    /// A `"` inside a string that is not an escape (code in `content`).
    InnerQuote,
    /// Python `"""text"""`.
    TripleQuote,
    /// Curly quotes used as delimiters.
    SmartQuotes,
    Comment,
    /// A markdown fence or `<tool_call>` tags around the JSON.
    Fence,
    /// Prose before or after the JSON.
    Prose,
    /// Closers missing after a complete value.
    Closed,
    /// A signed, leading-dot or otherwise non-JSON number.
    Number,
    /// The arguments were a JSON string inside a JSON string.
    Stringified,
    /// `{"arguments": {...}}` and the like.
    Unwrapped,
    /// A value of the wrong type converted where nothing is lost.
    Coerced,
    /// One unknown name taken for the one missing required name.
    Renamed,
    /// The tool name matched another spelling (`Read` for `read`).
    ToolName,
    /// The call was written as text, not as a structured call.
    FromText,
}

impl Fix {
    pub fn as_str(self) -> &'static str {
        match self {
            Fix::SingleQuotes => "single_quotes",
            Fix::PythonLiterals => "python_literals",
            Fix::TrailingComma => "trailing_comma",
            Fix::MissingComma => "missing_comma",
            Fix::UnquotedKey => "unquoted_key",
            Fix::BareValue => "bare_value",
            Fix::RawControl => "raw_control",
            Fix::BadEscape => "bad_escape",
            Fix::InnerQuote => "inner_quote",
            Fix::TripleQuote => "triple_quote",
            Fix::SmartQuotes => "smart_quotes",
            Fix::Comment => "comment",
            Fix::Fence => "fence",
            Fix::Prose => "prose",
            Fix::Closed => "closed",
            Fix::Number => "number",
            Fix::Stringified => "stringified",
            Fix::Unwrapped => "unwrapped",
            Fix::Coerced => "coerced",
            Fix::Renamed => "renamed",
            Fix::ToolName => "tool_name",
            Fix::FromText => "from_text",
        }
    }
}

/// What [`parse_lenient`] made of its input.
#[derive(Debug, Clone, PartialEq)]
pub enum Parsed {
    /// A complete value; `fixes` is empty when the input was already valid JSON.
    Ok { value: Value, fixes: Vec<Fix> },
    /// The input ended inside a value. `value` is what was read; it must not be run.
    Truncated { value: Value, fixes: Vec<Fix> },
    Failed,
}

impl Parsed {
    /// The value when the input was complete.
    pub fn complete(self) -> Option<(Value, Vec<Fix>)> {
        match self {
            Parsed::Ok { value, fixes } => Some((value, fixes)),
            _ => None,
        }
    }
}

/// Reads an object or array out of `input`, tolerating the damage listed in the module docs.
pub fn parse_lenient(input: &str) -> Parsed {
    let raw = input.trim().trim_start_matches('\u{feff}');
    if raw.is_empty() {
        return Parsed::Failed;
    }
    if let Ok(v) = serde_json::from_str::<Value>(raw) {
        return Parsed::Ok { value: v, fixes: Vec::new() };
    }
    let mut fixes = Vec::new();
    let body = strip_wrappers(raw, &mut fixes);
    let chars: Vec<char> = body.chars().collect();
    let Some(start) = chars.iter().position(|c| matches!(c, '{' | '[')) else {
        return Parsed::Failed;
    };
    if chars[..start].iter().any(|c| !c.is_whitespace()) {
        fixes.push(Fix::Prose);
    }
    let mut p = Reader { s: &chars, i: start, fixes, truncated: false, depth: 0 };
    let Some(value) = p.value() else {
        // Cut off before anything usable was read (`{`, `{"a"`): there is nothing to show.
        return if p.truncated { Parsed::Truncated { value: Value::Null, fixes: Vec::new() } } else { Parsed::Failed };
    };
    p.skip_ws();
    if p.i < chars.len() {
        p.fixes.push(Fix::Prose);
    }
    let mut fixes = p.fixes;
    dedup(&mut fixes);
    if p.truncated {
        Parsed::Truncated { value, fixes }
    } else {
        Parsed::Ok { value, fixes }
    }
}

/// `input` as compact valid JSON, or `None` when it is unusable or cut off.
pub fn repair_json(input: &str) -> Option<String> {
    parse_lenient(input).complete().map(|(v, _)| v.to_string())
}

fn dedup(fixes: &mut Vec<Fix>) {
    fixes.sort();
    fixes.dedup();
}

/// Removes a code fence and `<tool_call>` tags around the payload.
fn strip_wrappers<'a>(raw: &'a str, fixes: &mut Vec<Fix>) -> &'a str {
    let mut s = raw.trim();
    if let Some(rest) = s.strip_prefix("<tool_call>") {
        s = rest.trim();
        s = s.strip_suffix("</tool_call>").unwrap_or(s).trim();
        fixes.push(Fix::Fence);
    }
    if let Some(rest) = s.strip_prefix("```") {
        let after_lang = rest.find('\n').map_or("", |n| &rest[n + 1..]);
        s = after_lang.trim_end();
        s = s.strip_suffix("```").unwrap_or(s).trim();
        fixes.push(Fix::Fence);
    }
    s
}

struct Reader<'a> {
    s: &'a [char],
    i: usize,
    fixes: Vec<Fix>,
    truncated: bool,
    depth: u32,
}

const MAX_DEPTH: u32 = 64;

impl Reader<'_> {
    fn peek(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }

    fn at(&self, k: usize) -> Option<char> {
        self.s.get(self.i + k).copied()
    }

    fn skip_ws(&mut self) {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() => self.i += 1,
                Some('/') if self.at(1) == Some('/') => {
                    self.fixes.push(Fix::Comment);
                    while self.peek().is_some_and(|c| c != '\n') {
                        self.i += 1;
                    }
                }
                Some('/') if self.at(1) == Some('*') => {
                    self.fixes.push(Fix::Comment);
                    self.i += 2;
                    while self.i < self.s.len() && !(self.peek() == Some('*') && self.at(1) == Some('/')) {
                        self.i += 1;
                    }
                    self.i = (self.i + 2).min(self.s.len());
                }
                _ => break,
            }
        }
    }

    fn value(&mut self) -> Option<Value> {
        self.skip_ws();
        if self.depth > MAX_DEPTH {
            return None;
        }
        match self.peek() {
            None => {
                self.truncated = true;
                None
            }
            Some('{') => self.object(),
            Some('[') => self.array(),
            Some(c) if is_open_quote(c) => self.string().map(Value::String),
            Some(c) if c == '-' || c == '+' || c == '.' || c.is_ascii_digit() => self.number(),
            Some(c) if c.is_alphabetic() || c == '_' => self.word_value(),
            _ => None,
        }
    }

    fn object(&mut self) -> Option<Value> {
        self.depth += 1;
        self.i += 1;
        let mut map = Map::new();
        let out = loop {
            self.skip_ws();
            match self.peek() {
                None => {
                    // After a complete member closers are all that is missing; an object that
                    // never got a member is a stream cut at its first byte.
                    self.fixes.push(Fix::Closed);
                    if map.is_empty() {
                        self.truncated = true;
                    }
                    break Some(Value::Object(map));
                }
                Some('}') => {
                    self.i += 1;
                    break Some(Value::Object(map));
                }
                Some(',') => {
                    // `{,` and `,,` are noise; a comma before `}` was already counted below.
                    self.i += 1;
                    continue;
                }
                _ => {}
            }
            let key = match self.key() {
                Some(k) => k,
                None => break None,
            };
            self.skip_ws();
            match self.peek() {
                Some(':') => self.i += 1,
                Some('=') if self.at(1) == Some('>') => self.i += 2,
                Some('=') => self.i += 1,
                None => {
                    self.truncated = true;
                    break None;
                }
                _ => break None,
            }
            let Some(v) = self.value() else {
                if self.truncated {
                    // Keep what was read so the caller can show it; the value is incomplete.
                    break Some(Value::Object(map));
                }
                break None;
            };
            map.insert(key, v);
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.i += 1;
                    self.skip_ws();
                    if matches!(self.peek(), Some('}')) {
                        self.fixes.push(Fix::TrailingComma);
                    }
                }
                Some('}') => {}
                None => {
                    self.fixes.push(Fix::Closed);
                    // The last value was complete unless it was a number or word at the very end.
                    if self.ended_on_loose_token() {
                        self.truncated = true;
                    }
                    break Some(Value::Object(map));
                }
                Some(c) if is_open_quote(c) || c.is_alphabetic() || c == '_' => self.fixes.push(Fix::MissingComma),
                _ => break None,
            }
        };
        self.depth -= 1;
        out
    }

    fn array(&mut self) -> Option<Value> {
        self.depth += 1;
        self.i += 1;
        let mut items = Vec::new();
        let out = loop {
            self.skip_ws();
            match self.peek() {
                None => {
                    self.fixes.push(Fix::Closed);
                    break Some(Value::Array(items));
                }
                Some(']') => {
                    self.i += 1;
                    break Some(Value::Array(items));
                }
                Some(',') => {
                    self.i += 1;
                    continue;
                }
                _ => {}
            }
            let Some(v) = self.value() else {
                if self.truncated {
                    break Some(Value::Array(items));
                }
                break None;
            };
            items.push(v);
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.i += 1;
                    self.skip_ws();
                    if matches!(self.peek(), Some(']')) {
                        self.fixes.push(Fix::TrailingComma);
                    }
                }
                Some(']') => {}
                None => {
                    self.fixes.push(Fix::Closed);
                    if self.ended_on_loose_token() {
                        self.truncated = true;
                    }
                    break Some(Value::Array(items));
                }
                Some(c) if c == '{' || c == '[' || is_open_quote(c) || c.is_alphanumeric() => {
                    self.fixes.push(Fix::MissingComma)
                }
                _ => break None,
            }
        };
        self.depth -= 1;
        out
    }

    /// The text just read ends in a number or bare word, which a cut stream could have shortened.
    fn ended_on_loose_token(&self) -> bool {
        self.s.last().is_some_and(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-')
            && !self.s.ends_with(&['"'])
            && !self.s.ends_with(&['\''])
            && !self.s.ends_with(&['}'])
            && !self.s.ends_with(&[']'])
            && !ends_with_literal(self.s)
    }

    fn key(&mut self) -> Option<String> {
        match self.peek() {
            Some(c) if is_open_quote(c) => self.string(),
            Some(c) if c.is_alphabetic() || c == '_' || c == '$' => {
                let start = self.i;
                while self.peek().is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '$' | '.')) {
                    self.i += 1;
                }
                self.fixes.push(Fix::UnquotedKey);
                Some(self.s[start..self.i].iter().collect())
            }
            None => {
                self.truncated = true;
                None
            }
            _ => None,
        }
    }

    fn number(&mut self) -> Option<Value> {
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E' | '_')) {
            self.i += 1;
        }
        let tok: String = self.s[start..self.i].iter().filter(|c| **c != '_').collect();
        let clean = tok.trim_start_matches('+');
        let fixed = if clean.starts_with('.') {
            format!("0{clean}")
        } else if let Some(rest) = clean.strip_prefix("-.") {
            format!("-0.{rest}")
        } else {
            clean.to_string()
        };
        if fixed != tok {
            self.fixes.push(Fix::Number);
        }
        if self.i >= self.s.len() {
            // A number is the last thing in the text: the stream may have cut it short.
            self.truncated = true;
        }
        if let Ok(n) = serde_json::from_str::<Number>(&fixed) {
            return Some(Value::Number(n));
        }
        None
    }

    fn word_value(&mut self) -> Option<Value> {
        let start = self.i;
        while self.peek().is_some_and(|c| !c.is_whitespace() && !matches!(c, ',' | '}' | ']' | ':' | '"' | '\'')) {
            self.i += 1;
        }
        let word: String = self.s[start..self.i].iter().collect();
        let literal = match word.as_str() {
            "true" => Some(Value::Bool(true)),
            "false" => Some(Value::Bool(false)),
            "null" => Some(Value::Null),
            "True" => Some(Value::Bool(true)),
            "False" => Some(Value::Bool(false)),
            "None" | "undefined" | "nil" => Some(Value::Null),
            _ => None,
        };
        if let Some(v) = literal {
            if matches!(word.as_str(), "True" | "False" | "None" | "undefined" | "nil") {
                self.fixes.push(Fix::PythonLiterals);
            }
            return Some(v);
        }
        if self.i >= self.s.len() {
            // `tru` at the very end: a literal cut short.
            self.truncated = true;
        }
        // A bare path or word where a string belongs: `"command": ls`. Only simple tokens; a
        // sentence would be a guess.
        if !word.is_empty() && word.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '/' | '\\' | ':' | '-' | '*' | '~')) {
            self.fixes.push(Fix::BareValue);
            return Some(Value::String(word));
        }
        None
    }

    /// Reads a string starting at the opening delimiter at `self.i`.
    fn string(&mut self) -> Option<String> {
        let open = self.peek()?;
        // Python triple quotes.
        if (open == '"' || open == '\'') && self.at(1) == Some(open) && self.at(2) == Some(open) {
            return self.triple(open);
        }
        let smart = matches!(open, '\u{201c}' | '\u{2018}');
        if smart {
            self.fixes.push(Fix::SmartQuotes);
        } else if open == '\'' {
            self.fixes.push(Fix::SingleQuotes);
        }
        self.i += 1;
        let mut out = String::new();
        loop {
            let Some(c) = self.peek() else {
                self.truncated = true;
                return Some(out);
            };
            self.i += 1;
            match c {
                '\\' => self.escape(&mut out, open),
                c if self.closes(c, open) => {
                    if self.quote_ends_here() {
                        return Some(out);
                    }
                    self.fixes.push(Fix::InnerQuote);
                    out.push(c);
                }
                c if (c as u32) < 0x20 => {
                    self.fixes.push(Fix::RawControl);
                    out.push(c);
                }
                c => out.push(c),
            }
        }
    }

    fn closes(&self, c: char, open: char) -> bool {
        match open {
            '"' => c == '"',
            '\'' => c == '\'',
            '\u{201c}' => c == '\u{201d}' || c == '"',
            '\u{2018}' => c == '\u{2019}' || c == '\'',
            _ => false,
        }
    }

    /// Whether the quote just read (at `self.i - 1`) ends the string, as opposed to being a quote
    /// inside code. It ends it when what follows is structure: `, } ] :` or the end.
    fn quote_ends_here(&self) -> bool {
        let mut j = self.i;
        let mut newline = false;
        while let Some(&c) = self.s.get(j) {
            if c == '\n' {
                newline = true;
            }
            if !c.is_whitespace() {
                break;
            }
            j += 1;
        }
        match self.s.get(j) {
            None => true,
            Some(',') | Some('}') | Some(']') | Some(':') => true,
            // A new member on the next line with the comma forgotten.
            Some(&c) if newline && (is_open_quote(c) || c == '{') => true,
            // `"a" "b"` on one line would be a missing comma too, but inside code it is far more
            // often content (`print("a" "b")`); keep it as content.
            _ => false,
        }
    }

    fn escape(&mut self, out: &mut String, open: char) {
        let Some(c) = self.peek() else {
            self.truncated = true;
            return;
        };
        self.i += 1;
        match c {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            '/' => out.push('/'),
            '\\' => out.push('\\'),
            '"' => out.push('"'),
            '\'' => out.push('\''),
            'u' => {
                let hex: String = self.s[self.i..].iter().take(4).collect();
                match u32::from_str_radix(&hex, 16).ok().filter(|_| hex.chars().count() == 4) {
                    Some(hi) if (0xD800..0xDC00).contains(&hi) => {
                        self.i += 4;
                        // A pair: `\uD83D\uDE00`.
                        let lo = (self.at(0) == Some('\\') && self.at(1) == Some('u'))
                            .then(|| self.s[self.i + 2..].iter().take(4).collect::<String>())
                            .and_then(|h| u32::from_str_radix(&h, 16).ok())
                            .filter(|l| (0xDC00..0xE000).contains(l));
                        match lo {
                            Some(lo) => {
                                self.i += 6;
                                let cp = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                                out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                            }
                            None => out.push('\u{fffd}'),
                        }
                    }
                    Some(cp) => {
                        self.i += 4;
                        out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                    }
                    None => {
                        self.fixes.push(Fix::BadEscape);
                        out.push_str("\\u");
                    }
                }
            }
            // `\` then a real newline: a line continuation inside the string.
            '\n' => {
                self.fixes.push(Fix::RawControl);
            }
            other => {
                // Not a JSON escape (`\d`, `\U`, `\ `): the backslash was meant literally.
                let _ = open;
                self.fixes.push(Fix::BadEscape);
                out.push('\\');
                out.push(other);
            }
        }
    }

    fn triple(&mut self, q: char) -> Option<String> {
        self.fixes.push(Fix::TripleQuote);
        self.i += 3;
        let mut out = String::new();
        loop {
            let Some(c) = self.peek() else {
                self.truncated = true;
                return Some(out);
            };
            if c == q && self.at(1) == Some(q) && self.at(2) == Some(q) {
                self.i += 3;
                return Some(out);
            }
            self.i += 1;
            if c == '\\' {
                self.escape(&mut out, q);
            } else {
                out.push(c);
            }
        }
    }
}

fn is_open_quote(c: char) -> bool {
    matches!(c, '"' | '\'' | '\u{201c}' | '\u{2018}')
}

fn ends_with_literal(s: &[char]) -> bool {
    let tail: String = s.iter().rev().take(5).collect::<Vec<_>>().into_iter().rev().collect();
    ["true", "false", "null", "True", "False", "None"].iter().any(|l| tail.ends_with(l))
}

// ---- arguments against a schema -------------------------------------------------------------

/// The outcome of [`repair_args`].
#[derive(Debug, Clone, PartialEq)]
pub enum ArgsOutcome {
    /// Valid already; nothing changed.
    Clean(Value),
    Repaired(Value, Vec<Fix>),
    /// An object, but a required argument is still missing after repair: the call will fail in
    /// its tool. The value is passed on so the tool reports exactly what is missing.
    Invalid { value: Value, fixes: Vec<Fix>, missing: Vec<String> },
    /// Could not be made into an object. The raw text is kept (as a string) for the loop to
    /// report; `truncated` says the call was cut off rather than malformed.
    Unusable { raw: Value, truncated: bool },
}

impl ArgsOutcome {
    /// The value to put in the call.
    pub fn into_value(self) -> Value {
        match self {
            ArgsOutcome::Clean(v) | ArgsOutcome::Repaired(v, _) | ArgsOutcome::Invalid { value: v, .. } => v,
            ArgsOutcome::Unusable { raw, .. } => raw,
        }
    }
}

const WRAPPER_KEYS: [&str; 5] = ["arguments", "parameters", "args", "input", "params"];
const META_KEYS: [&str; 6] = ["name", "tool", "tool_name", "id", "type", "function"];

/// Makes `raw` (an object, or a string holding one) the arguments `tool` takes.
pub fn repair_args(raw: &Value, tool: Option<&ToolSpec>) -> ArgsOutcome {
    let mut fixes = Vec::new();
    let mut value = match raw {
        Value::String(s) => {
            if s.trim().is_empty() {
                return finish(Value::Object(Map::new()), fixes, tool);
            }
            match parse_lenient(s) {
                Parsed::Ok { value, fixes: f } => {
                    fixes.extend(f);
                    value
                }
                Parsed::Truncated { .. } => return ArgsOutcome::Unusable { raw: raw.clone(), truncated: true },
                Parsed::Failed => return ArgsOutcome::Unusable { raw: raw.clone(), truncated: false },
            }
        }
        Value::Object(_) | Value::Array(_) => raw.clone(),
        Value::Null => return finish(Value::Object(Map::new()), fixes, tool),
        _ => return ArgsOutcome::Unusable { raw: raw.clone(), truncated: false },
    };
    // JSON inside a JSON string: `"{\"a\": 1}"`.
    if let Value::String(inner) = &value {
        match parse_lenient(inner) {
            Parsed::Ok { value: v, fixes: f } if v.is_object() => {
                fixes.push(Fix::Stringified);
                fixes.extend(f);
                value = v;
            }
            _ => return ArgsOutcome::Unusable { raw: raw.clone(), truncated: false },
        }
    }
    // A one-element array holding the object.
    if let Value::Array(items) = &value {
        if items.len() == 1 && items[0].is_object() {
            fixes.push(Fix::Unwrapped);
            value = items[0].clone();
        }
    }
    if !value.is_object() {
        return ArgsOutcome::Unusable { raw: raw.clone(), truncated: false };
    }
    unwrap_wrappers(&mut value, tool, &mut fixes);
    if let Some(spec) = tool {
        let before = value.clone();
        coerce_to_schema(&mut value, &spec.input_schema);
        if value != before {
            fixes.push(Fix::Coerced);
            if renamed(&before, &value) {
                fixes.push(Fix::Renamed);
            }
        }
    }
    finish(value, fixes, tool)
}

/// Puts one structured call right against the request's tools and counts it: the tool's own
/// spelling of the name (`Read` is `read`), then its arguments against the schema. A name that
/// matches no tool of the request is counted as `unknown_tool`, not as a valid call. Returns the
/// name to use and the outcome.
pub fn repair_call(
    name: &str,
    raw: &Value,
    tools: Option<&[ToolSpec]>,
    stats: Option<&RepairStats>,
) -> (String, ArgsOutcome) {
    let found = tools.and_then(|t| match_tool(name, t));
    let (name, renamed, spec) = match found {
        Some((s, renamed)) => (s.name.clone(), renamed, Some(s)),
        None => (name.to_string(), false, None),
    };
    let mut outcome = repair_args(raw, spec);
    if renamed {
        outcome = match outcome {
            ArgsOutcome::Clean(v) => ArgsOutcome::Repaired(v, vec![Fix::ToolName]),
            ArgsOutcome::Repaired(v, mut f) => {
                f.push(Fix::ToolName);
                ArgsOutcome::Repaired(v, f)
            }
            ArgsOutcome::Invalid { value, mut fixes, missing } => {
                fixes.push(Fix::ToolName);
                ArgsOutcome::Invalid { value, fixes, missing }
            }
            u => u,
        };
    }
    if let Some(stats) = stats {
        if tools.is_some() && found.is_none() {
            stats.record_unknown_tool();
        } else {
            stats.record(&outcome, false);
        }
    }
    (name, outcome)
}

/// The names `tool` requires that `value` does not have.
fn missing_required(value: &Value, tool: Option<&ToolSpec>) -> Vec<String> {
    let (Some(tool), Some(obj)) = (tool, value.as_object()) else { return Vec::new() };
    tool.input_schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|k| !obj.contains_key(*k))
        .map(str::to_string)
        .collect()
}

fn finish(value: Value, mut fixes: Vec<Fix>, tool: Option<&ToolSpec>) -> ArgsOutcome {
    dedup(&mut fixes);
    let missing = missing_required(&value, tool);
    if !missing.is_empty() {
        ArgsOutcome::Invalid { value, fixes, missing }
    } else if fixes.is_empty() {
        ArgsOutcome::Clean(value)
    } else {
        ArgsOutcome::Repaired(value, fixes)
    }
}

fn renamed(before: &Value, after: &Value) -> bool {
    match (before.as_object(), after.as_object()) {
        (Some(b), Some(a)) => b.keys().any(|k| !a.contains_key(k)),
        _ => false,
    }
}

/// `{"arguments": {...}}`, `{"name": "read", "arguments": {...}}`, `{"read": {...}}`: unwrapped
/// only when the object holds nothing else and the tool has no argument of that name.
fn unwrap_wrappers(value: &mut Value, tool: Option<&ToolSpec>, fixes: &mut Vec<Fix>) {
    for _ in 0..3 {
        let Some(obj) = value.as_object() else { return };
        let props = tool.and_then(|t| t.input_schema.get("properties")).and_then(Value::as_object);
        let owns = |k: &str| props.is_some_and(|p| p.contains_key(k));
        let inner = if obj.len() == 1 {
            let (k, v) = obj.iter().next().expect("one entry");
            let named_tool = tool.is_some_and(|t| k.eq_ignore_ascii_case(&t.name)) && !owns(k);
            let wrapper = WRAPPER_KEYS.contains(&k.as_str()) && !owns(k);
            (named_tool || wrapper).then(|| v.clone())
        } else {
            let only_wrapper_and_meta =
                obj.keys().all(|k| (WRAPPER_KEYS.contains(&k.as_str()) || META_KEYS.contains(&k.as_str())) && !owns(k));
            if only_wrapper_and_meta {
                WRAPPER_KEYS.iter().find_map(|w| obj.get(*w)).cloned()
            } else {
                None
            }
        };
        let Some(inner) = inner else { return };
        let inner = match inner {
            Value::String(s) => match parse_lenient(&s).complete() {
                Some((v, f)) if v.is_object() => {
                    fixes.extend(f);
                    fixes.push(Fix::Stringified);
                    v
                }
                _ => return,
            },
            v => v,
        };
        if !inner.is_object() {
            return;
        }
        fixes.push(Fix::Unwrapped);
        *value = inner;
    }
}

/// Converts values to the type the schema asks for where nothing is lost: `"20"` for an integer,
/// `"true"` for a boolean, `42` for a string, an array or object sent as a JSON string, an
/// integral float for an integer. Recurses into properties and array items. Anything that does
/// not convert cleanly is left for the tool to report.
pub fn coerce_to_schema(value: &mut Value, schema: &Value) {
    let kind = schema
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or(if schema.get("properties").is_some() { "object" } else { "" });
    let json_in_string = |s: &str, open: char| {
        s.trim_start().starts_with(open).then(|| parse_lenient(s).complete().map(|(v, _)| v)).flatten()
    };
    match (kind, &*value) {
        ("integer", Value::String(s)) => {
            let t = s.trim();
            if let Ok(n) = t.parse::<i64>() {
                *value = Value::from(n);
            } else if let Some(f) = t.parse::<f64>().ok().filter(|f| f.fract() == 0.0 && f.abs() < 9.0e15) {
                *value = Value::from(f as i64);
            }
        }
        ("integer", Value::Number(n)) => {
            if let Some(f) = n.as_f64().filter(|f| n.is_f64() && f.fract() == 0.0 && f.abs() < 9.0e15) {
                *value = Value::from(f as i64);
            }
        }
        ("number", Value::String(s)) => {
            if let Some(n) = s.trim().parse::<f64>().ok().and_then(Number::from_f64) {
                *value = Value::Number(n);
            }
        }
        ("boolean", Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" => *value = Value::Bool(true),
            "false" | "no" => *value = Value::Bool(false),
            _ => {}
        },
        ("string", Value::Number(n)) => *value = Value::String(n.to_string()),
        ("string", Value::Bool(b)) => *value = Value::String(b.to_string()),
        ("array", Value::String(s)) => {
            if let Some(v @ Value::Array(_)) = json_in_string(s, '[') {
                *value = v;
                coerce_to_schema(value, schema);
            }
        }
        ("object", Value::String(s)) => {
            if let Some(v @ Value::Object(_)) = json_in_string(s, '{') {
                *value = v;
                coerce_to_schema(value, schema);
            }
        }
        ("array", Value::Array(_)) => {
            if let (Some(items), Value::Array(values)) = (schema.get("items"), value) {
                for v in values {
                    coerce_to_schema(v, items);
                }
            }
        }
        ("object", Value::Object(_)) => {
            if let (Some(props), Value::Object(map)) = (schema.get("properties").and_then(Value::as_object), value) {
                respell_names(map, props);
                rename_misnamed(map, props, schema.get("required"));
                for (name, sub) in props {
                    if let Some(v) = map.get_mut(name) {
                        coerce_to_schema(v, sub);
                    }
                }
            }
        }
        _ => {}
    }
}

/// `filePath` for `file_path`, `oldString` for `old_string`, `Limit` for `limit`: a name the schema
/// does not have that differs from exactly one of its names only in case and separators is that
/// name. A name the call already uses correctly is never overwritten.
fn respell_names(map: &mut Map<String, Value>, props: &Map<String, Value>) {
    let squash = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect::<String>();
    let unknown: Vec<String> = map.keys().filter(|k| !props.contains_key(*k)).cloned().collect();
    for key in unknown {
        let want = squash(&key);
        if want.is_empty() {
            continue;
        }
        let hits: Vec<&String> = props.keys().filter(|p| squash(p) == want).collect();
        if let [target] = hits.as_slice() {
            if !map.contains_key(*target) {
                if let Some(v) = map.remove(&key) {
                    map.insert((*target).clone(), v);
                }
            }
        }
    }
}

/// `{"status": 7}` where the schema's one missing required argument is `code`: with one name the
/// schema does not know and one required name absent, the model meant that one. The call would
/// fail on the missing argument anyway. The value must fit the type.
fn rename_misnamed(map: &mut Map<String, Value>, props: &Map<String, Value>, required: Option<&Value>) {
    let unknown: Vec<String> = map.keys().filter(|k| !props.contains_key(*k)).cloned().collect();
    let missing: Vec<String> = required
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|r| !map.contains_key(*r))
        .map(str::to_string)
        .collect();
    let ([key], [wanted]) = (unknown.as_slice(), missing.as_slice()) else { return };
    let Some(sub) = props.get(wanted) else { return };
    let mut v = map[key].clone();
    coerce_to_schema(&mut v, sub);
    let fits = match sub.get("type").and_then(Value::as_str) {
        Some("integer") => v.is_i64() || v.is_u64(),
        Some("number") => v.is_number(),
        Some("string") => v.is_string(),
        Some("boolean") => v.is_boolean(),
        Some("array") => v.is_array(),
        Some("object") => v.is_object(),
        _ => true,
    };
    if fits {
        map.remove(key);
        map.insert(wanted.clone(), v);
    }
}

// ---- tool names -----------------------------------------------------------------------------

/// The spec a model-written name refers to: exact, else the one tool whose name matches ignoring
/// case and separators, else after dropping a namespace (`functions.read`, `default_api:read`).
/// Never guesses between two.
pub fn match_tool<'a>(name: &str, tools: &'a [ToolSpec]) -> Option<(&'a ToolSpec, bool)> {
    let name = name.trim();
    if let Some(t) = tools.iter().find(|t| t.name == name) {
        return Some((t, false));
    }
    let squash = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect::<String>();
    let stripped = name
        .rsplit(['.', ':', '/'])
        .next()
        .map(|s| s.trim_start_matches("functions__").trim_start_matches("functions_"))
        .unwrap_or(name);
    for candidate in [name, stripped] {
        let want = squash(candidate);
        if want.is_empty() {
            continue;
        }
        let hits: Vec<&ToolSpec> = tools.iter().filter(|t| squash(&t.name) == want).collect();
        if let [one] = hits.as_slice() {
            return Some((one, true));
        }
    }
    None
}

// ---- counting -------------------------------------------------------------------------------

/// How many calls needed repair. Shared (`Arc`) between a provider and whatever reports it.
///
/// A4 asks for the share of calls "valid after repair" and the share "needing repair":
/// `(clean + repaired + from_text) / calls` and `(repaired + from_text) / calls`. A call that
/// still lacks a required argument (`invalid`) or names a tool the request never offered
/// (`unknown_tool`) counts as a call and is not valid.
#[derive(Default)]
pub struct RepairStats {
    calls: AtomicU64,
    clean: AtomicU64,
    repaired: AtomicU64,
    from_text: AtomicU64,
    unusable: AtomicU64,
    truncated: AtomicU64,
    invalid: AtomicU64,
    unknown_tool: AtomicU64,
    fixes: Mutex<BTreeMap<&'static str, u64>>,
}

/// A copy of the counters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepairSnapshot {
    pub calls: u64,
    pub clean: u64,
    pub repaired: u64,
    pub from_text: u64,
    pub unusable: u64,
    pub truncated: u64,
    /// Objects still missing a required argument after repair.
    pub invalid: u64,
    /// Calls to a name that matches no tool of the request.
    pub unknown_tool: u64,
    pub fixes: BTreeMap<String, u64>,
}

impl RepairSnapshot {
    /// Share of calls that ran with valid arguments after repair (1.0 with no calls).
    pub fn valid_rate(&self) -> f64 {
        if self.calls == 0 {
            return 1.0;
        }
        (self.clean + self.repaired + self.from_text) as f64 / self.calls as f64
    }

    /// Share of calls that needed any repair.
    pub fn repair_rate(&self) -> f64 {
        if self.calls == 0 {
            return 0.0;
        }
        (self.repaired + self.from_text) as f64 / self.calls as f64
    }
}

impl RepairStats {
    pub fn new() -> Self {
        Self::default()
    }

    /// Counts one call by its outcome.
    pub fn record(&self, outcome: &ArgsOutcome, from_text: bool) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let fixes: &[Fix] = match outcome {
            ArgsOutcome::Clean(_) => &[],
            ArgsOutcome::Repaired(_, f) => f,
            ArgsOutcome::Invalid { fixes, .. } => {
                self.invalid.fetch_add(1, Ordering::Relaxed);
                fixes
            }
            ArgsOutcome::Unusable { truncated, .. } => {
                self.unusable.fetch_add(1, Ordering::Relaxed);
                if *truncated {
                    self.truncated.fetch_add(1, Ordering::Relaxed);
                }
                &[]
            }
        };
        match outcome {
            ArgsOutcome::Unusable { .. } | ArgsOutcome::Invalid { .. } => {}
            _ if from_text => {
                self.from_text.fetch_add(1, Ordering::Relaxed);
            }
            ArgsOutcome::Repaired(..) => {
                self.repaired.fetch_add(1, Ordering::Relaxed);
            }
            ArgsOutcome::Clean(_) => {
                self.clean.fetch_add(1, Ordering::Relaxed);
            }
        }
        if !fixes.is_empty() || from_text {
            let mut map = self.fixes.lock().unwrap_or_else(|e| e.into_inner());
            for f in fixes {
                *map.entry(f.as_str()).or_default() += 1;
            }
            if from_text {
                *map.entry(Fix::FromText.as_str()).or_default() += 1;
            }
        }
    }

    /// Counts one call whose name matches no tool of the request. It is a call, and not valid.
    pub fn record_unknown_tool(&self) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.unknown_tool.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> RepairSnapshot {
        RepairSnapshot {
            calls: self.calls.load(Ordering::Relaxed),
            clean: self.clean.load(Ordering::Relaxed),
            repaired: self.repaired.load(Ordering::Relaxed),
            from_text: self.from_text.load(Ordering::Relaxed),
            unusable: self.unusable.load(Ordering::Relaxed),
            truncated: self.truncated.load(Ordering::Relaxed),
            invalid: self.invalid.load(Ordering::Relaxed),
            unknown_tool: self.unknown_tool.load(Ordering::Relaxed),
            fixes: self
                .fixes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .map(|(k, v)| (k.to_string(), *v))
                .collect(),
        }
    }

    pub fn reset(&self) {
        for a in [&self.calls, &self.clean, &self.repaired, &self.from_text, &self.unusable, &self.truncated, &self.invalid, &self.unknown_tool] {
            a.store(0, Ordering::Relaxed);
        }
        self.fixes.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(name: &str, schema: Value) -> ToolSpec {
        ToolSpec { name: name.into(), description: String::new(), input_schema: schema }
    }

    fn ok(s: &str) -> Value {
        match parse_lenient(s) {
            Parsed::Ok { value, .. } => value,
            other => panic!("{s:?} -> {other:?}"),
        }
    }

    fn fixes_of(s: &str) -> Vec<Fix> {
        match parse_lenient(s) {
            Parsed::Ok { fixes, .. } | Parsed::Truncated { fixes, .. } => fixes,
            Parsed::Failed => panic!("failed: {s:?}"),
        }
    }

    #[test]
    fn valid_json_is_untouched_and_costs_no_fix() {
        assert_eq!(parse_lenient(r#"{"a":1,"b":[true,null]}"#), Parsed::Ok { value: json!({"a":1,"b":[true,null]}), fixes: vec![] });
    }

    #[test]
    fn python_dicts_and_literals_become_json() {
        let v = ok("{'question': 'What language?', 'options': ['Python', 'Rust'], 'multi': False, 'x': None, 'y': True}");
        assert_eq!(v["question"], "What language?");
        assert_eq!(v["options"], json!(["Python", "Rust"]));
        assert_eq!((v["multi"].clone(), v["x"].clone(), v["y"].clone()), (json!(false), Value::Null, json!(true)));
        let f = fixes_of("{'a': True}");
        assert!(f.contains(&Fix::SingleQuotes) && f.contains(&Fix::PythonLiterals));
    }

    #[test]
    fn trailing_commas_missing_commas_comments_and_unquoted_keys() {
        assert_eq!(ok(r#"{"a": 1, "b": [1, 2,],}"#), json!({"a": 1, "b": [1, 2]}));
        assert_eq!(ok("{\"a\": 1\n \"b\": 2}"), json!({"a": 1, "b": 2}));
        assert_eq!(ok("{path: \"a.rs\", limit: 5}"), json!({"path": "a.rs", "limit": 5}));
        assert_eq!(ok("{\"a\": 1, // note\n \"b\": /* x */ 2}"), json!({"a": 1, "b": 2}));
        assert_eq!(ok(r#"{"command": ls}"#), json!({"command": "ls"}));
    }

    #[test]
    fn raw_newlines_tabs_and_odd_escapes_inside_strings_are_kept_as_text() {
        let v = ok("{\"content\": \"line one\nline two\tend\"}");
        assert_eq!(v["content"], "line one\nline two\tend");
        let v = ok(r#"{"pattern": "\d+\s*\.", "path": "C:\Users\me"}"#);
        assert_eq!(v["pattern"], "\\d+\\s*\\.");
        assert_eq!(v["path"], "C:\\Users\\me");
        assert_eq!(ok(r#"{"a": "\ud83d\ude00 \u00e9 \u12"}"#)["a"], "\u{1f600} \u{e9} \\u12");
    }

    #[test]
    fn quotes_inside_code_do_not_end_the_string() {
        let v = ok(r#"{"path": "a.py", "content": "print("hello")\nx = "y""}"#);
        assert_eq!(v["content"], "print(\"hello\")\nx = \"y\"");
        assert!(fixes_of(r#"{"content": "say "hi" now"}"#).contains(&Fix::InnerQuote));
        // Structure after the quote still closes it.
        assert_eq!(ok(r#"{"a": "x", "b": "y"}"#), json!({"a": "x", "b": "y"}));
        // A forgotten comma between lines is not an inner quote.
        assert_eq!(ok("{\"a\": \"x\"\n\"b\": \"y\"}"), json!({"a": "x", "b": "y"}));
    }

    #[test]
    fn triple_quotes_and_smart_quotes() {
        assert_eq!(ok("{'content': '''def f():\n    return 1\n'''}")["content"], "def f():\n    return 1\n");
        assert_eq!(ok("{\u{201c}cmd\u{201d}: \u{201c}ls -la\u{201d}}"), json!({"cmd": "ls -la"}));
        // Curly quotes inside a straight-quoted string are text, never delimiters.
        assert_eq!(ok("{\"note\": \"it\u{2019}s \u{201c}fine\u{201d}\", \"b\": 1, }")["note"], "it\u{2019}s \u{201c}fine\u{201d}");
    }

    #[test]
    fn fences_tags_and_prose_around_the_json_are_stripped() {
        assert_eq!(ok("```json\n{\"a\": 1}\n```"), json!({"a": 1}));
        assert_eq!(ok("<tool_call>{\"a\": 1,}</tool_call>"), json!({"a": 1}));
        assert_eq!(ok("Sure! Here is the call: {\"a\": {\"b\": 2}} Let me know!"), json!({"a": {"b": 2}}));
        assert!(fixes_of("Here: {\"a\": 1}").contains(&Fix::Prose));
    }

    #[test]
    fn closers_missing_after_a_complete_value_are_supplied() {
        assert_eq!(ok(r#"{"path": "a.rs", "n": [1, 2]"#), json!({"path": "a.rs", "n": [1, 2]}));
        assert_eq!(ok(r#"{"a": {"b": "c""#), json!({"a": {"b": "c"}}));
        assert_eq!(ok(r#"{"a": true"#), json!({"a": true}));
    }

    #[test]
    fn a_call_cut_off_mid_value_is_never_completed() {
        for cut in [
            r#"{"path": "src/main.rs", "content": "fn main() {\n let x = compute("#,
            r#"{"path": "/home/user/proj"#,
            r#"{"cmd": "cargo test", "flags": {"fast": tru"#,
            r#"{"limit": 4"#,
            r#"{"a": 1, "b""#,
            r#"{"a": "#,
            r#"{"a": 1, "b":"#,
            r#"{"a":"x\"#,
        ] {
            match parse_lenient(cut) {
                Parsed::Truncated { .. } => {}
                other => panic!("{cut:?} was {other:?}"),
            }
            assert_eq!(repair_json(cut), None, "{cut:?}");
        }
    }

    #[test]
    fn garbage_is_failed_not_invented() {
        for s in ["", "not json at all", "{{{", "[1, 2", "}{"] {
            let p = parse_lenient(s);
            assert!(!matches!(p, Parsed::Ok { .. }), "{s:?} -> {p:?}");
        }
        assert_eq!(repair_json("plain words"), None);
        // Valid JSON that is not an object cannot be a call's arguments.
        let t = Value::String("true".into());
        assert_eq!(repair_args(&t, None), ArgsOutcome::Unusable { raw: t.clone(), truncated: false });
    }

    #[test]
    fn deeply_nested_input_stops_instead_of_overflowing() {
        let deep = "[".repeat(5000) + &"]".repeat(5000);
        assert!(matches!(parse_lenient(&deep), Parsed::Failed | Parsed::Ok { .. }));
    }

    #[test]
    fn unicode_survives() {
        assert_eq!(ok("{'note': 'caf\u{e9} \u{2728} \u{4f60}\u{597d}'}")["note"], "caf\u{e9} \u{2728} \u{4f60}\u{597d}");
    }

    #[test]
    fn numbers_in_odd_forms() {
        assert_eq!(ok("{\"a\": +5, \"b\": .5, \"c\": -.25, \"d\": 1_000, \"e\": 2e3}"), json!({"a": 5, "b": 0.5, "c": -0.25, "d": 1000, "e": 2000.0}));
    }

    fn read_spec() -> ToolSpec {
        spec(
            "read",
            json!({"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"integer"},"limit":{"type":"integer"},"all":{"type":"boolean"}},"required":["path"]}),
        )
    }

    #[test]
    fn arguments_are_made_to_fit_the_schema() {
        let t = read_spec();
        let out = repair_args(&json!({"path": 42, "offset": " 20 ", "limit": 10.0, "all": "True"}), Some(&t));
        assert_eq!(out, ArgsOutcome::Repaired(json!({"path": "42", "offset": 20, "limit": 10, "all": true}), vec![Fix::Coerced]));
        // Not convertible: left for the tool to report.
        let odd = repair_args(&json!({"path": "a", "offset": "twenty", "all": "maybe"}), Some(&t));
        assert_eq!(odd, ArgsOutcome::Clean(json!({"path": "a", "offset": "twenty", "all": "maybe"})));
        assert_eq!(repair_args(&json!({"path": "a", "limit": 5}), Some(&t)), ArgsOutcome::Clean(json!({"path": "a", "limit": 5})));
    }

    #[test]
    fn nested_values_sent_as_json_strings_are_parsed() {
        let t = spec(
            "multi_edit",
            json!({"type":"object","properties":{"edits":{"type":"array","items":{"type":"object","properties":{"n":{"type":"integer"}}}}}}),
        );
        let out = repair_args(&json!({"edits": "[{\"n\": \"3\"}]"}), Some(&t));
        assert_eq!(out.into_value(), json!({"edits": [{"n": 3}]}));
    }

    #[test]
    fn one_misnamed_argument_takes_the_one_required_name_left_out() {
        let t = spec("exit", json!({"type":"object","properties":{"code":{"type":"integer"}},"required":["code"]}));
        let out = repair_args(&json!({"status": "7"}), Some(&t));
        assert_eq!(out, ArgsOutcome::Repaired(json!({"code": 7}), vec![Fix::Coerced, Fix::Renamed]));
        let t = read_spec();
        // Two unknown names, or a value of the wrong type: nothing is renamed.
        for odd in [json!({"file": "a.rs", "lines": 5}), json!({"path": "a.rs", "lines": 5, "extra": 1})] {
            assert_eq!(repair_args(&odd, Some(&t)).into_value(), odd);
        }
    }

    #[test]
    fn names_that_differ_only_in_case_and_separators_take_the_schemas_spelling() {
        let t = spec(
            "edit",
            json!({"type":"object","properties":{"file_path":{"type":"string"},"old_string":{"type":"string"},
                                                  "new_string":{"type":"string"},"replace_all":{"type":"boolean"}}}),
        );
        let out = repair_args(&json!({"filePath": "a.rs", "oldString": "x", "NewString": "y", "replaceAll": "true"}), Some(&t));
        assert_eq!(
            out,
            ArgsOutcome::Repaired(
                json!({"file_path": "a.rs", "old_string": "x", "new_string": "y", "replace_all": true}),
                vec![Fix::Coerced, Fix::Renamed]
            )
        );
        // A correct name is never overwritten by its respelling.
        let both = json!({"file_path": "a.rs", "filePath": "evil.rs"});
        assert_eq!(repair_args(&both, Some(&t)).into_value(), both);
    }

    #[test]
    fn wrappers_unwrap_only_when_nothing_else_is_there() {
        let t = read_spec();
        for wrapped in [
            json!({"arguments": {"path": "a"}}),
            json!({"name": "read", "arguments": {"path": "a"}}),
            json!({"read": {"path": "a"}}),
            json!({"parameters": "{\"path\": \"a\"}"}),
            json!([{"path": "a"}]),
        ] {
            assert_eq!(repair_args(&wrapped, Some(&t)).into_value(), json!({"path": "a"}), "{wrapped}");
        }
        // Top-level fields win; a stray wrapper next to them is data.
        let both = json!({"path": "a", "arguments": {"path": "b"}});
        assert_eq!(repair_args(&both, Some(&t)).into_value(), both);
        // A tool that owns an argument called `args` keeps it.
        let own = spec("run", json!({"type":"object","properties":{"args":{"type":"array"}}}));
        let a = json!({"args": ["-v"]});
        assert_eq!(repair_args(&a, Some(&own)).into_value(), a);
        let gateway = spec("call_tool", json!({"type":"object","properties":{"name":{"type":"string"},"arguments":{"type":"object"}}}));
        let g = json!({"name": "search", "arguments": {"q": "x"}});
        assert_eq!(repair_args(&g, Some(&gateway)).into_value(), g);
    }

    #[test]
    fn string_arguments_are_parsed_and_truncated_ones_kept_raw() {
        let t = read_spec();
        let out = repair_args(&Value::String("{'path': 'a.rs', 'limit': '5',}".into()), Some(&t));
        match out {
            ArgsOutcome::Repaired(v, f) => {
                assert_eq!(v, json!({"path": "a.rs", "limit": 5}));
                assert!(f.contains(&Fix::SingleQuotes) && f.contains(&Fix::TrailingComma) && f.contains(&Fix::Coerced));
            }
            other => panic!("{other:?}"),
        }
        let cut = Value::String(r#"{"path": "a.rs", "offs"#.into());
        assert_eq!(repair_args(&cut, Some(&t)), ArgsOutcome::Unusable { raw: cut.clone(), truncated: true });
        let junk = Value::String("hello".into());
        assert_eq!(repair_args(&junk, Some(&t)), ArgsOutcome::Unusable { raw: junk.clone(), truncated: false });
        assert_eq!(repair_args(&Value::String("  ".into()), None), ArgsOutcome::Clean(json!({})));
        let double = Value::String("\"{\\\"path\\\": \\\"a\\\"}\"".into());
        assert_eq!(repair_args(&double, Some(&t)).into_value(), json!({"path": "a"}));
    }

    #[test]
    fn tool_names_match_ignoring_case_and_namespace_but_never_guess() {
        let tools = vec![spec("read", json!({})), spec("web_fetch", json!({})), spec("web-fetch2", json!({}))];
        assert_eq!(match_tool("read", &tools).map(|(t, f)| (t.name.as_str(), f)), Some(("read", false)));
        assert_eq!(match_tool("Read", &tools).map(|(t, f)| (t.name.as_str(), f)), Some(("read", true)));
        assert_eq!(match_tool("functions.read", &tools).map(|(t, _)| t.name.as_str()), Some("read"));
        assert_eq!(match_tool("default_api:read", &tools).map(|(t, _)| t.name.as_str()), Some("read"));
        assert_eq!(match_tool("WebFetch", &tools).map(|(t, _)| t.name.as_str()), Some("web_fetch"));
        assert!(match_tool("write", &tools).is_none());
        let twins = vec![spec("a_b", json!({})), spec("ab", json!({}))];
        assert!(match_tool("AB", &twins).is_none(), "two candidates are never guessed between");
    }

    #[test]
    fn a_missing_required_argument_is_invalid_not_valid() {
        let read = spec("read", json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}));
        let out = repair_args(&json!({}), Some(&read));
        assert!(matches!(&out, ArgsOutcome::Invalid { missing, .. } if missing == &["path".to_string()]), "{out:?}");
        assert_eq!(out.clone().into_value(), json!({}), "the tool still reports what is missing");
        assert!(matches!(repair_args(&Value::Null, Some(&read)), ArgsOutcome::Invalid { .. }));
        assert!(matches!(repair_args(&json!(""), Some(&read)), ArgsOutcome::Invalid { .. }));
        assert!(matches!(repair_args(&json!({"path": "a"}), Some(&read)), ArgsOutcome::Clean(_)));
        // Unknown tool: nothing to check against.
        assert!(matches!(repair_args(&json!({"x": 1}), None), ArgsOutcome::Clean(_)));
        // A rename that supplies the missing name makes it valid again.
        let exit = spec("exit", json!({"type":"object","properties":{"code":{"type":"integer"}},"required":["code"]}));
        assert!(matches!(repair_args(&json!({"status": 7}), Some(&exit)), ArgsOutcome::Repaired(..)));

        let s = RepairStats::new();
        s.record(&repair_args(&json!({}), Some(&read)), false);
        s.record(&repair_args(&json!({"x": 1}), None), false);
        s.record_unknown_tool();
        s.record(&repair_args(&json!({"path": "a"}), Some(&read)), false);
        let snap = s.snapshot();
        assert_eq!((snap.calls, snap.invalid, snap.unknown_tool, snap.clean), (4, 1, 1, 2));
        assert!((snap.valid_rate() - 0.5).abs() < 1e-9, "{}", snap.valid_rate());
    }

    #[test]
    fn stats_count_by_outcome_and_report_the_two_rates() {
        let s = RepairStats::new();
        s.record(&ArgsOutcome::Clean(json!({})), false);
        s.record(&ArgsOutcome::Clean(json!({})), false);
        s.record(&ArgsOutcome::Repaired(json!({}), vec![Fix::TrailingComma]), false);
        s.record(&ArgsOutcome::Clean(json!({})), true);
        s.record(&ArgsOutcome::Unusable { raw: json!("x"), truncated: true }, false);
        let snap = s.snapshot();
        assert_eq!((snap.calls, snap.clean, snap.repaired, snap.from_text, snap.unusable, snap.truncated), (5, 2, 1, 1, 1, 1));
        assert!((snap.valid_rate() - 0.8).abs() < 1e-9);
        assert!((snap.repair_rate() - 0.4).abs() < 1e-9);
        assert_eq!(snap.fixes.get("trailing_comma"), Some(&1));
        assert_eq!(snap.fixes.get("from_text"), Some(&1));
        s.reset();
        assert_eq!(s.snapshot(), RepairSnapshot::default());
        assert_eq!(RepairSnapshot::default().valid_rate(), 1.0);
    }
}
