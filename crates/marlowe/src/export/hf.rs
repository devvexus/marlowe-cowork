//! Turns a kept trajectory into Hugging Face chat records: one SFT row (`messages` in the
//! OpenAI/transformers tool-calling shape) and step-level DPO rows (TRL's conversational
//! `prompt`/`chosen`/`rejected`). Every string passes through the [`Scrubber`] here.
//!
//! Step DPO pairs come only from inside a kept trajectory, at a step the harness itself judged:
//! - `error_retry`: a tool call failed and the next assistant step called the same tool and
//!   succeeded. Rejected = the failing step, chosen = the corrected one, prompt = what came before.
//! - `denied`: the user denied a call. Rejected = that step, chosen = what the model did next
//!   (when that next step did not fail too).
//!
//! A user message between the two steps (a steer) breaks the pair: the chosen step then answers
//! something the prompt does not contain.

use std::collections::{HashMap, HashSet};

use marlowe_model::{Message, Part, Role, ToolCall};
use serde_json::{json, Value};

use super::scrub::{marker, Scrubber, Tally};

/// The text the agent loop returns for a denied call (`marlowe-agent/src/run_tools.rs`).
pub const DENIED: &str = "The user denied this call.";
/// The text for a call skipped by an interrupt.
pub const INTERRUPTED: &str = "Not run: the user interrupted the turn.";

pub const IMAGE_OMITTED: &str = "[image omitted]";

/// Argument keys whose string values are paths or commands (where a secret file is named).
const PATH_KEYS: &[&str] = &[
    "path", "paths", "file", "file_path", "filepath", "filename", "files", "target", "source", "src", "dest",
    "destination", "from", "to", "cwd", "dir", "directory", "command", "cmd", "script", "notebook_path", "glob",
];

#[derive(Debug, Clone, Copy)]
pub struct HfOptions {
    /// Keep reasoning as `reasoning_content` (signatures and provider payloads are always dropped).
    pub thinking: bool,
}

/// A trajectory converted to HF messages, with each source message's first HF index.
pub struct Converted {
    pub messages: Vec<Value>,
    /// `start[i]`: index in `messages` of source message `i`'s first HF message.
    pub start: Vec<usize>,
}

fn path_like_strings<'a>(v: &'a Value, key: Option<&str>, out: &mut Vec<&'a str>) {
    match v {
        Value::String(s) => {
            if key.is_some_and(|k| PATH_KEYS.contains(&k.to_ascii_lowercase().as_str())) {
                out.push(s);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| path_like_strings(x, key, out)),
        Value::Object(o) => o.iter().for_each(|(k, x)| path_like_strings(x, Some(k), out)),
        _ => {}
    }
}

/// True when the call reads, writes or runs something on a secret-shaped file.
fn touches_secret_file(sc: &Scrubber, call: &ToolCall) -> bool {
    let mut strings = Vec::new();
    path_like_strings(&call.args, None, &mut strings);
    strings.iter().any(|s| sc.mentions_secret_file(s))
}

/// A secret-file call's arguments: a bare path under a path key stays (scrubbed); every other
/// string (contents, edits, a whole command line) is replaced.
fn secret_file_args(sc: &Scrubber, v: &Value, key: Option<&str>, tally: &mut Tally) -> Value {
    match v {
        Value::String(s) => {
            let path_key = key.is_some_and(|k| {
                let k = k.to_ascii_lowercase();
                PATH_KEYS.contains(&k.as_str()) && !matches!(k.as_str(), "command" | "cmd" | "script")
            });
            let bare_path = !s.contains(char::is_whitespace) && !s.contains(['=', '>', '<', '|', ';', '&']);
            if path_key && bare_path && s.len() < 260 {
                Value::String(sc.scrub(s, tally))
            } else {
                *tally.0.entry("secret_file".into()).or_default() += 1;
                Value::String(marker("secret_file"))
            }
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| secret_file_args(sc, x, key, tally)).collect()),
        Value::Object(o) => Value::Object(
            o.iter().map(|(k, x)| (sc.scrub(k, tally), secret_file_args(sc, x, Some(k), tally))).collect(),
        ),
        other => other.clone(),
    }
}

/// Converts and scrubs `msgs`. Call ids are renumbered `call_0..` (provider ids say nothing
/// useful and some embed account data); results keep their links.
pub fn convert(msgs: &[Message], sc: &Scrubber, opts: HfOptions, tally: &mut Tally) -> Converted {
    let mut ids: HashMap<String, String> = HashMap::new();
    let mut names: HashMap<String, String> = HashMap::new();
    let mut secret: HashSet<String> = HashSet::new();
    for m in msgs {
        for c in m.tool_calls() {
            let n = ids.len();
            ids.entry(c.id.clone()).or_insert_with(|| format!("call_{n}"));
            names.insert(c.id.clone(), c.name.clone());
            if touches_secret_file(sc, c) {
                secret.insert(c.id.clone());
            }
        }
    }
    let id_of = |raw: &str, ids: &HashMap<String, String>| ids.get(raw).cloned().unwrap_or_else(|| "call_orphan".into());
    let mut out = Vec::new();
    let mut start = Vec::with_capacity(msgs.len());
    for m in msgs {
        start.push(out.len());
        match m.role {
            Role::System | Role::User => {
                let mut text = sc.scrub(&m.joined_text(), tally);
                for _ in m.images() {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(IMAGE_OMITTED);
                }
                let role = if m.role == Role::System { "system" } else { "user" };
                out.push(json!({ "role": role, "content": text }));
            }
            Role::Assistant => {
                let mut msg = serde_json::Map::new();
                msg.insert("role".into(), json!("assistant"));
                msg.insert("content".into(), json!(sc.scrub(&m.joined_text(), tally)));
                if opts.thinking {
                    if let Some(t) = m.thinking().filter(|t| !t.text.trim().is_empty()) {
                        msg.insert("reasoning_content".into(), json!(sc.scrub(&t.text, tally)));
                    }
                }
                let calls: Vec<Value> = m
                    .tool_calls()
                    .map(|c| {
                        let args = if secret.contains(&c.id) {
                            secret_file_args(sc, &c.args, None, tally)
                        } else {
                            sc.scrub_value(&c.args, tally)
                        };
                        json!({
                            "id": id_of(&c.id, &ids),
                            "type": "function",
                            "function": {
                                "name": sc.scrub(&c.name, tally),
                                "arguments": serde_json::to_string(&args).unwrap_or_else(|_| "{}".into()),
                            }
                        })
                    })
                    .collect();
                if !calls.is_empty() {
                    msg.insert("tool_calls".into(), Value::Array(calls));
                }
                out.push(Value::Object(msg));
            }
            Role::Tool => {
                for p in &m.content {
                    let Part::ToolResult(r) = p else { continue };
                    let mut content = if secret.contains(&r.call_id) {
                        *tally.0.entry("secret_file".into()).or_default() += 1;
                        marker("secret_file")
                    } else {
                        // grep/rg lines from a secret file inside an ordinary search.
                        sc.scrub(&sc.redact_secret_file_lines(&r.content, tally), tally)
                    };
                    if r.images().next().is_some() {
                        content.push('\n');
                        content.push_str(IMAGE_OMITTED);
                    }
                    out.push(json!({
                        "role": "tool",
                        "tool_call_id": id_of(&r.call_id, &ids),
                        "name": sc.scrub(names.get(&r.call_id).map(String::as_str).unwrap_or_default(), tally),
                        "content": content,
                    }));
                }
            }
        }
    }
    Converted { messages: out, start }
}

/// Drops trailing messages until the trajectory ends on an assistant answer (no pending calls),
/// so a mark taken mid-turn never exports a dangling tool call.
pub fn trim_to_answer(msgs: &mut Vec<Message>) {
    while let Some(last) = msgs.last() {
        if last.role == Role::Assistant && last.tool_calls().next().is_none() {
            break;
        }
        msgs.pop();
    }
}

/// A step-level preference pair, as indexes into the source messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepPair {
    pub kind: &'static str,
    pub rejected: usize,
    pub chosen: usize,
}

fn results_of(msgs: &[Message], i: usize) -> Vec<&marlowe_model::ToolResult> {
    match msgs.get(i + 1) {
        Some(m) if m.role == Role::Tool => m.tool_results_iter().collect(),
        _ => Vec::new(),
    }
}

fn step_failed(msgs: &[Message], i: usize) -> bool {
    results_of(msgs, i).iter().any(|r| r.is_error)
}

/// Finds the step pairs in a trajectory (see the module docs).
pub fn step_pairs(msgs: &[Message]) -> Vec<StepPair> {
    let mut out = Vec::new();
    for (i, m) in msgs.iter().enumerate() {
        if m.role != Role::Assistant || m.tool_calls().next().is_none() {
            continue;
        }
        let results = results_of(msgs, i);
        if results.iter().any(|r| r.content.starts_with(INTERRUPTED)) {
            continue;
        }
        let failed: Vec<&str> = results.iter().filter(|r| r.is_error).map(|r| r.call_id.as_str()).collect();
        if failed.is_empty() {
            continue;
        }
        let denied = results.iter().any(|r| r.is_error && r.content.starts_with(DENIED));
        // The next assistant step, with no user message in between.
        let mut j = i + 1;
        while j < msgs.len() && msgs[j].role == Role::Tool {
            j += 1;
        }
        let Some(next) = msgs.get(j).filter(|n| n.role == Role::Assistant) else { continue };
        if next.tool_calls().next().is_some() && (step_failed(msgs, j) || results_of(msgs, j).is_empty()) {
            continue;
        }
        if denied {
            out.push(StepPair { kind: "denied", rejected: i, chosen: j });
            continue;
        }
        let failed_tools: HashSet<&str> =
            m.tool_calls().filter(|c| failed.contains(&c.id.as_str())).map(|c| c.name.as_str()).collect();
        if next.tool_calls().any(|c| failed_tools.contains(c.name.as_str())) {
            out.push(StepPair { kind: "error_retry", rejected: i, chosen: j });
        }
    }
    out
}

/// The DPO row for `pair` over an already converted trajectory; `None` when the two sides came
/// out identical after scrubbing.
pub fn dpo_row(conv: &Converted, pair: &StepPair) -> Option<(Value, Value, Value)> {
    let prompt: Vec<Value> = conv.messages[..conv.start[pair.rejected]].to_vec();
    let rejected = conv.messages[conv.start[pair.rejected]].clone();
    let chosen = conv.messages[conv.start[pair.chosen]].clone();
    if prompt.is_empty() || strip_ids(&rejected) == strip_ids(&chosen) {
        return None;
    }
    Some((Value::Array(prompt), Value::Array(vec![chosen]), Value::Array(vec![rejected])))
}

fn strip_ids(v: &Value) -> Value {
    let mut v = v.clone();
    if let Some(calls) = v.get_mut("tool_calls").and_then(Value::as_array_mut) {
        for c in calls {
            if let Some(o) = c.as_object_mut() {
                o.remove("id");
            }
        }
    }
    v
}
