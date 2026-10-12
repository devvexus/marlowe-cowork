//! A scripted [`AgentHost`] for tests and conformance fixtures: a function decides each call's
//! answer and how long it takes; every start, finish and cancel is recorded with its time, so
//! tests can assert overlap, order and counts without a model.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use serde_json::{json, Map, Value};

use crate::host::{AgentCall, AgentHost, AgentResult, Completion};

/// One call's scripted answer.
#[derive(Debug, Clone)]
pub struct Scripted {
    pub result: AgentResult,
    pub delay: Duration,
}

impl Scripted {
    pub fn now(result: AgentResult) -> Self {
        Scripted { result, delay: Duration::ZERO }
    }

    pub fn after(ms: u64, result: AgentResult) -> Self {
        Scripted { result, delay: Duration::from_millis(ms) }
    }
}

/// What the host saw of one call.
#[derive(Debug, Clone)]
pub struct HostCall {
    pub call: AgentCall,
    pub started: Instant,
    pub finished: Option<Instant>,
    pub cancelled: bool,
}

type Respond = dyn Fn(&AgentCall) -> Scripted + Send + Sync;

/// Answers calls with a function, each on its own thread.
pub struct ScriptedHost {
    respond: Box<Respond>,
    calls: Arc<Mutex<Vec<HostCall>>>,
    cancelled: Arc<Mutex<HashSet<u32>>>,
}

impl ScriptedHost {
    pub fn new(respond: impl Fn(&AgentCall) -> Scripted + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(ScriptedHost { respond: Box::new(respond), calls: Arc::default(), cancelled: Arc::default() })
    }

    /// Echoes `done: <prompt>`; a schema call gets the smallest object that satisfies it.
    pub fn echo() -> Arc<Self> {
        Self::new(|c| Scripted::now(default_answer(c)))
    }

    /// Every call the host started, in start order.
    pub fn calls(&self) -> Vec<HostCall> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Prompts of every started call, in start order.
    pub fn prompts(&self) -> Vec<String> {
        self.calls().into_iter().map(|c| c.call.prompt).collect()
    }
}

/// `done: <prompt>`, or [`sample`] of the schema.
pub fn default_answer(c: &AgentCall) -> AgentResult {
    match &c.opts.schema {
        Some(s) => AgentResult::object(sample(s), 10),
        None => AgentResult::text(format!("done: {}", c.prompt), 10),
    }
}

/// The smallest value that satisfies a schema of the supported subset (first enum/const value,
/// every required property, minimum lengths and bounds).
pub fn sample(schema: &Value) -> Value {
    let Some(o) = schema.as_object() else { return Value::Null };
    if let Some(c) = o.get("const") {
        return c.clone();
    }
    if let Some(Value::Array(e)) = o.get("enum") {
        return e.first().cloned().unwrap_or(Value::Null);
    }
    for k in ["anyOf", "oneOf"] {
        if let Some(Value::Array(a)) = o.get(k) {
            if let Some(first) = a.first() {
                return sample(first);
            }
        }
    }
    let t = match o.get("type") {
        Some(Value::String(s)) => s.as_str(),
        Some(Value::Array(a)) => a.first().and_then(Value::as_str).unwrap_or("null"),
        _ => if o.contains_key("properties") { "object" } else { "null" },
    };
    match t {
        "object" => {
            let mut m = Map::new();
            let props = o.get("properties").and_then(Value::as_object);
            for r in o.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                let s = props.and_then(|p| p.get(r)).cloned().unwrap_or(json!({}));
                m.insert(r.to_string(), sample(&s));
            }
            Value::Object(m)
        }
        "array" => {
            let n = o.get("minItems").and_then(Value::as_u64).unwrap_or(0) as usize;
            let item = o.get("items").cloned().unwrap_or(json!({}));
            Value::Array((0..n).map(|_| sample(&item)).collect())
        }
        "string" => {
            let n = o.get("minLength").and_then(Value::as_u64).unwrap_or(1) as usize;
            Value::String("x".repeat(n.max(1)))
        }
        "integer" | "number" => {
            let lo = o.get("minimum").and_then(Value::as_i64).unwrap_or(0);
            json!(lo)
        }
        "boolean" => Value::Bool(false),
        _ => Value::Null,
    }
}

impl AgentHost for ScriptedHost {
    fn start(&self, call: AgentCall, done: Sender<Completion>) {
        let answer = (self.respond)(&call);
        let started = Instant::now();
        let slot = {
            let mut calls = self.calls.lock().unwrap_or_else(|e| e.into_inner());
            calls.push(HostCall { call: call.clone(), started, finished: None, cancelled: false });
            calls.len() - 1
        };
        let calls = self.calls.clone();
        let cancelled = self.cancelled.clone();
        let index = call.index;
        std::thread::spawn(move || {
            let until = started + answer.delay;
            while Instant::now() < until {
                if cancelled.lock().unwrap_or_else(|e| e.into_inner()).contains(&index) {
                    calls.lock().unwrap_or_else(|e| e.into_inner())[slot].cancelled = true;
                    let _ = done.send(Completion { index, result: AgentResult::skipped() });
                    return;
                }
                std::thread::sleep(Duration::from_millis(2).min(until.saturating_duration_since(Instant::now())));
            }
            calls.lock().unwrap_or_else(|e| e.into_inner())[slot].finished = Some(Instant::now());
            let _ = done.send(Completion { index, result: answer.result });
        });
    }

    fn cancel(&self, index: u32) {
        self.cancelled.lock().unwrap_or_else(|e| e.into_inner()).insert(index);
    }
}
