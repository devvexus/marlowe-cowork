//! Debug builds only: a scripted provider for end-to-end runs of the real binary without a model.
//!
//! `MARLOWE_SCRIPTED_PROVIDER=<script.json>` replaces the Ollama and OpenRouter providers with one
//! that answers from rules, and the daemon then uses an in-memory credential store, so no request
//! can leave the machine. Release builds do not compile this module, so the variable does nothing
//! there.
//!
//! Script: `{"log": "<requests.jsonl>"?, "rules": [Rule, ..]}`. Each request is answered by the
//! first rule that matches and has uses left (see `Rule`). A request that no rule matches gets a
//! short "no rule matched" reply. Every request is appended to `log` as one JSON line.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use marlowe_brains::ProviderFactory;
use marlowe_model::{
    CancelToken, ChatRequest, Completion, CompletionBuilder, ModelInfo, Part, Provider, ProviderError,
    ProviderKind, StopReason, StreamEvent, Usage,
};
use serde::Deserialize;
use serde_json::Value;

pub const ENV: &str = "MARLOWE_SCRIPTED_PROVIDER";

/// The scripted model's id in both providers' catalogs.
const MODEL_ID: &str = "scripted";

#[derive(Debug, Deserialize)]
struct Script {
    #[serde(default)]
    log: Option<PathBuf>,
    rules: Vec<Rule>,
}

/// One answer. Matching: every `system` substring is in the system prompt, no `not_system`
/// substring is, every `last` substring is in the last message (its text, tool calls and tool
/// results), and `tool_results`, when set, equals the number of tool results in the conversation.
#[derive(Debug, Deserialize)]
struct Rule {
    name: String,
    #[serde(default)]
    system: Vec<String>,
    #[serde(default)]
    not_system: Vec<String>,
    #[serde(default)]
    last: Vec<String>,
    #[serde(default)]
    tool_results: Option<usize>,
    /// Uses before the rule stops matching; unlimited when absent.
    #[serde(default)]
    times: Option<u32>,
    /// Wait before answering (cancel-aware), to hold a run mid-step.
    #[serde(default)]
    delay_ms: u64,
    #[serde(default)]
    reply: String,
    #[serde(default)]
    calls: Vec<Call>,
}

#[derive(Debug, Deserialize)]
struct Call {
    name: String,
    #[serde(default)]
    args: Value,
}

struct Scripted {
    kind: ProviderKind,
    script: Arc<Script>,
    used: Arc<Mutex<Vec<u32>>>,
}

/// True when the variable is set (debug builds only).
pub fn active() -> bool {
    std::env::var_os(ENV).is_some()
}

/// Registers the scripted provider for Ollama and OpenRouter when the variable is set. A script
/// that will not load makes every model call fail with the reason.
pub fn install(f: &mut ProviderFactory) {
    let Some(path) = std::env::var_os(ENV).map(PathBuf::from) else { return };
    let loaded = std::fs::read_to_string(&path)
        .map_err(|e| e.to_string())
        .and_then(|t| serde_json::from_str::<Script>(&t).map_err(|e| e.to_string()));
    let script = match loaded {
        Ok(s) => Arc::new(s),
        Err(e) => {
            let msg = format!("scripted provider: cannot load {}: {e}", path.display());
            eprintln!("marlowe: {msg}");
            for kind in [ProviderKind::Ollama, ProviderKind::OpenRouter] {
                let msg = msg.clone();
                f.register(kind, move |_| Err(ProviderError::bad_request(msg.clone())));
            }
            return;
        }
    };
    eprintln!("marlowe: scripted provider from {} ({} rules)", path.display(), script.rules.len());
    let used = Arc::new(Mutex::new(vec![0; script.rules.len()]));
    for kind in [ProviderKind::Ollama, ProviderKind::OpenRouter] {
        let p: Arc<dyn Provider> = Arc::new(Scripted { kind, script: script.clone(), used: used.clone() });
        f.register(kind, move |_| Ok(p.clone()));
    }
}

/// The last message as matchable text: text, then `name args` per call, then each result.
fn render_last(req: &ChatRequest) -> String {
    let Some(m) = req.messages.last() else { return String::new() };
    let mut out = String::new();
    for p in &m.content {
        match p {
            Part::Text { text } => out.push_str(text),
            Part::ToolCall(c) => out.push_str(&format!("{} {}", c.name, c.args)),
            Part::ToolResult(r) => out.push_str(&r.content),
            Part::Thinking(_) | Part::Image(_) => {}
        }
        out.push('\n');
    }
    out
}

fn tool_result_count(req: &ChatRequest) -> usize {
    req.messages.iter().map(|m| m.tool_results_iter().count()).sum()
}

impl Scripted {
    fn pick(&self, system: &str, last: &str, results: usize) -> Option<usize> {
        let mut used = self.used.lock().unwrap_or_else(|e| e.into_inner());
        let hit = self.script.rules.iter().enumerate().position(|(i, r)| {
            r.times.is_none_or(|t| used[i] < t)
                && r.system.iter().all(|s| system.contains(s.as_str()))
                && !r.not_system.iter().any(|s| system.contains(s.as_str()))
                && r.last.iter().all(|s| last.contains(s.as_str()))
                && r.tool_results.is_none_or(|n| n == results)
        })?;
        used[hit] += 1;
        Some(hit)
    }

    fn log(&self, rule: Option<&str>, system: &str, last: &str, results: usize) {
        let Some(path) = &self.script.log else { return };
        // The first line of a role card ("You are the crew's …") names the caller.
        let who = system
            .lines()
            .find(|l| l.starts_with("You are the crew's") || l.starts_with("You are a scout"))
            .map(|l| l.chars().take(60).collect::<String>())
            .unwrap_or_else(|| "conductor".into());
        let line = serde_json::json!({
            "at_ms": chrono::Utc::now().timestamp_millis(),
            "pid": std::process::id(),
            "kind": format!("{:?}", self.kind),
            "who": who,
            "rule": rule,
            "tool_results": results,
            "last": last.chars().take(4000).collect::<String>(),
        });
        // One write per line, so parallel steps never interleave inside a line.
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = f.write_all(format!("{line}\n").as_bytes());
        }
    }
}

impl Provider for Scripted {
    fn kind(&self) -> ProviderKind {
        self.kind
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let mut m = ModelInfo::bare(self.kind, MODEL_ID);
        m.supports_tools = true;
        m.context_window = Some(65_536);
        Ok(vec![m])
    }

    fn stream(&self, req: &ChatRequest, cancel: &CancelToken, on: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        let system = req.system_text();
        let last = render_last(req);
        let results = tool_result_count(req);
        let hit = self.pick(&system, &last, results);
        let rule = hit.map(|i| &self.script.rules[i]);
        self.log(rule.map(|r| r.name.as_str()), &system, &last, results);

        if let Some(r) = rule {
            let end = Instant::now() + Duration::from_millis(r.delay_ms);
            while Instant::now() < end {
                if cancel.is_cancelled() {
                    return Err(ProviderError::cancelled());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        if cancel.is_cancelled() {
            return Err(ProviderError::cancelled());
        }

        let mut events = Vec::new();
        let text = rule.map(|r| r.reply.clone()).unwrap_or_else(|| "(scripted: no rule matched)\nConfidence: low".into());
        if !text.is_empty() {
            events.push(StreamEvent::TextDelta(text));
        }
        let calls = rule.map(|r| r.calls.as_slice()).unwrap_or_default();
        for (i, c) in calls.iter().enumerate() {
            let index = i as u32;
            events.push(StreamEvent::ToolCallStart { index, id: format!("call_{}_{i}", chrono::Utc::now().timestamp_micros()), name: c.name.clone() });
            events.push(StreamEvent::ToolArgsDelta { index, json: c.args.to_string() });
            events.push(StreamEvent::ToolCallEnd { index });
        }
        events.push(StreamEvent::Usage(Usage { input: 120, output: 40, ..Default::default() }));
        events.push(StreamEvent::Stop(if calls.is_empty() { StopReason::EndTurn } else { StopReason::ToolUse }));

        let mut b = CompletionBuilder::new();
        for e in events {
            b.push(&e);
            on(e);
        }
        Ok(b.finish())
    }
}
