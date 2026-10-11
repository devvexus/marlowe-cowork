//! Tool calls a model wrote as text instead of as a structured call, and the guard that keeps
//! that text off the screen while it streams.
//!
//! Models without reliable native calling (and every model on a bad turn) write the call into
//! the answer: Hermes `<tool_call>{json}</tool_call>`, Qwen `<function=read><parameter=...>`,
//! GLM `<tool_call>read<arg_key>..`, Mistral `[TOOL_CALLS][..]`, a ```` ```tool_call ```` fence, or a bare JSON
//! object on its own line. [`scan`] finds them in finished text; [`LeakGuard`] withholds the
//! stretch of a stream that may be one.
//!
//! What makes this safe to run:
//! - **Known names only.** With a tool list, a block whose name matches no tool is prose. A model
//!   quoting `{"name": "Alice"}` runs nothing.
//! - **Fences are display.** Text inside a ```` ``` ```` fence is never a call, so an example of
//!   a call in an explanation is not executed. A ```` ```tool_call ```` fence is the exception (the
//!   tag says it is meant to run), and an untagged ```` ```json ```` fence runs only when it is the
//!   last thing in the reply and does not follow a colon (`Example:`).
//! - **Cut-off blocks are prose.** A block that ends mid-value is left as text; repair never
//!   finishes a half-written file.
//! - **Arguments are repaired against the schema** like native calls, and the call is marked
//!   [`Fix::FromText`] so it is counted.
//!
//! Fixtures adapted from FlashAgent (MIT, @ 0b08afc, `crates/llm/src/parse.rs` `TextToolScanner`
//! tests) are in `tests/fixtures/repair_corpus.json`; the scanner is Marlowe's own.

use std::ops::Range;

use marlowe_model::ToolSpec;
use serde_json::{Map, Value};

use super::leak::{glm_call, xml_calls, XmlCall};
use super::repair::{match_tool, parse_lenient, repair_args, ArgsOutcome, Fix, Parsed};

/// Which names a call may carry.
#[derive(Clone, Copy)]
pub enum ToolSet<'a> {
    /// The caller does not know the tools (legacy path): explicit markup forms are accepted under
    /// any name, bare JSON is never accepted.
    Any,
    Known(&'a [ToolSpec]),
}

/// How a call was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// `<function=NAME><parameter=KEY>`.
    Qwen,
    /// `<tool_call>{json}</tool_call>`.
    Hermes,
    /// `<tool_call>NAME<arg_key>`.
    Glm,
    /// `[TOOL_CALLS][{json}]`.
    Mistral,
    /// ```` ```tool_call ```` or a final ```` ```json ```` fence.
    Fence,
    /// A JSON object or array on a line of its own.
    Bare,
}

/// One call found in text.
#[derive(Debug, Clone, PartialEq)]
pub struct ScannedCall {
    pub name: String,
    pub args: Value,
    pub format: Format,
    pub fixes: Vec<Fix>,
    pub span: Range<usize>,
}

/// What [`scan`] found.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Scan {
    pub calls: Vec<ScannedCall>,
    /// The text with every accepted call cut out.
    pub prose: String,
    /// Blocks that looked like calls and were left as text (unknown name, cut off, unreadable).
    pub rejected: usize,
}

const HERMES_OPEN: &str = "<tool_call>";
const HERMES_CLOSE: &str = "</tool_call>";
const MISTRAL: &str = "[TOOL_CALLS]";

/// Finds the tool calls written in `text`.
pub fn scan(text: &str, tools: ToolSet<'_>) -> Scan {
    let fences = fences(text);
    let mut calls: Vec<ScannedCall> = Vec::new();
    let mut rejected = 0;
    let mut pos = 0;
    while pos < text.len() {
        let Some((start, kind)) = next_candidate(text, pos, &fences, tools) else { break };
        match read_candidate(text, start, kind, &fences, tools) {
            Some((found, end)) => {
                calls.extend(found);
                pos = end.max(start + 1);
            }
            None => {
                rejected += 1;
                pos = start + kind.skip();
            }
        }
    }
    // Tagged fences are candidates of their own; they never overlap the above (not inside a
    // generic fence, and the other kinds skip them).
    let prose = prose_without(text, &calls);
    Scan { calls, prose, rejected }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Hermes,
    Qwen,
    Mistral,
    CallFence(usize),
    JsonFence(usize),
    Bare,
}

impl Kind {
    fn skip(self) -> usize {
        match self {
            Kind::Hermes => HERMES_OPEN.len(),
            Kind::Qwen => "<function=".len(),
            Kind::Mistral => MISTRAL.len(),
            Kind::CallFence(_) | Kind::JsonFence(_) => 3,
            Kind::Bare => 1,
        }
    }
}

// ---- fences ----------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Fence {
    /// From the opening backticks through the end of the closing line (or the end of the text).
    span: Range<usize>,
    /// The body between the lines.
    body: Range<usize>,
    info: String,
}

impl Fence {
    fn is_call_tag(&self) -> bool {
        matches!(self.info.as_str(), "tool_call" | "tool_code" | "tool")
    }
}

fn fences(text: &str) -> Vec<Fence> {
    let mut out = Vec::new();
    let mut open: Option<(usize, usize, String)> = None; // span start, body start, info
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let trimmed = line.trim_start_matches([' ', '\t']);
        if !trimmed.starts_with("```") {
            continue;
        }
        match open.take() {
            None => {
                let info = trimmed.trim_start_matches('`').trim().to_ascii_lowercase();
                open = Some((start, offset, info));
            }
            Some((s, body_start, info)) => {
                out.push(Fence { span: s..offset, body: body_start..start, info });
            }
        }
    }
    if let Some((s, body_start, info)) = open {
        out.push(Fence { span: s..text.len(), body: body_start..text.len(), info });
    }
    out
}

fn inside_generic_fence(pos: usize, fences: &[Fence]) -> bool {
    fences.iter().any(|f| !f.is_call_tag() && f.span.contains(&pos))
}

// ---- finding candidates ----------------------------------------------------------------------

fn next_candidate(text: &str, from: usize, fences: &[Fence], tools: ToolSet<'_>) -> Option<(usize, Kind)> {
    let mut best: Option<(usize, Kind)> = None;
    let mut consider = |p: usize, k: Kind| {
        if best.is_none_or(|(b, _)| p < b) {
            best = Some((p, k));
        }
    };
    for (marker, kind) in [(HERMES_OPEN, Kind::Hermes), ("<function=", Kind::Qwen), (MISTRAL, Kind::Mistral)] {
        let mut at = from;
        while let Some(rel) = text[at..].find(marker) {
            let p = at + rel;
            if !inside_generic_fence(p, fences) && !inside_call_fence(p, fences) {
                consider(p, kind);
                break;
            }
            at = p + marker.len();
        }
    }
    for (i, f) in fences.iter().enumerate() {
        if f.span.start < from {
            continue;
        }
        if f.is_call_tag() {
            consider(f.span.start, Kind::CallFence(i));
        } else if matches!(f.info.as_str(), "json" | "") && matches!(tools, ToolSet::Known(_)) {
            consider(f.span.start, Kind::JsonFence(i));
        }
    }
    if matches!(tools, ToolSet::Known(_)) {
        let mut line_start = from;
        // Back up to the start of the line `from` is in, unless it begins exactly at one.
        if from > 0 && text.as_bytes()[from - 1] != b'\n' {
            match text[from..].find('\n') {
                Some(n) => line_start = from + n + 1,
                None => return best,
            }
        }
        while line_start < text.len() {
            let rest = &text[line_start..];
            let indent = rest.len() - rest.trim_start_matches([' ', '\t']).len();
            let at = line_start + indent;
            if matches!(text.as_bytes().get(at), Some(b'{') | Some(b'['))
                && !inside_generic_fence(at, fences)
                && !inside_call_fence(at, fences)
            {
                consider(at, Kind::Bare);
                break;
            }
            match rest.find('\n') {
                Some(n) => line_start += n + 1,
                None => break,
            }
        }
    }
    best
}

fn inside_call_fence(pos: usize, fences: &[Fence]) -> bool {
    fences.iter().any(|f| f.is_call_tag() && f.span.contains(&pos))
}

fn read_candidate(
    text: &str,
    start: usize,
    kind: Kind,
    fences: &[Fence],
    tools: ToolSet<'_>,
) -> Option<(Vec<ScannedCall>, usize)> {
    match kind {
        Kind::Hermes => read_hermes(text, start, tools),
        Kind::Qwen => {
            let xml: Vec<XmlCall> = xml_calls(&text[start..]).into_iter().take(1).collect();
            let x = xml.first()?;
            let end = start + x.span.end;
            let call = xml_to_call(x, Format::Qwen, start + x.span.start..end, tools)?;
            Some((vec![call], end))
        }
        Kind::Mistral => read_mistral(text, start, tools),
        Kind::CallFence(i) => {
            let f = fences.get(i)?;
            let body = &text[f.body.clone()];
            // An unclosed tagged fence still counts when its JSON is complete.
            let (calls, fixes) = calls_from_json(body, tools)?;
            let found = finish_calls(calls, fixes, Format::Fence, f.span.clone(), tools)?;
            Some((found, f.span.end))
        }
        Kind::JsonFence(i) => {
            let f = fences.get(i)?;
            // Runs only as the last thing in the reply, and not after "Example:" and the like.
            if !text[f.span.end..].trim().is_empty() {
                return None;
            }
            let before = text[..f.span.start].trim_end();
            if before.ends_with(':') {
                return None;
            }
            let (calls, fixes) = calls_from_json(&text[f.body.clone()], tools)?;
            let found = finish_calls(calls, fixes, Format::Fence, f.span.clone(), tools)?;
            Some((found, f.span.end))
        }
        Kind::Bare => {
            let len = balanced_len(&text[start..])?;
            let span = start..start + len;
            let (calls, fixes) = calls_from_json(&text[span.clone()], tools)?;
            let found = finish_calls(calls, fixes, Format::Bare, span.clone(), tools)?;
            Some((found, span.end))
        }
    }
}

fn read_hermes(text: &str, start: usize, tools: ToolSet<'_>) -> Option<(Vec<ScannedCall>, usize)> {
    let body_start = start + HERMES_OPEN.len();
    let (body, end) = match text[body_start..].find(HERMES_CLOSE) {
        Some(e) => (&text[body_start..body_start + e], body_start + e + HERMES_CLOSE.len()),
        None => (&text[body_start..], text.len()),
    };
    let trimmed = body.trim();
    // Qwen's own dialect, wrapped.
    if trimmed.starts_with("<function=") {
        let xml = xml_calls(trimmed);
        if xml.is_empty() || xml.iter().any(|x| !x.complete) {
            return None;
        }
        let mut out = Vec::new();
        for x in &xml {
            out.push(xml_to_call(x, Format::Qwen, start..end, tools)?);
        }
        return Some((out, end));
    }
    if trimmed.contains("<arg_key>") {
        let x = glm_call(trimmed)?;
        let call = xml_to_call(&x, Format::Glm, start..end, tools)?;
        return Some((vec![call], end));
    }
    let (calls, fixes) = calls_from_json(trimmed, tools)?;
    let found = finish_calls(calls, fixes, Format::Hermes, start..end, tools)?;
    Some((found, end))
}

fn read_mistral(text: &str, start: usize, tools: ToolSet<'_>) -> Option<(Vec<ScannedCall>, usize)> {
    let after = start + MISTRAL.len();
    let rest = text[after..].trim_start();
    let json_start = text.len() - rest.len();
    // `[TOOL_CALLS] name[ARGS]{json}` (the older template).
    if !rest.starts_with(['[', '{']) {
        let name_end = rest.find("[ARGS]")?;
        let name = rest[..name_end].trim();
        if name.is_empty() || name.contains(char::is_whitespace) {
            return None;
        }
        let args_start = json_start + name_end + "[ARGS]".len();
        let len = balanced_len(&text[args_start..])?;
        let raw = Value::String(text[args_start..args_start + len].to_string());
        let end = args_start + len;
        let call = build_call(name, &raw, Format::Mistral, start..end, Vec::new(), tools)?;
        return Some((vec![call], end));
    }
    let len = balanced_len(rest)?;
    let (calls, fixes) = calls_from_json(&rest[..len], tools)?;
    let end = json_start + len;
    let found = finish_calls(calls, fixes, Format::Mistral, start..end, tools)?;
    Some((found, end))
}

fn xml_to_call(x: &XmlCall, format: Format, span: Range<usize>, tools: ToolSet<'_>) -> Option<ScannedCall> {
    if !x.complete {
        return None;
    }
    let spec = resolve(&x.name, tools)?;
    let mut args = Map::new();
    for (k, v) in &x.params {
        args.insert(k.clone(), typed_param(v, spec.as_ref().map(|s| s.0)));
    }
    build_call(&x.name, &Value::Object(args), format, span, Vec::new(), tools)
}

/// An XML parameter value is text. With the tool's schema the coercion pass types it; without,
/// whole numbers become numbers (what the markup reader always did).
fn typed_param(v: &str, spec: Option<&ToolSpec>) -> Value {
    if spec.is_some() {
        return Value::String(v.to_string());
    }
    match v.parse::<i64>() {
        Ok(n) => Value::from(n),
        Err(_) => Value::String(v.to_string()),
    }
}

fn resolve<'a>(name: &str, tools: ToolSet<'a>) -> Option<Option<(&'a ToolSpec, bool)>> {
    match tools {
        ToolSet::Any => Some(None),
        ToolSet::Known(list) => match_tool(name, list).map(Some),
    }
}

fn build_call(
    name: &str,
    raw_args: &Value,
    format: Format,
    span: Range<usize>,
    mut fixes: Vec<Fix>,
    tools: ToolSet<'_>,
) -> Option<ScannedCall> {
    let resolved = resolve(name, tools)?;
    let (spec, renamed) = match resolved {
        Some((s, renamed)) => (Some(s), renamed),
        None => (None, false),
    };
    if renamed {
        fixes.push(Fix::ToolName);
    }
    let outcome = repair_args(raw_args, spec);
    let args = match outcome {
        ArgsOutcome::Clean(v) => v,
        ArgsOutcome::Repaired(v, f) => {
            fixes.extend(f);
            v
        }
        // Unreadable or cut-off arguments in text are not worth guessing at.
        ArgsOutcome::Unusable { .. } => return None,
    };
    fixes.push(Fix::FromText);
    fixes.sort();
    fixes.dedup();
    let name = spec.map_or_else(|| name.trim().to_string(), |s| s.name.clone());
    Some(ScannedCall { name, args, format, fixes, span })
}

/// A JSON payload's calls as `(name, raw arguments)`, plus the fixes reading it needed.
fn calls_from_json(body: &str, tools: ToolSet<'_>) -> Option<(Vec<(String, Value)>, Vec<Fix>)> {
    let Parsed::Ok { value, fixes } = parse_lenient(body) else { return None };
    let calls = call_objects(&value, tools)?;
    if calls.is_empty() {
        return None;
    }
    Some((calls, fixes))
}

fn finish_calls(
    calls: Vec<(String, Value)>,
    fixes: Vec<Fix>,
    format: Format,
    span: Range<usize>,
    tools: ToolSet<'_>,
) -> Option<Vec<ScannedCall>> {
    let mut out = Vec::new();
    for (name, raw) in calls {
        out.push(build_call(&name, &raw, format, span.clone(), fixes.clone(), tools)?);
    }
    Some(out)
}

const NAME_KEYS: [&str; 5] = ["name", "tool", "tool_name", "function_name", "action"];
const ARG_KEYS: [&str; 6] = ["arguments", "parameters", "args", "input", "params", "action_input"];
const META_KEYS: [&str; 7] = ["name", "tool", "tool_name", "function_name", "action", "id", "type"];

/// The calls a parsed JSON value expresses, or `None` if it is not (entirely) calls.
fn call_objects(v: &Value, tools: ToolSet<'_>) -> Option<Vec<(String, Value)>> {
    match v {
        Value::Array(items) => {
            if let [Value::String(name), args @ Value::Object(_)] = items.as_slice() {
                return Some(vec![(name.clone(), args.clone())]);
            }
            let mut out = Vec::new();
            for it in items {
                out.extend(call_objects(it, tools)?);
            }
            Some(out)
        }
        Value::Object(o) => {
            if let Some(Value::Array(list)) = o.get("tool_calls") {
                return call_objects(&Value::Array(list.clone()), tools);
            }
            if let Some(f) = o.get("function") {
                match f {
                    Value::Object(fo) => {
                        let name = fo.get("name").and_then(Value::as_str)?;
                        let args = ARG_KEYS.iter().find_map(|k| fo.get(*k)).cloned().unwrap_or_else(|| Value::Object(Map::new()));
                        return Some(vec![(name.to_string(), args)]);
                    }
                    Value::String(name) => {
                        let args = ARG_KEYS.iter().find_map(|k| o.get(*k)).cloned().unwrap_or_else(|| Value::Object(Map::new()));
                        return Some(vec![(name.clone(), args)]);
                    }
                    _ => return None,
                }
            }
            if let Some(name) = NAME_KEYS.iter().find_map(|k| o.get(*k)).and_then(Value::as_str) {
                if let Some(args) = ARG_KEYS.iter().find_map(|k| o.get(*k)) {
                    return Some(vec![(name.to_string(), args.clone())]);
                }
                // Flat form: the other keys are the arguments.
                let flat: Map<String, Value> =
                    o.iter().filter(|(k, _)| !META_KEYS.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
                return Some(vec![(name.to_string(), Value::Object(flat))]);
            }
            // `{"read": {...}}`: one key that is a tool.
            if let (ToolSet::Known(list), true) = (tools, o.len() == 1) {
                let (k, val) = o.iter().next()?;
                if val.is_object() && match_tool(k, list).is_some() {
                    return Some(vec![(k.clone(), val.clone())]);
                }
            }
            None
        }
        _ => None,
    }
}

/// The length of the `{..}`/`[..]` value starting `text`, string-aware for both quote styles;
/// `None` while it is still open or `text` does not start with a bracket.
pub fn balanced_len(text: &str) -> Option<usize> {
    if !text.starts_with(['{', '[']) {
        return None;
    }
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut esc = false;
    for (i, c) in text.char_indices() {
        if let Some(q) = quote {
            match c {
                '\\' if !esc => esc = true,
                c if c == q && !esc => quote = None,
                _ => esc = false,
            }
            continue;
        }
        match c {
            '"' | '\'' => quote = Some(c),
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + c.len_utf8());
                }
            }
            _ => {}
        }
    }
    None
}

fn prose_without(text: &str, calls: &[ScannedCall]) -> String {
    let mut spans: Vec<Range<usize>> = calls.iter().map(|c| c.span.clone()).collect();
    spans.sort_by_key(|s| s.start);
    spans.dedup();
    let mut out = String::new();
    let mut at = 0;
    for s in spans {
        if s.start < at {
            continue;
        }
        out.push_str(&text[at..s.start]);
        at = s.end;
    }
    out.push_str(&text[at..]);
    let mut tidy = String::with_capacity(out.len());
    let mut blank_run = 0;
    for line in out.split('\n') {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        tidy.push_str(line);
        tidy.push('\n');
    }
    tidy.trim().to_string()
}

// ---- the stream guard ------------------------------------------------------------------------

/// Withholds the part of a streaming answer that may be a tool call, so the markup never reaches
/// the screen. Everything before a call marker streams normally; from the marker on, text is held
/// until [`take_held`](Self::take_held) at the end of the call, when [`scan`] decides. A held
/// bare-JSON candidate that closes and is not a call is released at once.
pub struct LeakGuard {
    tools: Option<Vec<ToolSpec>>,
    pending: String,
    held: bool,
    fence_open: bool,
    mid_line: bool,
    /// The start of the current line, read piece by piece, so a fence that opens or closes across
    /// pieces is still counted once.
    line: String,
    line_done: bool,
    /// The last non-space character released: a colon before a fence marks it as an example.
    last_sig: Option<char>,
    /// The hold is an untagged fence whose body looks like a call. It ends, as text, when the
    /// fence has closed and more follows it.
    fence_hold: bool,
}

impl LeakGuard {
    /// `tools` is the request's tool list; `None` guards only the explicit markup forms.
    pub fn new(tools: Option<Vec<ToolSpec>>) -> Self {
        Self {
            tools,
            pending: String::new(),
            held: false,
            fence_open: false,
            mid_line: false,
            line: String::new(),
            line_done: false,
            last_sig: None,
            fence_hold: false,
        }
    }

    fn set(&self) -> ToolSet<'_> {
        match &self.tools {
            Some(t) => ToolSet::Known(t),
            None => ToolSet::Any,
        }
    }

    /// Feeds streamed text; returns what may be shown now.
    pub fn push(&mut self, text: &str) -> String {
        self.pending.push_str(text);
        let mut shown = String::new();
        if self.held {
            // A held fence that closed with more text after it was an example: show it, go on.
            let Some(end) = self.fence_release_point().filter(|_| self.fence_hold) else {
                return String::new();
            };
            let head: String = self.pending.drain(..end).collect();
            self.advance(&head);
            shown.push_str(&head);
            self.held = false;
            self.fence_hold = false;
        }
        loop {
            let hold = self.next_hold();
            match hold {
                Hold::None => {
                    let all = std::mem::take(&mut self.pending);
                    self.advance(&all);
                    shown.push_str(&all);
                    return shown;
                }
                Hold::Prefix(at) => {
                    let head: String = self.pending.drain(..at).collect();
                    self.advance(&head);
                    shown.push_str(&head);
                    return shown;
                }
                Hold::Block(at) | Hold::FenceBlock(at) => {
                    let fence = matches!(hold, Hold::FenceBlock(_));
                    let head: String = self.pending.drain(..at).collect();
                    self.advance(&head);
                    shown.push_str(&head);
                    self.held = true;
                    self.fence_hold = fence;
                    return shown;
                }
                Hold::Release(upto) => {
                    let head: String = self.pending.drain(..upto).collect();
                    self.advance(&head);
                    shown.push_str(&head);
                }
            }
        }
    }

    /// Everything still withheld, for the final scan. The guard is empty afterwards.
    pub fn take_held(&mut self) -> String {
        self.held = false;
        self.fence_hold = false;
        std::mem::take(&mut self.pending)
    }

    /// Drops the guard's state because the text so far turned out to be reasoning.
    pub fn reset(&mut self) -> String {
        self.fence_open = false;
        self.mid_line = false;
        self.take_held()
    }

    /// Whether text is being withheld.
    pub fn is_holding(&self) -> bool {
        self.held || !self.pending.is_empty()
    }

    /// Tracks fence and line state across released text.
    fn advance(&mut self, released: &str) {
        for ch in released.chars() {
            if ch == '\n' {
                self.line.clear();
                self.line_done = false;
                continue;
            }
            if !ch.is_whitespace() {
                self.last_sig = Some(ch);
            }
            if self.line_done || (self.line.is_empty() && (ch == ' ' || ch == '\t')) {
                continue;
            }
            self.line.push(ch);
            if self.line == "```" {
                self.fence_open = !self.fence_open;
                self.line_done = true;
            } else if !"```".starts_with(self.line.as_str()) {
                self.line_done = true;
            }
        }
        if let Some(last) = released.chars().last() {
            self.mid_line = last != '\n';
        }
    }

    fn next_hold(&self) -> Hold {
        let text = self.pending.as_str();
        let mut fence = self.fence_open;
        let mut line_start = !self.mid_line;
        let mut offset = 0;
        for line in text.split_inclusive('\n') {
            let at_line_start = line_start;
            let body_start = offset;
            offset += line.len();
            line_start = line.ends_with('\n');
            let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
            let trimmed = &line[indent..];
            if at_line_start && trimmed.starts_with("```") {
                let info = trimmed.trim_start_matches('`').trim().to_ascii_lowercase();
                let tag = matches!(info.as_str(), "tool_call" | "tool_code" | "tool");
                if !fence {
                    if tag && line.ends_with('\n') {
                        return Hold::Block(body_start + indent);
                    }
                    if matches!(info.as_str(), "" | "json") && line.ends_with('\n') && self.tools.is_some() {
                        match self.json_fence_state(text, body_start + indent, offset) {
                            FenceCall::No => {}
                            FenceCall::Wait => return Hold::Prefix(body_start + indent),
                            FenceCall::Hold => return Hold::FenceBlock(body_start + indent),
                            FenceCall::Example(end) => return Hold::Release(end),
                        }
                    }
                    // The info word is still being written: "```to" may become "```tool_call".
                    if !line.ends_with('\n')
                        && offset == text.len()
                        && ["tool_call", "tool_code", "json"].iter().any(|t| t.starts_with(info.as_str()))
                    {
                        return Hold::Prefix(body_start + indent);
                    }
                }
                fence = !fence;
                continue;
            }
            // "``" at a line start may become a fence: wait for the third backtick.
            if at_line_start && !fence && offset == text.len() && !trimmed.is_empty() && trimmed.chars().all(|c| c == '`') {
                return Hold::Prefix(body_start + indent);
            }
            if fence {
                continue;
            }
            // Markers anywhere in the line.
            let mut earliest: Option<usize> = None;
            for m in [HERMES_OPEN, "<function=", MISTRAL] {
                if let Some(p) = line.find(m) {
                    earliest = Some(earliest.map_or(p, |e| e.min(p)));
                }
            }
            if let Some(p) = earliest {
                return Hold::Block(body_start + p);
            }
            // A bare JSON candidate at the start of a line.
            if at_line_start && matches!(self.tools, Some(_)) && (trimmed.starts_with('{') || trimmed.starts_with('[')) {
                let rest = &text[body_start + indent..];
                match self.bare_state(rest) {
                    Bare::No => {}
                    Bare::Maybe => return Hold::Prefix(body_start + indent),
                    Bare::Call => return Hold::Block(body_start + indent),
                    Bare::Closed(len) => return Hold::Release(body_start + indent + len),
                    Bare::Open => return Hold::Block(body_start + indent),
                }
            }
            // A partial marker at the very end of the text.
            if offset == text.len() {
                if let Some(p) = partial_marker_at_end(line) {
                    return Hold::Prefix(body_start + p);
                }
            }
        }
        Hold::None
    }

    /// Whether the untagged fence opening at `fence_start` (its first line ends at `body_off`)
    /// is, so far, a call written in a fence.
    fn json_fence_state(&self, text: &str, fence_start: usize, body_off: usize) -> FenceCall {
        let before = text[..fence_start].trim_end();
        let colon = if before.is_empty() { self.last_sig == Some(':') } else { before.ends_with(':') };
        if colon {
            return FenceCall::No;
        }
        let body = &text[body_off..];
        let trimmed = body.trim_start();
        if trimmed.is_empty() {
            return FenceCall::Wait;
        }
        if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
            return FenceCall::No;
        }
        match self.bare_state(trimmed) {
            Bare::No | Bare::Closed(_) => FenceCall::No,
            Bare::Maybe => FenceCall::Wait,
            Bare::Open | Bare::Call => match fence_close(body) {
                Some(end) if !text[body_off + end..].trim().is_empty() => FenceCall::Example(body_off + end),
                _ => FenceCall::Hold,
            },
        }
    }

    /// Where a held fence ends, when it has closed and more text follows it.
    fn fence_release_point(&self) -> Option<usize> {
        let first = self.pending.find('\n')? + 1;
        let close = fence_close(&self.pending[first..])?;
        let end = first + close;
        (!self.pending[end..].trim().is_empty()).then_some(end)
    }

    fn bare_state(&self, rest: &str) -> Bare {
        let ToolSet::Known(list) = self.set() else { return Bare::No };
        let mut keys: Vec<String> = ["name", "tool", "function", "tool_name", "function_name", "action", "tool_calls"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        keys.extend(list.iter().map(|t| t.name.clone()));
        let after_open = rest[1..].trim_start();
        let mut inner = after_open;
        if rest.starts_with('[') {
            // `[{"name": ..}]` and `["read", {..}]`
            inner = after_open.strip_prefix('{').map_or(after_open, |s| s.trim_start());
        }
        let compatible = |s: &str| -> Option<bool> {
            // Some(true): a key starts here; Some(false) maybe (prefix of a key); None no.
            if s.is_empty() {
                return Some(false);
            }
            let quote = s.chars().next()?;
            if quote != '"' && quote != '\'' {
                return None;
            }
            let body = &s[1..];
            let mut maybe = false;
            for k in &keys {
                if body.starts_with(k.as_str()) && body[k.len()..].starts_with(quote) {
                    return Some(true);
                }
                if k.starts_with(body) {
                    maybe = true;
                }
            }
            maybe.then_some(false)
        };
        match compatible(inner) {
            None => Bare::No,
            Some(false) => Bare::Maybe,
            Some(true) => match balanced_len(rest) {
                None => Bare::Open,
                Some(len) => {
                    if scan(&rest[..len], self.set()).calls.is_empty() {
                        Bare::Closed(len)
                    } else {
                        Bare::Call
                    }
                }
            },
        }
    }
}

enum Hold {
    /// Nothing to withhold.
    None,
    /// Show everything before this offset, keep the rest pending (it may become a marker).
    Prefix(usize),
    /// A call starts here: show what is before it, hold from here to the end of the stream.
    Block(usize),
    /// An untagged fence that looks like a call starts here; as `Block`, but released as text if
    /// the fence closes and more text follows.
    FenceBlock(usize),
    /// Show everything before this offset (a closed non-call candidate), then look again.
    Release(usize),
}

/// What an untagged fence looks like so far.
enum FenceCall {
    /// An ordinary fence (or an example).
    No,
    /// Not enough written yet to tell.
    Wait,
    /// A call: hold it until the stream ends.
    Hold,
    /// A call-shaped block that was followed by more text: release through this offset.
    Example(usize),
}

/// Offset just past the line that closes a fence whose body is `body`.
fn fence_close(body: &str) -> Option<usize> {
    let mut offset = 0;
    for line in body.split_inclusive('\n') {
        offset += line.len();
        if line.trim_start_matches([' ', '\t']).starts_with("```") {
            return Some(offset);
        }
    }
    None
}

enum Bare {
    No,
    Maybe,
    Open,
    Call,
    Closed(usize),
}

/// Offset in `line` where a proper prefix of a marker begins at the line's end.
fn partial_marker_at_end(line: &str) -> Option<usize> {
    let mut best: Option<usize> = None;
    for m in [HERMES_OPEN, "<function=", MISTRAL] {
        for len in (1..m.len()).rev() {
            if line.ends_with(&m[..len]) {
                let p = line.len() - len;
                best = Some(best.map_or(p, |b| b.min(p)));
                break;
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn t(name: &str, props: Value) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: String::new(),
            input_schema: json!({"type": "object", "properties": props}),
        }
    }

    fn tools() -> Vec<ToolSpec> {
        vec![
            t("read", json!({"path": {"type": "string"}, "limit": {"type": "integer"}})),
            t("write", json!({"path": {"type": "string"}, "content": {"type": "string"}})),
            t("bash", json!({"command": {"type": "string"}})),
            t("grep", json!({"pattern": {"type": "string"}})),
            t("ask_user", json!({"question": {"type": "string"}, "options": {"type": "array"}, "multi_select": {"type": "boolean"}})),
        ]
    }

    fn names(s: &Scan) -> Vec<&str> {
        s.calls.iter().map(|c| c.name.as_str()).collect()
    }

    #[test]
    fn hermes_blocks_are_calls_and_the_prose_stays() {
        let list = tools();
        let s = scan(
            "Thinking... <tool_call>{\"name\":\"bash\",\"arguments\":{\"command\":\"ls\"}}</tool_call> done",
            ToolSet::Known(&list),
        );
        assert_eq!(names(&s), ["bash"]);
        assert_eq!(s.calls[0].args, json!({"command": "ls"}));
        assert_eq!(s.calls[0].format, Format::Hermes);
        assert!(s.calls[0].fixes.contains(&Fix::FromText));
        assert_eq!(s.prose, "Thinking...  done");
    }

    #[test]
    fn qwen_xml_is_typed_by_the_schema_not_guessed() {
        let list = tools();
        let s = scan(
            "Let me look.\n<function=read><parameter=path>42</parameter><parameter=limit>40</parameter></function>",
            ToolSet::Known(&list),
        );
        assert_eq!(s.calls[0].args, json!({"path": "42", "limit": 40}), "a string stays a string, an integer becomes one");
        assert_eq!(s.prose, "Let me look.");
        let any = scan("<function=whatever><parameter=n>7</parameter></function>", ToolSet::Any);
        assert_eq!(any.calls[0].args, json!({"n": 7}));
        assert_eq!(any.calls[0].name, "whatever");
    }

    #[test]
    fn wrapped_qwen_and_glm_forms() {
        let list = tools();
        let wrapped = scan("<tool_call>\n<function=bash>\n<parameter=command>\nls -la\n</parameter>\n</function>\n</tool_call>", ToolSet::Known(&list));
        assert_eq!(names(&wrapped), ["bash"]);
        assert_eq!(wrapped.calls[0].args, json!({"command": "ls -la"}));
        assert_eq!(wrapped.prose, "");
        let glm = scan("<tool_call>read\n<arg_key>path</arg_key>\n<arg_value>a.md</arg_value>\n</tool_call>", ToolSet::Known(&list));
        assert_eq!((names(&glm), glm.calls[0].format), (vec!["read"], Format::Glm));
        assert_eq!(glm.calls[0].args, json!({"path": "a.md"}));
    }

    #[test]
    fn mistral_blocks_yield_every_call() {
        let list = tools();
        let s = scan(r#"[TOOL_CALLS][{"name":"read","arguments":{"path":"a"}},{"name":"read","arguments":{"path":"b"}}]"#, ToolSet::Known(&list));
        assert_eq!(names(&s), ["read", "read"]);
        let old = scan(r#"[TOOL_CALLS] read[ARGS]{"path": "x"}"#, ToolSet::Known(&list));
        assert_eq!(old.calls[0].args, json!({"path": "x"}));
    }

    #[test]
    fn bare_json_on_its_own_line_is_a_call_in_every_shape() {
        let list = tools();
        let cases = [
            "\n{\"name\": \"write\", \"arguments\": {\"path\": \"x.txt\", \"content\": \"hi\"}}",
            "\n{\"name\": \"ask_user\", \"question\": \"What language?\", \"options\": [\"Python\", \"Rust\"]}",
            "\n{\"ask_user\": {\"question\": \"Pick one\", \"options\": [\"A\", \"B\"]}}",
            "\n{'name': 'ask_user', 'arguments': {'question': 'Ready?', 'multi_select': False}}",
            "\n{\"function\": {\"name\": \"read\", \"arguments\": \"{\\\"path\\\": \\\"a\\\"}\"}}",
            "\n{\"tool\": \"grep\", \"parameters\": {\"pattern\": \"x\"}}",
            "\n[\"read\", {\"path\": \"Cargo.toml\"}]",
            "\n{\"tool_calls\": [{\"type\": \"function\", \"id\": \"c1\", \"function\": {\"name\": \"read\", \"arguments\": {\"path\": \"a\"}}}]}",
            "\n{\n  \"name\": \"read\",\n  \"arguments\": {\"path\": \"a.rs\"}\n}",
        ];
        for c in cases {
            let s = scan(c, ToolSet::Known(&list));
            assert_eq!(s.calls.len(), 1, "{c:?} -> {s:?}");
            assert_eq!(s.calls[0].format, Format::Bare);
        }
        let ask = scan(cases[3], ToolSet::Known(&list));
        assert_eq!(ask.calls[0].args["multi_select"], json!(false));
    }

    #[test]
    fn bare_json_with_an_unknown_name_is_just_prose() {
        let list = tools();
        for text in [
            "Here:\n{\"name\": \"Alice\", \"age\": 3}\n",
            "{\"id\": 1}\n",
            "{\"name\": \"nope\", \"arguments\": {}}",
            "[1, 2, 3]",
            "[link](https://x.y)",
        ] {
            let s = scan(text, ToolSet::Known(&list));
            assert!(s.calls.is_empty(), "{text:?} ran {:?}", s.calls);
        }
        // And with no tool list at all, bare JSON is never a call.
        assert!(scan("\n{\"name\": \"read\", \"arguments\": {}}", ToolSet::Any).calls.is_empty());
    }

    #[test]
    fn fenced_examples_are_never_calls() {
        let list = tools();
        let example = "{\"name\": \"write\", \"arguments\": {\"path\": \"README.md\", \"content\": \"\"}}";
        let s = scan(&format!("Example:\n```json\n{example}\n```\nDone."), ToolSet::Known(&list));
        assert!(s.calls.is_empty());
        assert!(s.prose.contains(example), "the example stays in the answer");
        let hermes = "<tool_call>{\"name\":\"bash\",\"arguments\":{\"command\":\"rm -rf /\"}}</tool_call>";
        let s = scan(&format!("Format:\n```\n{hermes}\n```\n"), ToolSet::Known(&list));
        assert!(s.calls.is_empty(), "{:?}", s.calls);
        let qwen = "<function=bash><parameter=command>rm -rf /</parameter></function>";
        assert!(scan(&format!("```xml\n{qwen}\n```"), ToolSet::Known(&list)).calls.is_empty());
        let rust = "Look:\n```rust\n{\"name\": \"bash\", \"x\": 1}\n```\nend";
        assert!(scan(rust, ToolSet::Known(&list)).calls.is_empty());
        // A fence that closed does not hide what comes after it.
        let after = scan(&format!("```\ncode\n```\n{hermes}"), ToolSet::Known(&list));
        assert_eq!(names(&after), ["bash"]);
    }

    #[test]
    fn a_tagged_fence_is_a_call_and_leaves_nothing_open() {
        let list = tools();
        let fenced = "```tool_call\n{\"name\": \"read\", \"arguments\": {\"path\": \"a.rs\"}}\n```";
        let then = "<tool_call>{\"name\":\"read\",\"arguments\":{\"path\":\"b.rs\"}}</tool_call>";
        let s = scan(&format!("Reading both.\n{fenced}\n{then}"), ToolSet::Known(&list));
        assert_eq!(names(&s), ["read", "read"]);
        assert_eq!(s.calls[1].args, json!({"path": "b.rs"}));
        assert!(!s.prose.contains("```"), "{:?}", s.prose);
        // Its closing brace forgotten between closed fences; or the stream ended on the JSON.
        assert_eq!(names(&scan("```tool_call\n{\"name\": \"read\", \"arguments\": {\"path\": \"a\"}\n```\nok", ToolSet::Known(&list))), ["read"]);
        assert_eq!(names(&scan("```tool_call\n{\"name\": \"read\", \"arguments\": {}}", ToolSet::Known(&list))), ["read"]);
    }

    #[test]
    fn an_untagged_json_fence_runs_only_as_the_last_thing_and_not_after_a_colon() {
        let list = tools();
        let call = "```json\n[\n  \"read\",\n  {\n    \"path\": \"Cargo.toml\"\n  }\n]\n```";
        assert_eq!(names(&scan(&format!("Reading Cargo.toml...\n{call}"), ToolSet::Known(&list))), ["read"]);
        assert!(scan(&format!("Reading Cargo.toml...\n{call}\ndone"), ToolSet::Known(&list)).calls.is_empty());
        assert!(scan(&format!("Here is the call:\n{call}"), ToolSet::Known(&list)).calls.is_empty());
    }

    #[test]
    fn blocks_cut_off_mid_value_are_not_completed() {
        let list = tools();
        for cut in [
            "<tool_call>{\"name\":\"write\",\"arguments\":{\"path\":\"src/main.rs\",\"content\":\"fn main() {\\n let x = compute(",
            "\n{\"name\": \"read\"",
            "<function=write><parameter=path>a.py</parameter><parameter=content>def f(",
            "[TOOL_CALLS][{\"name\":\"read\",\"arguments\":{\"path\":\"a",
            "<tool_call>not json at all",
            "<tool_call>@@@</tool_call> after",
        ] {
            let s = scan(cut, ToolSet::Known(&list));
            assert!(s.calls.is_empty(), "{cut:?} ran {:?}", s.calls);
            assert!(s.prose.contains(cut.trim()), "the text is kept: {:?}", s.prose);
        }
    }

    #[test]
    fn a_block_missing_only_its_close_still_runs_when_its_json_is_whole() {
        let list = tools();
        let s = scan("Let me look. <tool_call>{\"name\":\"read\",\"arguments\":{\"path\":\"a.rs\"}}</tool_", ToolSet::Known(&list));
        assert_eq!(names(&s), ["read"], "a cut close tag after whole json is still the call");
        let s = scan("<tool_call>{\"name\":\"read\",\"arguments\":{\"path\":\"a.rs\"}}", ToolSet::Known(&list));
        assert_eq!(names(&s), ["read"]);
    }

    #[test]
    fn names_resolve_through_case_and_namespace() {
        let list = tools();
        let s = scan("<tool_call>{\"name\":\"Read\",\"arguments\":{\"path\":\"a\"}}</tool_call>", ToolSet::Known(&list));
        assert_eq!(names(&s), ["read"]);
        assert!(s.calls[0].fixes.contains(&Fix::ToolName));
        let s = scan("<tool_call>{\"name\":\"functions.read\",\"arguments\":{\"path\":\"a\"}}</tool_call>", ToolSet::Known(&list));
        assert_eq!(names(&s), ["read"]);
        let s = scan("<tool_call>{\"name\":\"fetch\",\"arguments\":{}}</tool_call>", ToolSet::Known(&list));
        assert!(s.calls.is_empty() && s.rejected == 1);
    }

    #[test]
    fn arguments_are_repaired_like_native_ones() {
        let list = tools();
        let s = scan("<tool_call>{\"name\":\"bash\",\"arguments\":{\"command\":\"cargo test\",}}</tool_call>", ToolSet::Known(&list));
        assert_eq!(s.calls[0].args, json!({"command": "cargo test"}));
        assert!(s.calls[0].fixes.contains(&Fix::TrailingComma));
        let s = scan("<tool_call>{\"name\":\"read\",\"arguments\":\"{\\\"path\\\": \\\"a\\\", \\\"limit\\\": \\\"5\\\"}\"}</tool_call>", ToolSet::Known(&list));
        assert_eq!(s.calls[0].args, json!({"path": "a", "limit": 5}));
    }

    #[test]
    fn calls_and_prose_keep_their_order_and_spans() {
        let list = tools();
        let text = "First.\n<function=read><parameter=path>a</parameter></function>\nThen:\n<function=read><parameter=path>b</parameter></function>\nBye.";
        let s = scan(text, ToolSet::Known(&list));
        assert_eq!(s.calls.len(), 2);
        assert!(text[s.calls[0].span.clone()].contains("path>a"));
        assert!(text[s.calls[1].span.clone()].contains("path>b"));
        assert_eq!(s.prose, "First.\n\nThen:\n\nBye.");
    }

    fn feed_all(tools: Option<Vec<ToolSpec>>, chunks: &[&str]) -> (String, String) {
        let mut g = LeakGuard::new(tools);
        let mut shown = String::new();
        for c in chunks {
            shown.push_str(&g.push(c));
        }
        (shown, g.take_held())
    }

    fn pieces(text: &str, n: usize) -> Vec<String> {
        text.chars().collect::<Vec<_>>().chunks(n).map(|c| c.iter().collect()).collect()
    }

    #[test]
    fn the_guard_streams_prose_and_withholds_from_the_marker_on() {
        let list = tools();
        let text = "Let me look at it.\n<function=read><parameter=path>a.md</parameter></function>";
        for n in [1, 3, 7, 1000] {
            let cut = pieces(text, n);
            let refs: Vec<&str> = cut.iter().map(String::as_str).collect();
            let (shown, held) = feed_all(Some(list.clone()), &refs);
            assert_eq!(shown, "Let me look at it.\n", "pieces of {n}");
            assert_eq!(format!("{shown}{held}"), text);
        }
        let (shown, held) = feed_all(Some(list.clone()), &["Done <", "tool", "_call>{\"na", "me\":\"read\"}"]);
        assert_eq!(shown, "Done ");
        assert!(held.starts_with("<tool_call>"));
    }

    #[test]
    fn the_guard_lets_ordinary_text_and_look_alikes_through() {
        let list = tools();
        for text in [
            "Use a < b and the <b>bold</b> tag.",
            "An array [1, 2, 3] in prose.\nAnd { braces } too.",
            "The function reads a file.",
            "[link](http://x) text",
            "Here is data:\n{\"id\": 1, \"v\": 2}\nthat is all.",
        ] {
            let cut = pieces(text, 2);
            let refs: Vec<&str> = cut.iter().map(String::as_str).collect();
            let (shown, held) = feed_all(Some(list.clone()), &refs);
            assert_eq!(format!("{shown}{held}"), text);
            assert_eq!(held, "", "{text:?} was held: {held:?}");
        }
    }

    #[test]
    fn the_guard_holds_a_bare_call_but_not_one_inside_a_fence() {
        let list = tools();
        let call = "{\"name\": \"read\", \"arguments\": {\"path\": \"a\"}}";
        for n in [1, 4, 100] {
            let cut = pieces(&format!("Reading.\n{call}"), n);
            let refs: Vec<&str> = cut.iter().map(String::as_str).collect();
            let (shown, held) = feed_all(Some(list.clone()), &refs);
            assert_eq!(shown, "Reading.\n", "pieces of {n}");
            assert_eq!(held, call);
        }
        let fenced = format!("Example:\n```json\n{call}\n```\nDone.");
        let cut = pieces(&fenced, 3);
        let refs: Vec<&str> = cut.iter().map(String::as_str).collect();
        let (shown, held) = feed_all(Some(list.clone()), &refs);
        assert_eq!((shown, held), (fenced.clone(), String::new()));
        // A fence that arrives in its own pieces still hides its example.
        let (shown, held) = feed_all(Some(list.clone()), &["Format:\n", "``", "`\n", call, "\n`", "``\n"]);
        assert_eq!(held, "");
        assert!(shown.contains(call));
    }

    #[test]
    fn the_guard_releases_a_closed_json_that_is_not_a_call() {
        let list = tools();
        let text = "{\"name\": \"Alice\"}\nBut then more text streams on and on.";
        let (shown, held) = feed_all(Some(list), &[text]);
        assert_eq!((shown.as_str(), held.as_str()), (text, ""));
    }

    #[test]
    fn a_tagged_fence_start_is_held_even_split_across_pieces() {
        let list = tools();
        let text = "Reading.\n```tool_call\n{\"name\": \"read\", \"arguments\": {}}\n```";
        let cut = pieces(text, 2);
        let refs: Vec<&str> = cut.iter().map(String::as_str).collect();
        let (shown, held) = feed_all(Some(list), &refs);
        assert_eq!(shown, "Reading.\n");
        assert!(held.starts_with("```tool_call"));
    }

    #[test]
    fn without_a_tool_list_the_guard_still_holds_explicit_markup() {
        let (shown, held) = feed_all(None, &["ok <function=", "read><parameter=path>a</parameter></function>"]);
        assert_eq!(shown, "ok ");
        assert!(held.starts_with("<function=read>"));
    }

    fn in_pieces(text: &str, n: usize) -> (String, String) {
        let cut = pieces(text, n);
        let refs: Vec<&str> = cut.iter().map(String::as_str).collect();
        feed_all(Some(tools()), &refs)
    }

    #[test]
    fn a_fence_closed_a_character_at_a_time_does_not_hide_what_follows() {
        let call = "<tool_call>{\"name\":\"read\",\"arguments\":{\"path\":\"a.rs\"}}</tool_call>";
        for n in [1, 2, 3] {
            let (shown, held) = in_pieces(&format!("```
code
```
{call}"), n);
            assert_eq!(shown, "```
code
```
", "pieces of {n}");
            assert_eq!(held, call);
        }
    }

    #[test]
    fn a_bare_call_opening_with_a_bracket_inside_is_held_whatever_the_piece_size() {
        let text = "
{\"tool_calls\": [{\"type\": \"function\", \"function\": {\"name\": \"read\", \"arguments\": {}}}]}";
        for n in [1, 3, 17, 40] {
            let (shown, held) = in_pieces(text, n);
            assert_eq!(shown, "
", "pieces of {n}");
            assert!(held.starts_with("{\"tool_calls\""), "{held:?}");
        }
    }

    #[test]
    fn an_untagged_json_fence_that_is_a_call_is_held_to_the_end_and_an_example_is_not() {
        let call = "```json
[\"read\", {\"path\": \"Cargo.toml\"}]
```";
        for n in [1, 4, 100] {
            let (shown, held) = in_pieces(&format!("Reading.
{call}"), n);
            assert_eq!(shown, "Reading.
", "pieces of {n}");
            assert_eq!(held, call);
            // Followed by more text it was an example: everything is shown, and the guard moves on.
            let (shown, held) = in_pieces(&format!("Reading.
{call}
Done."), n);
            assert_eq!((shown.as_str(), held.as_str()), (format!("Reading.
{call}
Done.").as_str(), ""), "pieces of {n}");
            // After a colon it is an example from the start.
            let (shown, held) = in_pieces(&format!("Here is the call:
{call}"), n);
            assert_eq!(held, "", "pieces of {n}");
            assert!(shown.ends_with("```"));
        }
        // An ordinary json fence streams as it arrives.
        let (shown, held) = in_pieces("Config:
```json
{\"a\": 1}
```", 3);
        assert_eq!((shown.as_str(), held.as_str()), ("Config:
```json
{\"a\": 1}
```", ""));
    }
}
