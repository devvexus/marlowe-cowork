//! The forgiving layer: what a model sent, made into what the tool's schema asks for, or an
//! answer that teaches the exact signature.
//!
//! It runs in two places, driven by the spec the model was actually shown (built-in, daemon or
//! MCP):
//! - [`prepare_call`], in the loop where calls are parsed, before any middleware: tool-name
//!   resolution (exact, then case/separator-insensitive, then the permanent [`LEGACY`] table for
//!   merged names) and the arguments container (null, a JSON string, a bare string). Transcripts,
//!   approvals and the journal then show the canonical name.
//! - [`ArgsLayer`], the first `before_approval` of every agent (conductor, subagent, crew):
//!   keys, types, `requires`. The approval gate, taint and egress judge the normalised call, and an
//!   invalid call is answered here without asking or running.
//!
//! Per-tool data ([`ArgRules`]) sits beside each tool's definition and reaches the layer through
//! [`crate::ToolHost::arg_rules`]. MCP tools get only the lossless JSON/number/boolean/array
//! coercions and their own `required`: no synonyms are guessed for third-party schemas.
//!
//! Never rewritten: free text (`command`, `code`, `content`, `old_string`, `new_string`, `text`,
//! `prompt`, `body`...), a missing required value (never invented), units (never converted), a
//! choice between two candidate keys, an ambiguous enum prefix, and a non-URL into `url`. Unknown
//! keys are dropped only when the tool lists them in [`ArgRules::ignore`], and then with a note.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use marlowe_model::{ToolCall, ToolSpec};
use serde_json::{Map, Value};

use crate::middleware::{CallFlow, ToolMiddleware};
use crate::tools::{CallCtx, ToolHost, ToolOutcome};

/// What a tool needs beyond its schema, declared next to the tool. All static: it is `Copy`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ArgRules {
    /// `(wrong key, schema key)`: names models send for this tool's arguments.
    pub aliases: &'static [(&'static str, &'static str)],
    /// Op-dependent and exactly-one checks the schema cannot say.
    pub requires: &'static [Require],
    /// `op`-folded renames (`project{op:"run", command:X}` -> `{op:X}`).
    pub folds: &'static [Fold],
    /// `(key, wrong value, schema value)`: enum values models send (`gh` op `pr_checks` -> `ci`).
    pub values: &'static [(&'static str, &'static str, &'static str)],
    /// String-array keys that are shell-split when sent as one string (`python.args`).
    pub split: &'static [&'static str],
    /// Keys the harness sets itself: dropped from model input.
    pub strip: &'static [&'static str],
    /// Keys models send that the tool has no use for: dropped with a rewrite and a one-time note
    /// ("ignored `k`: not a parameter of <tool>"), never refused.
    pub ignore: &'static [&'static str],
    /// Keys the tool's own policy guard judges (a force switch the schema does not have): left as
    /// sent, so the guard's refusal, not "unknown key", is what the model reads.
    pub policy_keys: &'static [&'static str],
    /// A valid call, shown in the error text.
    pub example: Option<&'static str>,
    /// A line for known confusions ("To search instead: ...").
    pub see_also: Option<&'static str>,
    /// An arXiv id sitting under an unknown key is read as `url`.
    pub sniff_arxiv: bool,
    /// The tool validates its own arguments, with its own retry accounting
    /// (`structured_output`, whose schema is the script's): the layer leaves the call alone.
    pub skip: bool,
}

impl ArgRules {
    /// No rules: the generic ones only.
    pub const NONE: ArgRules = ArgRules {
        aliases: &[],
        requires: &[],
        folds: &[],
        values: &[],
        split: &[],
        strip: &[],
        ignore: &[],
        policy_keys: &[],
        example: None,
        see_also: None,
        sniff_arxiv: false,
        skip: false,
    };
}

/// A check on the call as a whole.
#[derive(Debug, Clone, Copy)]
pub enum Require {
    /// Exactly one of these keys.
    OneOf(&'static [&'static str]),
    /// When `key` is `is`, every one of `need` must be present, unless one of `unless` is.
    When { key: &'static str, is: &'static str, need: &'static [&'static str], unless: &'static [&'static str] },
    /// When `key` is `is`, at least one of `any` must be present.
    WhenAny { key: &'static str, is: &'static str, any: &'static [&'static str] },
    /// `need` is required unless one of `unless` is present.
    UnlessAny { need: &'static str, unless: &'static [&'static str] },
}

impl Require {
    /// Every key the rule names.
    fn keys(&self) -> Vec<&'static str> {
        match self {
            Require::OneOf(keys) => keys.to_vec(),
            Require::When { key, need, unless, .. } => std::iter::once(*key).chain(need.iter().copied()).chain(unless.iter().copied()).collect(),
            Require::WhenAny { key, any, .. } => std::iter::once(*key).chain(any.iter().copied()).collect(),
            Require::UnlessAny { need, unless } => std::iter::once(*need).chain(unless.iter().copied()).collect(),
        }
    }
}

/// When `when.0` is `when.1` (case-insensitively), the value of `from` moves to `to`.
#[derive(Debug, Clone, Copy)]
pub struct Fold {
    pub when: (&'static str, &'static str),
    pub from: &'static str,
    pub to: &'static str,
}

/// A name that used to be a tool and now is another call: `from` resolves to `to` when `from`
/// is not a tool of the host and `to` is. `inject` is a `(key, JSON literal)` set when absent.
#[derive(Debug, Clone, Copy)]
pub struct Legacy {
    pub from: &'static str,
    pub to: &'static str,
    pub inject: Option<(&'static str, &'static str)>,
}

/// The permanent table: old transcripts, saved workflows and crew allowlists keep working.
pub const LEGACY: &[Legacy] = &[
    Legacy { from: "paper", to: "web_fetch", inject: None },
    Legacy { from: "job_output", to: "task_output", inject: None },
    Legacy { from: "job_watch", to: "task_output", inject: Some(("watch", "true")) },
    Legacy { from: "job_kill", to: "task_stop", inject: None },
    Legacy { from: "cron_create", to: "schedule", inject: None },
    Legacy { from: "schedule_wakeup", to: "schedule", inject: None },
    Legacy { from: "cron_list", to: "task_list", inject: None },
    Legacy { from: "list_agents", to: "task_list", inject: None },
    Legacy { from: "cron_delete", to: "task_stop", inject: None },
    Legacy { from: "todo_add", to: "todo", inject: None },
    Legacy { from: "todo_update", to: "todo", inject: None },
    Legacy { from: "todo_get", to: "todo", inject: None },
    Legacy { from: "ci_status", to: "gh", inject: Some(("op", "\"ci\"")) },
    Legacy { from: "workflow", to: "team", inject: None },
    Legacy { from: "team_task", to: "team", inject: None },
];

/// A tool name as an allowlist or denylist should read it: a merged tool's old name is the tool
/// that now runs the call, so a saved role or agent file keeps its grants and its refusals.
pub fn canonical_tool(name: &str) -> &str {
    LEGACY.iter().find(|l| l.from == name).map_or(name, |l| l.to)
}

/// Key names every tool's models send for the same idea; used only when the tool has the target
/// key and lacks the source key.
const SYNONYMS: &[(&str, &str)] = &[
    ("file_path", "path"),
    ("filepath", "path"),
    ("filename", "path"),
    ("file", "path"),
    ("run_in_background", "background"),
    ("description", "label"),
    ("head_limit", "limit"),
    ("max", "limit"),
    ("max_results", "limit"),
    ("n", "limit"),
    ("cmd", "command"),
    ("q", "query"),
    ("search", "query"),
    ("keywords", "query"),
    ("link", "url"),
    ("href", "url"),
    ("uri", "url"),
    ("url_or_arxiv_id", "url"),
    ("msg", "text"),
];

/// Rules for the tools that live outside the built-in table: memory, `dispatch`, the subagent's
/// own and the MCP resource tools. Their hosts return these from [`crate::ToolHost::arg_rules`].
pub fn wired_rules(name: &str) -> Option<ArgRules> {
    Some(match name {
        "remember" => ArgRules {
            aliases: &[("content", "text"), ("memory", "text"), ("statement", "text"), ("type", "kind"), ("category", "kind")],
            ..ArgRules::NONE
        },
        "recall" => ArgRules {
            aliases: &[("q", "query"), ("search", "query"), ("text", "query"), ("max", "limit"), ("n", "limit")],
            example: Some(r#"recall{"query": "the cluster"}"#),
            ..ArgRules::NONE
        },
        "forget" => ArgRules {
            aliases: &[("memory_id", "id")],
            ..ArgRules::NONE
        },
        "correct" => ArgRules {
            aliases: &[("memory_id", "id"), ("content", "text"), ("new_text", "text")],
            ..ArgRules::NONE
        },
        "dispatch" => ArgRules {
            aliases: &[("goal", "prompt"), ("task", "prompt"), ("instructions", "prompt"), ("workspace", "cwd"), ("path", "cwd"), ("type", "role"), ("agent", "role")],
            ignore: &["background", "label"],
            see_also: Some("A helper inside this session: agent(type, prompt)."),
            ..ArgRules::NONE
        },
        "list_mcp_resources" => ArgRules {
            aliases: &[("name", "server")],
            ..ArgRules::NONE
        },
        "read_mcp_resource" => ArgRules {
            aliases: &[("url", "uri"), ("resource", "uri"), ("id", "uri")],
            ..ArgRules::NONE
        },
        "handback" => ArgRules {
            aliases: &[("message", "text"), ("report", "text"), ("result", "text"), ("content", "text")],
            ..ArgRules::NONE
        },
        "set_statusline" => ArgRules {
            aliases: &[("cmd", "command")],
            ..ArgRules::NONE
        },
        "create_agent" => ArgRules {
            aliases: &[("system_prompt", "prompt"), ("instructions", "prompt")],
            ..ArgRules::NONE
        },
        _ => return None,
    })
}

/// Whether `name` is an MCP tool (`mcp__server__tool`): third-party schemas get no guessing.
pub fn is_mcp_tool(name: &str) -> bool {
    name.starts_with("mcp__")
}

// ---------------------------------------------------------------- signatures

/// `web_fetch(url, offset?: int, part?: text|references, prompt?, raw?: bool)`: the spec the
/// model was shown, as one line. Plain strings carry no type.
pub fn signature(spec: &ToolSpec) -> String {
    format!("{}({})", spec.name, arg_list(&spec.input_schema))
}

/// `url, offset?: int, part?: text|references`: a schema's arguments, required ones first and
/// bare, the rest in key order (the workspace keeps JSON keys sorted).
pub fn arg_list(schema: &Value) -> String {
    let required: Vec<&str> = schema.get("required").and_then(Value::as_array).map(|r| r.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let Some(props) = schema.get("properties").and_then(Value::as_object) else { return String::new() };
    let first = required.iter().filter_map(|k| props.get_key_value(*k));
    let rest = props.iter().filter(|(k, _)| !required.contains(&k.as_str()));
    first
        .chain(rest)
        .map(|(k, v)| {
            let opt = if required.contains(&k.as_str()) { "" } else { "?" };
            match type_label(v) {
                Some(t) => format!("{k}{opt}: {t}"),
                None => format!("{k}{opt}"),
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The type shown after a key; `None` for a plain string.
fn type_label(p: &Value) -> Option<String> {
    if let Some(e) = p.get("enum").and_then(Value::as_array) {
        let names: Vec<String> = e.iter().map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string)).collect();
        return Some(names.join("|"));
    }
    match schema_type(p) {
        Some("array") => {
            let inner = p.get("items");
            Some(match inner {
                Some(it) if schema_type(it) == Some("object") && it.get("properties").is_some() => format!("[{{{}}}]", arg_list(it)),
                Some(it) => format!("[{}]", type_label(it).unwrap_or_else(|| "string".into())),
                None => "[string]".into(),
            })
        }
        Some("object") if p.get("properties").is_some() => Some(format!("{{{}}}", arg_list(p))),
        Some("integer") => Some("int".into()),
        Some("boolean") => Some("bool".into()),
        Some("number") => Some("num".into()),
        Some("string") | None => None,
        Some(other) => Some(other.to_string()),
    }
}

/// A property's JSON type: `type`, or the first non-null of a type list.
fn schema_type(p: &Value) -> Option<&str> {
    match p.get("type")? {
        Value::String(s) => Some(s.as_str()),
        Value::Array(a) => a.iter().filter_map(Value::as_str).find(|t| *t != "null"),
        _ => None,
    }
}

// ---------------------------------------------------------------- names

/// Lowercase letters and digits only: `WebFetch`, `web-fetch`, `web_fetch` agree.
fn squash(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_lowercase()).collect()
}

/// Where a call to `name`, which the host does not have, should go: `(tool, injected arg)`.
/// `names` are the tools the host lists; `known` is whether the host can run a name at all.
pub fn resolve_name(name: &str, names: &[String], known: &dyn Fn(&str) -> bool) -> Option<(String, Option<(String, Value)>)> {
    if known(name) {
        return None;
    }
    let trimmed = name.trim();
    if trimmed != name && known(trimmed) {
        return Some((trimmed.to_string(), None));
    }
    // `server__tool` and `server.tool` are an MCP tool without its prefix.
    if !is_mcp_tool(trimmed) {
        let mcp = format!("mcp__{}", trimmed.replace('.', "__"));
        if (trimmed.contains("__") || trimmed.contains('.')) && known(&mcp) {
            return Some((mcp, None));
        }
    }
    let key = squash(trimmed);
    if !key.is_empty() {
        let mut hits = names.iter().filter(|n| squash(n) == key);
        if let (Some(one), None) = (hits.next(), hits.next()) {
            return Some((one.clone(), None));
        }
        if let Some(l) = LEGACY.iter().find(|l| squash(l.from) == key && known(l.to)) {
            let inject = l.inject.and_then(|(k, v)| Some((k.to_string(), serde_json::from_str(v).ok()?)));
            return Some((l.to.to_string(), inject));
        }
    }
    None
}

/// A call as the loop parses it, made canonical: the tool name resolved and the arguments a
/// JSON object. Anything it cannot fix is left for the layer and the host to answer.
pub fn prepare_call(host: &dyn ToolHost, call: &mut ToolCall) {
    let known = |n: &str| host.tool_spec(n).is_some() || host.manifest(n).is_some();
    let mut inject = None;
    if !call.name.is_empty() && !known(&call.name) {
        let names: Vec<String> = host.specs().into_iter().map(|s| s.name).collect();
        if let Some((to, inj)) = resolve_name(&call.name, &names, &known) {
            call.name = to;
            inject = inj;
        }
    }
    if let Some(spec) = host.tool_spec(&call.name) {
        if let Ok(obj) = container(&spec, &call.args) {
            call.args = Value::Object(obj);
        }
    }
    if let (Some((k, v)), Some(obj)) = (inject, call.args.as_object_mut()) {
        obj.entry(k).or_insert(v);
    }
}

/// The arguments as a JSON object: null is empty, a JSON string is parsed, a bare string is the
/// one required string argument.
fn container(spec: &ToolSpec, args: &Value) -> Result<Map<String, Value>, String> {
    match args {
        Value::Object(m) => Ok(m.clone()),
        Value::Null => Ok(Map::new()),
        Value::String(s) => {
            if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(s.trim()) {
                return Ok(m);
            }
            let props = spec.input_schema.get("properties").and_then(Value::as_object);
            let required: Vec<&str> =
                spec.input_schema.get("required").and_then(Value::as_array).map(|r| r.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            match (props, required.as_slice()) {
                (Some(p), [only]) if p.get(*only).is_some_and(|v| matches!(schema_type(v), None | Some("string")) && v.get("enum").is_none()) => {
                    let mut m = Map::new();
                    m.insert((*only).to_string(), Value::String(s.clone()));
                    Ok(m)
                }
                _ => Err(format!("the arguments were not a JSON object: {}", clip(s, 80))),
            }
        }
        other => Err(format!("the arguments were not a JSON object: {}", clip(&other.to_string(), 80))),
    }
}

// ---------------------------------------------------------------- normalise

/// What changed in a call that was then run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rewrite {
    pub tool: String,
    /// The key the model sent (or the key whose value was coerced).
    pub from: String,
    /// The schema key it now sits under.
    pub to: String,
    /// A value coerced in place, not a key moved.
    pub coerced: bool,
}

/// A call made to match its schema.
#[derive(Debug, Clone, PartialEq)]
pub struct Normalized {
    pub args: Value,
    pub rewrites: Vec<Rewrite>,
}

/// Why a call cannot run: one short phrase per problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgError {
    pub problems: Vec<String>,
}

/// Normalises `args` against `spec` and `rules`, or says what is wrong.
pub fn normalize(spec: &ToolSpec, rules: &ArgRules, args: &Value) -> Result<Normalized, ArgError> {
    let fail = |m: String| ArgError { problems: vec![m] };
    let mut obj = container(spec, args).map_err(fail)?;
    let Some(props) = spec.input_schema.get("properties").and_then(Value::as_object) else {
        return Ok(Normalized { args: Value::Object(obj), rewrites: Vec::new() });
    };
    let mcp = is_mcp_tool(&spec.name);
    let tool = spec.name.clone();
    let mut rewrites: Vec<Rewrite> = Vec::new();
    let moved = |rw: &mut Vec<Rewrite>, from: &str, to: &str| rw.push(Rewrite { tool: tool.clone(), from: from.to_string(), to: to.to_string(), coerced: false });
    let required: Vec<&str> = spec.input_schema.get("required").and_then(Value::as_array).map(|r| r.iter().filter_map(Value::as_str).collect()).unwrap_or_default();

    for k in rules.strip {
        obj.remove(*k);
    }
    // A null is an argument not given (models fill every optional slot); a third-party schema may
    // mean it, so only the built-ins' nulls go.
    if !mcp {
        obj.retain(|_, v| !v.is_null());
    }

    // Keys: exact, then spelling, then the tool's aliases, then generic synonyms.
    if !mcp {
        let unknown: Vec<String> = obj.keys().filter(|k| !props.contains_key(*k)).cloned().collect();
        for k in unknown {
            let sk = spelling(&k);
            let named = |f: &str| f == k || f == sk;
            let target = spelled(&k, props)
                .or_else(|| rules.aliases.iter().find(|(f, t)| named(f) && props.contains_key(*t)).map(|(_, t)| (*t).to_string()))
                .or_else(|| SYNONYMS.iter().find(|(f, t)| named(f) && props.contains_key(*t)).map(|(_, t)| (*t).to_string()));
            let Some(to) = target else { continue };
            match obj.get(&to) {
                None => {
                    let v = obj.remove(&k).unwrap_or(Value::Null);
                    obj.insert(to.clone(), v);
                    moved(&mut rewrites, &k, &to);
                }
                // The same value twice: the extra key says nothing new.
                Some(have) if obj.get(&k) == Some(have) => {
                    obj.remove(&k);
                }
                Some(_) => {}
            }
        }
        // `op`-folded renames and enum spellings of the tool's own.
        for f in rules.folds.iter().filter(|f| props.contains_key(f.to) && !props.contains_key(f.from)) {
            let on = obj.get(f.when.0).and_then(Value::as_str).is_some_and(|v| v.eq_ignore_ascii_case(f.when.1));
            if on && obj.contains_key(f.from) && (f.to == f.when.0 || !obj.contains_key(f.to)) {
                let v = obj.remove(f.from).unwrap_or(Value::Null);
                obj.insert(f.to.to_string(), v);
                moved(&mut rewrites, f.from, f.to);
            }
        }
        let in_enum = |key: &str, v: &str| props.get(key).and_then(|p| p.get("enum")).and_then(Value::as_array).is_some_and(|e| e.iter().any(|x| x.as_str() == Some(v)));
        for (key, from, to) in rules.values.iter().filter(|(k, _, t)| in_enum(k, t)) {
            if obj.get(*key).and_then(Value::as_str).is_some_and(|v| v.eq_ignore_ascii_case(from)) {
                obj.insert((*key).to_string(), Value::String((*to).to_string()));
                moved(&mut rewrites, key, key);
            }
        }
        // Value sniffing: a URL (or arXiv id) under a key the tool does not have is the `url`.
        if props.contains_key("url") && !obj.contains_key("url") {
            let cands: Vec<String> = obj
                .iter()
                .filter(|(k, v)| !props.contains_key(*k) && v.as_str().is_some_and(|s| looks_like_url(s) || (rules.sniff_arxiv && looks_like_arxiv_id(s))))
                .map(|(k, _)| k.clone())
                .collect();
            if let [one] = cands.as_slice() {
                let v = obj.remove(one).unwrap_or(Value::Null);
                obj.insert("url".into(), v);
                moved(&mut rewrites, one, "url");
            }
        }
        // Keys the tool lists as ignorable go, with a note; they are not strays.
        for k in rules.ignore {
            if !props.contains_key(*k) && obj.remove(*k).is_some() {
                moved(&mut rewrites, k, "");
            }
        }
        // One-to-one rescue: one stray key, one missing required key, and the value fits it.
        let stray: Vec<String> = obj.keys().filter(|k| !props.contains_key(*k) && !is_policy_key(rules, k)).cloned().collect();
        let missing: Vec<&str> = required.iter().copied().filter(|r| !obj.contains_key(*r)).collect();
        if let ([one], [want]) = (stray.as_slice(), missing.as_slice()) {
            let v = &obj[one];
            let url_mismatch = *want == "url" || (*want == "query" && v.as_str().is_some_and(looks_like_url));
            if !url_mismatch && coerce(v, &props[*want], false, false).is_ok() {
                let v = obj.remove(one).unwrap_or(Value::Null);
                obj.insert((*want).to_string(), v);
                moved(&mut rewrites, one, want);
            }
        }
    }

    // Types, lossless only.
    let mut problems: Vec<String> = Vec::new();
    for (k, schema) in props {
        let Some(v) = obj.get(k) else { continue };
        let split = rules.split.contains(&k.as_str());
        match coerce(v, schema, mcp, split) {
            Ok(new) if &new != v => {
                obj.insert(k.clone(), new);
                rewrites.push(Rewrite { tool: spec.name.clone(), from: k.clone(), to: k.clone(), coerced: true });
            }
            Ok(_) => {}
            Err(why) => problems.push(format!("`{k}` {why}")),
        }
    }

    // Required, and the tool's own checks.
    let has = |o: &Map<String, Value>, k: &str| o.get(k).is_some_and(|v| !v.is_null());
    for r in &required {
        if !has(&obj, r) {
            problems.push(format!("missing `{r}`"));
        }
    }
    // A rule that names a key this schema does not have waits for the schema that does.
    for rule in rules.requires.iter().filter(|r| r.keys().iter().all(|k| props.contains_key(*k))) {
        match rule {
            Require::OneOf(keys) => {
                let present = keys.iter().filter(|k| has(&obj, k)).count();
                let list = keys.iter().map(|k| format!("`{k}`")).collect::<Vec<_>>().join("|");
                if present == 0 && !keys.iter().any(|k| required.contains(k)) {
                    problems.push(format!("missing one of {list}"));
                } else if present > 1 {
                    problems.push(format!("send only one of {list}"));
                }
            }
            Require::When { key, is, need, unless } => {
                if obj.get(*key).and_then(Value::as_str).is_some_and(|v| v.eq_ignore_ascii_case(is)) && !unless.iter().any(|u| has(&obj, u)) {
                    for n in *need {
                        if !has(&obj, n) && !problems.iter().any(|p| p == &format!("missing `{n}`")) {
                            problems.push(format!("missing `{n}` for {key} `{is}`"));
                        }
                    }
                }
            }
            Require::WhenAny { key, is, any } => {
                let list = any.iter().map(|k| format!("`{k}`")).collect::<Vec<_>>().join("|");
                if obj.get(*key).and_then(Value::as_str).is_some_and(|v| v.eq_ignore_ascii_case(is)) && !any.iter().any(|k| has(&obj, k)) {
                    problems.push(format!("missing one of {list} for {key} `{is}`"));
                }
            }
            Require::UnlessAny { need, unless } => {
                if !has(&obj, need) && !unless.iter().any(|u| has(&obj, u)) && !problems.iter().any(|p| p == &format!("missing `{need}`")) {
                    problems.push(format!("missing `{need}`"));
                }
            }
        }
    }

    // Unknown keys are an error where the schema is closed: every built-in, and an MCP tool
    // that says so. Never dropped silently.
    let closed = match spec.input_schema.get("additionalProperties") {
        Some(Value::Bool(false)) => true,
        Some(_) => false,
        None => !mcp,
    };
    if closed {
        let url_missing = props.contains_key("url") && !has(&obj, "url");
        for (k, v) in &obj {
            if props.contains_key(k) || is_policy_key(rules, k) {
                continue;
            }
            let note = match v.as_str() {
                Some(s) if url_missing && !looks_like_url(s) => format!(" (\"{}\" is not a URL)", clip(s, 30)),
                _ => String::new(),
            };
            problems.push(format!("unknown `{k}`{note}"));
        }
    }

    if problems.is_empty() {
        Ok(Normalized { args: Value::Object(obj), rewrites })
    } else {
        Err(ArgError { problems })
    }
}

/// Whether `k` is one the tool's policy guard judges (any spelling).
fn is_policy_key(rules: &ArgRules, k: &str) -> bool {
    let k = spelling(k);
    rules.policy_keys.iter().any(|p| *p == k)
}

/// The schema key `k` is another spelling of (`filePath`, `--limit`, `LIMIT`), when exactly one.
fn spelled(k: &str, props: &Map<String, Value>) -> Option<String> {
    let want = spelling(k);
    if want.is_empty() {
        return None;
    }
    let mut hits = props.keys().filter(|p| spelling(p) == want);
    match (hits.next(), hits.next()) {
        (Some(one), None) => Some(one.clone()),
        _ => None,
    }
}

/// snake_case, lowercase, no leading dashes. Grep's `-A` and `-i` are matched literally first.
fn spelling(k: &str) -> String {
    let k = k.trim_start_matches('-');
    let mut out = String::new();
    let mut prev: Option<char> = None;
    for c in k.chars() {
        if c.is_ascii_uppercase() && prev.is_some_and(|p| p.is_ascii_lowercase() || p.is_ascii_digit()) {
            out.push('_');
        }
        out.push(if c == '-' || c == ' ' { '_' } else { c.to_ascii_lowercase() });
        prev = Some(c);
    }
    out
}

fn looks_like_url(s: &str) -> bool {
    let s = s.trim();
    (s.starts_with("http://") || s.starts_with("https://")) && !s.contains(char::is_whitespace)
}

/// `2407.01234`, `2407.01234v2`, `arXiv:2407.01234`, `hep-th/9901001`.
fn looks_like_arxiv_id(s: &str) -> bool {
    let s = s.trim();
    let s = s.strip_prefix("arXiv:").or_else(|| s.strip_prefix("arxiv:")).unwrap_or(s);
    let s = match s.rsplit_once('v') {
        Some((head, v)) if !v.is_empty() && v.chars().all(|c| c.is_ascii_digit()) => head,
        _ => s,
    };
    let digits = |t: &str| t.chars().all(|c| c.is_ascii_digit());
    match (s.split_once('.'), s.split_once('/')) {
        (Some((a, b)), _) => a.len() == 4 && (4..=5).contains(&b.len()) && digits(a) && digits(b),
        (_, Some((a, b))) => !a.is_empty() && a.chars().all(|c| c.is_ascii_lowercase() || c == '-') && b.len() == 7 && digits(b),
        _ => false,
    }
}

fn clip(s: &str, max: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= max {
        s
    } else {
        format!("{}...", s.chars().take(max).collect::<String>())
    }
}

/// `v` as `schema` asks, losslessly, or why not. `any`: an untyped property is any value (MCP),
/// not a string.
fn coerce(v: &Value, schema: &Value, any: bool, split: bool) -> Result<Value, String> {
    let got = |v: &Value| clip(&v.to_string(), 24);
    if v.is_null() {
        return Ok(Value::Null);
    }
    if let Some(options) = schema.get("enum").and_then(Value::as_array) {
        let options: Vec<&str> = options.iter().filter_map(Value::as_str).collect();
        let text = match v {
            Value::String(s) => s.trim().to_string(),
            Value::Number(n) => n.to_string(),
            _ => return Err(format!("must be one of {}", options.join("|"))),
        };
        if options.contains(&text.as_str()) {
            return Ok(Value::String(text));
        }
        let lower = text.to_ascii_lowercase();
        if let Some(o) = options.iter().find(|o| o.to_ascii_lowercase() == lower) {
            return Ok(Value::String((*o).to_string()));
        }
        let by_prefix: Vec<&&str> = options.iter().filter(|o| !lower.is_empty() && o.to_ascii_lowercase().starts_with(&lower)).collect();
        return match by_prefix.as_slice() {
            [one] => Ok(Value::String((**one).to_string())),
            [] => Err(format!("must be one of {} (got {})", options.join("|"), got(v))),
            many => Err(format!("is ambiguous: {} could be {}", got(v), many.iter().map(|o| **o).collect::<Vec<_>>().join("|"))),
        };
    }
    // A union (`anyOf`, a type list) is the tool's to read: only a value that fits none of its
    // types is looked at, and it is coerced to the first that takes it.
    if ["anyOf", "oneOf", "allOf", "$ref"].iter().any(|k| schema.get(*k).is_some()) && schema.get("type").is_none() {
        return Ok(v.clone());
    }
    if let Some(types) = schema.get("type").and_then(Value::as_array).filter(|t| t.iter().filter(|x| x.as_str() != Some("null")).count() > 1) {
        let fits = |t: &str| match (t, v) {
            ("string", Value::String(_)) | ("boolean", Value::Bool(_)) | ("array", Value::Array(_)) | ("object", Value::Object(_)) | ("null", Value::Null) => true,
            ("integer", Value::Number(n)) => n.is_i64() || n.is_u64(),
            ("number", Value::Number(_)) => true,
            _ => false,
        };
        if types.iter().filter_map(Value::as_str).any(fits) {
            return Ok(v.clone());
        }
        let mut last = Err(format!("must be one of {}", types.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("|")));
        for t in types.iter().filter_map(Value::as_str).filter(|t| *t != "null") {
            let one = serde_json::json!({ "type": t });
            match coerce(v, &one, any, split) {
                Ok(done) => return Ok(done),
                Err(e) => last = Err(e),
            }
        }
        return last;
    }
    let ty = match schema_type(schema) {
        Some(t) => t,
        None if any => return Ok(v.clone()),
        None => "string",
    };
    match ty {
        "integer" | "number" => match v {
            Value::Number(n) if ty == "number" => Ok(Value::Number(n.clone())),
            Value::Number(n) => match n.as_i64() {
                Some(_) => Ok(v.clone()),
                None => n.as_f64().filter(|f| f.fract() == 0.0 && f.abs() < 9e15).map(|f| Value::from(f as i64)).ok_or_else(|| format!("must be an integer (got {})", got(v))),
            },
            Value::String(s) => {
                let t = s.trim();
                if let Ok(i) = t.parse::<i64>() {
                    return Ok(Value::from(i));
                }
                match t.parse::<f64>() {
                    Ok(f) if ty == "number" && f.is_finite() => serde_json::Number::from_f64(f).map(Value::Number).ok_or_else(|| "must be a number".to_string()),
                    Ok(f) if f.is_finite() && f.fract() == 0.0 && f.abs() < 9e15 => Ok(Value::from(f as i64)),
                    _ => Err(format!("must be {} (got {})", if ty == "integer" { "an integer" } else { "a number" }, got(v))),
                }
            }
            _ => Err(format!("must be {} (got {})", if ty == "integer" { "an integer" } else { "a number" }, got(v))),
        },
        "boolean" => match v {
            Value::Bool(_) => Ok(v.clone()),
            Value::String(s) if s.trim().eq_ignore_ascii_case("true") => Ok(Value::Bool(true)),
            Value::String(s) if s.trim().eq_ignore_ascii_case("false") => Ok(Value::Bool(false)),
            _ => Err(format!("must be true or false (got {})", got(v))),
        },
        "string" => match v {
            Value::String(_) => Ok(v.clone()),
            Value::Number(n) => Ok(Value::String(n.to_string())),
            _ => Err(format!("must be text (got {})", got(v))),
        },
        "array" => {
            let items = schema.get("items");
            let list: Vec<Value> = match v {
                Value::Array(a) => a.clone(),
                Value::Object(_) => vec![v.clone()],
                Value::String(s) => match serde_json::from_str::<Value>(s.trim()) {
                    Ok(Value::Array(a)) if s.trim_start().starts_with('[') => a,
                    _ if split => crate::commands::split_args(s).into_iter().map(Value::String).collect(),
                    _ => vec![v.clone()],
                },
                other => vec![other.clone()],
            };
            let mut out = Vec::with_capacity(list.len());
            for item in &list {
                out.push(match items {
                    Some(it) => coerce_item(item, it, any)?,
                    None => item.clone(),
                });
            }
            Ok(Value::Array(out))
        }
        "object" => match v {
            Value::Object(_) => Ok(coerce_fields(v, schema, any)),
            Value::String(s) => match serde_json::from_str::<Value>(s.trim()) {
                Ok(parsed @ Value::Object(_)) => Ok(coerce_fields(&parsed, schema, any)),
                _ => Err(format!("must be an object (got {})", got(v))),
            },
            _ => Err(format!("must be an object (got {})", got(v))),
        },
        _ => Ok(v.clone()),
    }
}

/// One array element. A string where objects with a `label` are expected is that label.
fn coerce_item(item: &Value, schema: &Value, any: bool) -> Result<Value, String> {
    if schema_type(schema) == Some("object") {
        if let (Value::String(s), true) = (item, schema.get("properties").and_then(|p| p.get("label")).is_some()) {
            return Ok(serde_json::json!({ "label": s }));
        }
    }
    coerce(item, schema, any, false)
}

/// Coerces the declared properties of an object, leaving what fails or is not declared as it is
/// (a nested mistake is the tool's to report).
fn coerce_fields(v: &Value, schema: &Value, any: bool) -> Value {
    let (Some(obj), Some(props)) = (v.as_object(), schema.get("properties").and_then(Value::as_object)) else { return v.clone() };
    let mut out = obj.clone();
    for (k, s) in props {
        if let Some(have) = out.get(k) {
            if let Ok(new) = coerce(have, s, any, false) {
                out.insert(k.clone(), new);
            }
        }
    }
    Value::Object(out)
}

// ---------------------------------------------------------------- the error text

/// The answer to an invalid call: what is wrong, the exact signature, one example, and a line
/// for known confusions. At most four lines. `repeat`: the same invalid call came before, so the
/// example leads.
pub fn invalid_text(spec: &ToolSpec, rules: &ArgRules, err: &ArgError, repeat: bool) -> String {
    let head = format!("{} was not run: {}.", spec.name, err.problems.join("; "));
    let sig = signature(spec);
    let example = rules.example.map(|e| format!("e.g. {e}"));
    let mut lines: Vec<String> = Vec::new();
    if repeat {
        lines.push(format!("Same invalid call again: {head}"));
        lines.extend(example);
        lines.push(sig);
    } else {
        lines.push(head);
        lines.push(sig);
        lines.extend(example);
    }
    lines.extend(rules.see_also.map(str::to_string));
    lines.join("\n")
}

/// The answer to a call to a tool the host does not have: the nearest tool's signature and the
/// names there are.
pub fn unknown_tool_text(name: &str, specs: &[ToolSpec]) -> String {
    let near = nearest(name, specs);
    let names = specs.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", ");
    match near {
        Some(s) => format!("No tool `{name}`. Did you mean {}?\nTools: {names}", signature(s)),
        None => format!("No tool `{name}`.\nTools: {names}"),
    }
}

fn nearest<'a>(name: &str, specs: &'a [ToolSpec]) -> Option<&'a ToolSpec> {
    let want = squash(name);
    if want.len() < 3 {
        return None;
    }
    let contains = specs.iter().filter(|s| {
        let have = squash(&s.name);
        have.len() >= 3 && (have.contains(&want) || want.contains(&have))
    });
    let mut contains = contains;
    if let (Some(one), None) = (contains.next(), contains.next()) {
        return Some(one);
    }
    let limit = (want.len() / 3).max(2);
    specs.iter().map(|s| (edit_distance(&want, &squash(&s.name)), s)).filter(|(d, _)| *d <= limit).min_by_key(|(d, _)| *d).map(|(_, s)| s)
}

fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            cur.push((prev[j] + usize::from(ca != cb)).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

// ---------------------------------------------------------------- the middleware

/// Every rewrite the layer made in a session, for `/context` and the probe report. Kept in the
/// conversation's extensions; the oldest go first past [`MAX_REWRITES`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArgRewrites(pub Vec<Rewrite>);

/// Rewrites kept per session.
pub const MAX_REWRITES: usize = 200;

/// The first `before_approval` of every agent: unknown tools, key and type fixes, and the exact
/// signature for what cannot be fixed. Reads the spec the model was shown from the host.
pub struct ArgsLayer {
    host: Arc<dyn ToolHost>,
    /// The last invalid call, to tell a second identical one.
    last_invalid: Mutex<Option<(String, String)>>,
    /// `(tool, key)` pairs whose rename was already told to the model.
    told: Mutex<HashSet<(String, String)>>,
    /// Footers waiting for their call's result, by call id.
    footers: Mutex<HashMap<String, String>>,
}

impl ArgsLayer {
    pub fn new(host: Arc<dyn ToolHost>) -> Self {
        Self { host, last_invalid: Mutex::new(None), told: Mutex::new(HashSet::new()), footers: Mutex::new(HashMap::new()) }
    }

    fn refuse(text: String, summary: &str) -> CallFlow {
        CallFlow::Done(ToolOutcome { summary: summary.into(), ..ToolOutcome::error(text) })
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl ToolMiddleware for ArgsLayer {
    fn name(&self) -> &str {
        "args"
    }

    fn before_approval(&self, call: &mut ToolCall, cx: &CallCtx) -> CallFlow {
        let Some(spec) = self.host.tool_spec(&call.name) else {
            // Not a tool the model was shown. A tool the host still runs (one it holds back, a
            // wrapper's own) answers for itself; a name nobody has is answered here.
            if call.name.trim().is_empty() || self.host.manifest(&call.name).is_some() {
                return CallFlow::Continue;
            }
            let specs = self.host.specs();
            if specs.is_empty() {
                return CallFlow::Continue;
            }
            return Self::refuse(unknown_tool_text(&call.name, &specs), "no such tool");
        };
        let rules = self.host.arg_rules(&call.name).unwrap_or_default();
        if rules.skip {
            return CallFlow::Continue;
        }
        match normalize(&spec, &rules, &call.args) {
            Ok(done) => {
                call.args = done.args;
                if !done.rewrites.is_empty() {
                    self.note(&call.id, &spec.name, &done.rewrites, cx);
                }
                *lock(&self.last_invalid) = None;
                CallFlow::Continue
            }
            Err(err) => {
                let key = (call.name.clone(), call.args.to_string());
                let repeat = lock(&self.last_invalid).replace(key.clone()).is_some_and(|last| last == key);
                Self::refuse(invalid_text(&spec, &rules, &err, repeat), "invalid arguments")
            }
        }
    }

    fn after_call(&self, call: &ToolCall, _cx: &CallCtx, outcome: &mut ToolOutcome) {
        if let Some(footer) = lock(&self.footers).remove(&call.id) {
            outcome.content.push_str("\n\n");
            outcome.content.push_str(&footer);
        }
    }
}

impl ArgsLayer {
    /// Logs the rewrites and, once per `(tool, key)`, queues the footer that teaches the real name.
    fn note(&self, call_id: &str, tool: &str, rewrites: &[Rewrite], cx: &CallCtx) {
        cx.scope.with_ext(|e| {
            let log = e.get_or_default::<ArgRewrites>();
            log.0.extend(rewrites.iter().cloned());
            let over = log.0.len().saturating_sub(MAX_REWRITES);
            log.0.drain(..over);
        });
        let mut told = lock(&self.told);
        let lines: Vec<String> = rewrites
            .iter()
            .filter(|r| !r.coerced && r.from != r.to)
            .filter(|r| told.insert((tool.to_string(), r.from.clone())))
            .map(|r| {
                if r.to.is_empty() {
                    format!("ignored `{}`: not a parameter of {tool}", r.from)
                } else {
                    format!("read `{}` as `{}`; the parameter is `{}`", r.from, r.to, r.to)
                }
            })
            .collect();
        if !lines.is_empty() {
            lock(&self.footers).insert(call_id.to_string(), format!("[args: {}]", lines.join("; ")));
        }
    }
}

#[cfg(test)]
mod tests;
