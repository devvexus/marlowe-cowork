//! The tool-call suite (gate A4): each prompt goes to the model with the session's real tool
//! specs; the answer is scored for naming the right tool, carrying the right values, and
//! arguments that satisfy the tool's schema. The repair layer inside the providers counts what
//! it had to fix ([`marlowe_provider::ollama::repair::RepairStats`]); the suite reports that
//! delta, which is the number A4 states ("valid after repair", "needing repair").

use std::collections::BTreeMap;
use std::time::Instant;

use marlowe_model::{CancelToken, ChatRequest, Message, Provider, SystemBlock, ToolSpec};
use marlowe_provider::ollama::repair::{RepairSnapshot, RepairStats};
use serde::Serialize;
use serde_json::Value;

use super::prompts::Prompt;

/// Consecutive provider errors that end the suite (a stopped server would otherwise burn the
/// whole list on timeouts).
const MAX_CONSECUTIVE_ERRORS: usize = 5;

const SYSTEM: &str = "You are a coding agent working in a repository. Do what the user asks by calling \
exactly one of the provided tools with correct arguments. Do not answer in prose when a tool fits.";

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct ToolsReport {
    pub prompts: usize,
    /// Prompts that got any tool call.
    pub called: usize,
    /// The first call named the expected tool.
    pub selected: usize,
    /// ... and its arguments carried every expected value.
    pub args_ok: usize,
    /// The first call's arguments satisfy the tool's schema.
    pub schema_valid: usize,
    pub provider_errors: usize,
    pub aborted: bool,
    /// Calls the providers counted (the repair layer's own denominator).
    pub repair_calls: u64,
    /// Share of ALL prompts whose first call ran with valid arguments after repair; a prompt
    /// answered in prose, with an unknown tool, or lost to an error counts as not valid (A4).
    pub valid_rate: f64,
    /// Share of all prompts whose valid call needed repair.
    pub repair_rate: f64,
    /// Share of the calls the provider counted that came out valid (the per-call view).
    pub call_valid_rate: f64,
    pub fixes: BTreeMap<String, u64>,
    pub mean_ms: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Per expected tool: (prompts, right tool and values).
    pub by_tool: BTreeMap<String, (usize, usize)>,
    /// Up to ten misses, for the report.
    pub misses: Vec<String>,
}

impl ToolsReport {
    pub fn selection_rate(&self) -> f64 {
        ratio(self.selected, self.prompts)
    }

    pub fn args_rate(&self) -> f64 {
        ratio(self.args_ok, self.prompts)
    }

    /// Share of prompts that got any tool call.
    pub fn call_rate(&self) -> f64 {
        ratio(self.called, self.prompts)
    }

    /// Gate A4 for a local model (98% of the prompts valid after repair, at most 5% needing
    /// repair) or a frontier one (99.5% valid). Both rates are over all prompts, so a model that
    /// answers most of them in prose fails.
    pub fn meets_a4(&self, local: bool) -> bool {
        self.prompts > 0 && !self.aborted && if local { self.valid_rate >= 0.98 && self.repair_rate <= 0.05 } else { self.valid_rate >= 0.995 }
    }
}

fn ratio(n: usize, d: usize) -> f64 {
    if d == 0 {
        0.0
    } else {
        n as f64 / d as f64
    }
}

/// Whether `args` satisfies `spec`'s schema: an object, every required key present, no key the
/// schema does not list (the schemas are closed), and each listed value of its declared type.
pub fn satisfies_schema(spec: &ToolSpec, args: &Value) -> bool {
    let Some(obj) = args.as_object() else { return false };
    let schema = &spec.input_schema;
    let required = schema.get("required").and_then(Value::as_array).cloned().unwrap_or_default();
    if required.iter().filter_map(Value::as_str).any(|k| !obj.contains_key(k)) {
        return false;
    }
    let Some(props) = schema.get("properties").and_then(Value::as_object) else { return true };
    obj.iter().all(|(k, v)| match props.get(k) {
        None => schema.get("additionalProperties") != Some(&Value::Bool(false)),
        Some(p) => type_ok(p.get("type"), v),
    })
}

fn type_ok(declared: Option<&Value>, v: &Value) -> bool {
    let one = |t: &str| match t {
        "string" => v.is_string(),
        "integer" => v.is_i64() || v.is_u64(),
        "number" => v.is_number(),
        "boolean" => v.is_boolean(),
        "array" => v.is_array(),
        "object" => v.is_object(),
        "null" => v.is_null(),
        _ => true,
    };
    match declared {
        None => true,
        Some(Value::String(t)) => one(t),
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).any(one),
        Some(_) => true,
    }
}

/// Whether the call's arguments contain every expected string (case-insensitively).
pub fn carries(args: &Value, expect: &[String]) -> bool {
    let hay = args.to_string().to_lowercase();
    // JSON escapes quotes and backslashes in strings; compare against the unescaped values too.
    let flat = flatten(args).to_lowercase();
    expect.iter().map(|e| e.to_lowercase()).all(|e| hay.contains(&e) || flat.contains(&e))
}

fn flatten(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(flatten).collect::<Vec<_>>().join("\n"),
        Value::Object(m) => m.values().map(flatten).collect::<Vec<_>>().join("\n"),
        other => other.to_string(),
    }
}

fn delta(after: &RepairSnapshot, before: &RepairSnapshot) -> RepairSnapshot {
    let d = |a: u64, b: u64| a.saturating_sub(b);
    RepairSnapshot {
        calls: d(after.calls, before.calls),
        clean: d(after.clean, before.clean),
        repaired: d(after.repaired, before.repaired),
        from_text: d(after.from_text, before.from_text),
        unusable: d(after.unusable, before.unusable),
        truncated: d(after.truncated, before.truncated),
        invalid: d(after.invalid, before.invalid),
        unknown_tool: d(after.unknown_tool, before.unknown_tool),
        fixes: after
            .fixes
            .iter()
            .filter_map(|(k, v)| {
                let n = v.saturating_sub(before.fixes.get(k).copied().unwrap_or(0));
                (n > 0).then(|| (k.clone(), n))
            })
            .collect(),
    }
}

/// Runs `prompts` one at a time. `stats` is the repair counter the provider reports into, when
/// it has one; without it the suite counts a call valid when its arguments satisfy the schema
/// and none as repaired.
pub fn run(
    provider: &dyn Provider,
    model_id: &str,
    specs: &[ToolSpec],
    prompts: &[Prompt],
    stats: Option<&RepairStats>,
    cancel: &CancelToken,
    mut progress: impl FnMut(usize, usize),
) -> ToolsReport {
    let before = stats.map(RepairStats::snapshot).unwrap_or_default();
    let mut r = ToolsReport { prompts: prompts.len(), ..ToolsReport::default() };
    let (mut total_ms, mut answered, mut streak) = (0.0, 0usize, 0usize);
    // Prompts whose first call was valid after repair, and how many of those needed repair.
    let (mut valid_prompts, mut repaired_prompts) = (0usize, 0usize);
    for (i, p) in prompts.iter().enumerate() {
        if cancel.is_cancelled() {
            r.aborted = true;
            break;
        }
        progress(i, prompts.len());
        let mut req = ChatRequest::new(model_id, vec![Message::user(p.text.clone())]);
        req.system = vec![SystemBlock { text: SYSTEM.into(), cache: false }];
        req.tools = specs.to_vec();
        req.max_output_tokens = Some(2048);
        let pre = stats.map(RepairStats::snapshot).unwrap_or_default();
        let started = Instant::now();
        let done = match provider.stream(&req, cancel, &mut |_| {}) {
            Ok(d) => d,
            Err(e) => {
                r.provider_errors += 1;
                streak += 1;
                if r.misses.len() < 10 {
                    r.misses.push(format!("{}: provider error: {e}", p.text));
                }
                if streak >= MAX_CONSECUTIVE_ERRORS {
                    r.aborted = true;
                    break;
                }
                continue;
            }
        };
        streak = 0;
        total_ms += started.elapsed().as_secs_f64() * 1000.0;
        answered += 1;
        r.input_tokens += done.usage.input;
        r.output_tokens += done.usage.output;
        let row = r.by_tool.entry(p.tool.to_string()).or_default();
        row.0 += 1;
        let Some(call) = done.tool_calls.first() else {
            if r.misses.len() < 10 {
                r.misses.push(format!("{}: no tool call", p.text));
            }
            continue;
        };
        r.called += 1;
        let spec = specs.iter().find(|s| s.name == call.name);
        let valid = spec.is_some_and(|s| satisfies_schema(s, &call.args));
        // The provider's repair layer judges this prompt's calls when it counted any; otherwise
        // (a provider with no counter) the schema check does, and nothing was repaired.
        match stats.map(|s| delta(&s.snapshot(), &pre)).filter(|d| d.calls > 0) {
            Some(d) => {
                valid_prompts += usize::from(d.clean + d.repaired + d.from_text >= d.calls);
                repaired_prompts += usize::from(d.clean + d.repaired + d.from_text >= d.calls && d.repaired + d.from_text > 0);
            }
            None => valid_prompts += usize::from(valid),
        }
        r.schema_valid += usize::from(valid);
        let right_tool = call.name == p.tool;
        let right_args = right_tool && carries(&call.args, &p.expect);
        r.selected += usize::from(right_tool);
        r.args_ok += usize::from(right_args);
        if right_args {
            if let Some(row) = r.by_tool.get_mut(p.tool) {
                row.1 += 1;
            }
        } else if r.misses.len() < 10 {
            r.misses.push(format!("{}: expected {} {:?}, got {} {}", p.text, p.tool, p.expect, call.name, call.args));
        }
    }
    r.mean_ms = if answered == 0 { 0.0 } else { total_ms / answered as f64 };
    r.valid_rate = ratio(valid_prompts, r.prompts);
    r.repair_rate = ratio(repaired_prompts, r.prompts);
    match stats.map(|s| delta(&s.snapshot(), &before)).filter(|d| d.calls > 0) {
        Some(d) => {
            r.repair_calls = d.calls;
            r.call_valid_rate = d.valid_rate();
            r.fixes = d.fixes;
        }
        None => {
            r.repair_calls = r.called as u64;
            r.call_valid_rate = ratio(valid_prompts, r.called);
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::fake::{Fake, Reply};
    use crate::bench::prompts::tool_prompts;
    use serde_json::json;

    fn specs() -> Vec<ToolSpec> {
        let spec = |name: &str, props: Value, req: &[&str]| ToolSpec {
            name: name.into(),
            description: name.into(),
            input_schema: json!({"type": "object", "properties": props, "required": req, "additionalProperties": false}),
        };
        vec![
            spec("read", json!({"path": {"type": "string"}, "limit": {"type": "integer"}}), &["path"]),
            spec("bash", json!({"command": {"type": "string"}}), &["command"]),
            spec("grep", json!({"pattern": {"type": "string"}}), &["pattern"]),
        ]
    }

    fn names() -> Vec<String> {
        vec!["read".into(), "bash".into(), "grep".into()]
    }

    #[test]
    fn the_schema_check_wants_required_keys_types_and_no_strays() {
        let s = &specs()[0];
        assert!(satisfies_schema(s, &json!({"path": "a.rs"})));
        assert!(satisfies_schema(s, &json!({"path": "a.rs", "limit": 5})));
        assert!(!satisfies_schema(s, &json!({"limit": 5})), "required key missing");
        assert!(!satisfies_schema(s, &json!({"path": "a.rs", "limit": "5"})), "wrong type");
        assert!(!satisfies_schema(s, &json!({"path": "a.rs", "extra": 1})), "closed schema");
        assert!(!satisfies_schema(s, &json!("path")), "not an object");
    }

    #[test]
    fn values_are_found_in_nested_and_escaped_arguments() {
        assert!(carries(&json!({"command": "echo \"hi\" | wc"}), &["echo \"hi\"".into()]));
        assert!(carries(&json!({"edits": [{"old_string": "A b"}]}), &["a B".into()]));
        assert!(!carries(&json!({"path": "x"}), &["y".into()]));
    }

    #[test]
    fn a_faithful_model_scores_every_prompt_and_a_sloppy_one_does_not() {
        let prompts = tool_prompts(&names(), 30);
        assert_eq!(prompts.len(), 30);
        // The oracle answers each prompt with the right tool and its expected values.
        let oracle = Fake::new(|req| {
            let text = crate::bench::fake::last_user_text(req);
            let p = tool_prompts(&names(), 30).into_iter().find(|p| p.text == text).expect("a known prompt");
            let key = match p.tool {
                "read" => "path",
                "bash" => "command",
                _ => "pattern",
            };
            Reply::call(p.tool, json!({ key: p.expect.join(" ") }))
        });
        let r = run(&oracle, "m", &specs(), &prompts, None, &CancelToken::new(), |_, _| {});
        assert_eq!((r.prompts, r.called, r.selected, r.args_ok, r.schema_valid), (30, 30, 30, 30, 30));
        assert_eq!(r.valid_rate, 1.0);
        assert!(r.meets_a4(true) && r.meets_a4(false));
        assert_eq!(r.by_tool.values().map(|v| v.0).sum::<usize>(), 30);

        // Every third prompt: wrong tool; every fifth: prose; every seventh: bad arguments.
        let n = std::sync::atomic::AtomicUsize::new(0);
        let sloppy = Fake::new(move |req| {
            let k = n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let text = crate::bench::fake::last_user_text(req);
            let p = tool_prompts(&names(), 30).into_iter().find(|p| p.text == text).unwrap();
            let key = match p.tool {
                "read" => "path",
                "bash" => "command",
                _ => "pattern",
            };
            if k % 5 == 0 {
                Reply::text("I would look at the file.")
            } else if k % 3 == 0 {
                Reply::call("bash", json!({"command": "ls"}))
            } else if k % 7 == 0 {
                Reply::call(p.tool, json!({"nope": 1}))
            } else {
                Reply::call(p.tool, json!({ key: p.expect.join(" ") }))
            }
        });
        let r = run(&sloppy, "m", &specs(), &prompts, None, &CancelToken::new(), |_, _| {});
        assert!(r.called < 30 && r.args_ok < r.called, "{r:?}");
        assert!(r.valid_rate < 1.0 && !r.meets_a4(true));
        assert!(!r.misses.is_empty() && r.misses.len() <= 10);
    }

    #[test]
    fn repair_counts_come_from_the_providers_stats_when_they_have_any() {
        let stats = std::sync::Arc::new(RepairStats::new());
        let s2 = stats.clone();
        let fake = Fake::new(move |_| {
            // Two clean calls and one the repair layer had to fix, as the provider would record.
            s2.record(&marlowe_provider::ollama::repair::ArgsOutcome::Clean(json!({"command": "ls"})), false);
            Reply::call("bash", json!({"command": "ls"}))
        });
        let prompts = tool_prompts(&["bash".to_string()], 3);
        let r = run(&fake, "m", &specs(), &prompts, Some(&stats), &CancelToken::new(), |_, _| {});
        assert_eq!(r.repair_calls, 3);
        assert_eq!(r.valid_rate, 1.0);
        assert_eq!(r.repair_rate, 0.0);
    }

    #[test]
    fn a_model_that_answers_in_prose_fails_a4_however_clean_its_few_calls_are() {
        use marlowe_provider::ollama::repair::ArgsOutcome;
        // 150 of 200 prompts get prose; the 50 calls are all clean, as the provider records them.
        let every_tool: Vec<String> =
            ["read", "grep", "bash", "edit", "glob", "write", "todo", "web_fetch", "multi_edit", "python", "ask_user", "arxiv"].iter().map(|s| s.to_string()).collect();
        let prompts = tool_prompts(&every_tool, 200);
        assert_eq!(prompts.len(), 200);
        for counted in [true, false] {
            let stats = std::sync::Arc::new(RepairStats::new());
            let s2 = stats.clone();
            let n = std::sync::atomic::AtomicUsize::new(0);
            let fake = Fake::new(move |_| {
                if n.fetch_add(1, std::sync::atomic::Ordering::SeqCst) % 4 == 0 {
                    s2.record(&ArgsOutcome::Clean(json!({"command": "ls"})), false);
                    Reply::call("bash", json!({"command": "ls"}))
                } else {
                    Reply::text("Sure, I would run ls.")
                }
            });
            let r = run(&fake, "m", &specs(), &prompts, counted.then_some(&*stats), &CancelToken::new(), |_, _| {});
            assert_eq!((r.prompts, r.called), (200, 50));
            assert_eq!(r.call_rate(), 0.25);
            assert!((r.valid_rate - 0.25).abs() < 1e-9, "valid over all prompts, not over calls: {r:?}");
            assert!(!r.meets_a4(true) && !r.meets_a4(false), "{r:?}");
        }
    }

    #[test]
    fn repair_is_counted_per_prompt_and_an_unknown_tool_is_not_valid() {
        use marlowe_provider::ollama::repair::{ArgsOutcome, Fix};
        let stats = std::sync::Arc::new(RepairStats::new());
        let s2 = stats.clone();
        let n = std::sync::atomic::AtomicUsize::new(0);
        let fake = Fake::new(move |_| match n.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
            0 => {
                s2.record(&ArgsOutcome::Clean(json!({"command": "ls"})), false);
                Reply::call("bash", json!({"command": "ls"}))
            }
            1 => {
                s2.record(&ArgsOutcome::Repaired(json!({"command": "ls"}), vec![Fix::SingleQuotes]), false);
                Reply::call("bash", json!({"command": "ls"}))
            }
            _ => {
                s2.record_unknown_tool();
                Reply::call("nope", json!({}))
            }
        });
        let prompts = tool_prompts(&["bash".to_string()], 3);
        let r = run(&fake, "m", &specs(), &prompts, Some(&stats), &CancelToken::new(), |_, _| {});
        assert_eq!(r.repair_calls, 3);
        assert!((r.valid_rate - 2.0 / 3.0).abs() < 1e-9, "{r:?}");
        assert!((r.repair_rate - 1.0 / 3.0).abs() < 1e-9, "{r:?}");
        assert!(!r.meets_a4(true));
    }

    #[test]
    fn a_dead_server_ends_the_suite_early() {
        let dead = Fake::failing();
        let prompts = tool_prompts(&names(), 50);
        let r = run(&dead, "m", &specs(), &prompts, None, &CancelToken::new(), |_, _| {});
        assert!(r.aborted);
        assert_eq!(r.provider_errors, MAX_CONSECUTIVE_ERRORS);
        assert_eq!(r.called, 0);
    }
}
