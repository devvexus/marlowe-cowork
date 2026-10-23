//! Shared by the engine tests: a project on a throwaway home, a fake model that answers any
//! form from its schema, and the fake code steps of the research menu.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver};
use marlowe_model::{CancelToken, ChatRequest, Completion, ModelInfo, Provider, ProviderError, ProviderKind, StopReason, StreamEvent, Usage};
use marlowe_project::engine::{Effect, Finished};
use marlowe_project::*;
use serde_json::{json, Map, Value};

/// A throwaway home that is removed when dropped.
pub struct Home(pub PathBuf);

impl Home {
    pub fn new(tag: &str) -> Home {
        let p = std::env::temp_dir().join(format!("marlowe-engine-{tag}-{}-{}", std::process::id(), uuid_like()));
        std::fs::create_dir_all(&p).unwrap();
        Home(p)
    }

    pub fn store(&self) -> ProjectStore {
        ProjectStore::open(&self.0.join("projects"), &self.0.join("cowork.db")).unwrap()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn uuid_like() -> String {
    use std::sync::atomic::AtomicU64;
    static N: AtomicU64 = AtomicU64::new(0);
    format!("{:x}{:x}", N.fetch_add(1, Ordering::SeqCst), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0))
}

/// A project with its state actor running.
pub struct Project {
    pub home: Home,
    pub id: String,
    pub actor: Option<ProjectActor>,
    pub handle: ProjectHandle,
    pub events: Receiver<ProjectEvent>,
    pub registry: Arc<Registry>,
}

pub fn fixed_clock() -> u64 {
    // 2026-10-23 12:00 UTC, always: runs are deterministic and "today" is known.
    1_792_756_800_000
}

pub fn actor_cfg() -> ActorConfig {
    ActorConfig { coalesce: Duration::from_millis(20), save_every: Duration::from_millis(40), clock: fixed_clock }
}

pub fn research_plan(questions: usize) -> Value {
    let qs: Vec<String> = (1..=questions).map(|i| format!("Which models of family number {i} are newest or upcoming?")).collect();
    json!({
        "objective": "Find the newest and upcoming models of three families and say what is confirmed and what is rumoured.",
        "questions": qs,
        "fields": [{"key": "model_name"}, {"key": "status", "values": ["released", "announced", "rumoured", "unknown"]}],
    })
}

impl Project {
    pub fn new(tag: &str, registry: Registry, plan: &Value) -> Project {
        let home = Home::new(tag);
        let plan = parse_plan(plan, &registry, fixed_clock()).expect("a valid plan");
        let state = ProjectState::new("p_7f3a91c2".into(), plan, &registry, fixed_clock());
        let store = home.store();
        store.create(&state).unwrap();
        Project::attach(home, state, registry)
    }

    /// Starts the actor on `state` (a restart: the state came from `project.json`).
    pub fn attach(home: Home, state: ProjectState, registry: Registry) -> Project {
        let (tx, rx) = unbounded();
        let id = state.id.clone();
        let actor = ProjectActor::spawn(home.store(), state, tx, actor_cfg()).unwrap();
        let handle = actor.handle();
        Project { home, id, actor: Some(actor), handle, events: rx, registry: Arc::new(registry) }
    }

    pub fn engine(&self, provider: Arc<dyn Provider>, gate: Arc<dyn engine::ModelGate>, tools: Toolbox, cfg: EngineConfig) -> Engine {
        Engine::new(self.handle.clone(), self.home.store(), self.registry.clone(), provider, gate, Arc::new(tools), cfg)
    }

    pub fn state(&self) -> ProjectState {
        self.handle.snapshot().unwrap()
    }

    /// Stops the actor (saving) and returns the home, for a "restart".
    pub fn stop(mut self) -> Home {
        if let Some(a) = self.actor.take() {
            a.stop();
        }
        let Project { home, .. } = self;
        home
    }
}

pub fn config(model: &str) -> EngineConfig {
    let mut c = EngineConfig::new(model);
    c.clock = fixed_clock;
    c.poll = Duration::from_millis(5);
    c.runner.retry_pause = Duration::from_millis(0);
    c.runner.think = engine::ThinkSupport::Budgeted;
    c
}

/// Polls until `f` is true, or panics after `secs`.
pub fn wait_until(secs: u64, what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out waiting for {what}");
}

/// Joins a run, panicking if it does not end within `secs`.
pub fn join_within(h: engine::RunHandle, secs: u64) -> RunOutcome {
    wait_until(secs, "the run to end", || h.is_finished());
    h.join()
}

// ---- the fake model -------------------------------------------------------------------------

/// What a scripted reply is.
pub enum Say {
    /// This JSON.
    Json(Value),
    /// This raw text.
    Text(String),
    /// A provider error.
    Err(ProviderError),
    /// The default: a value that fits the schema.
    Schema,
}

/// A value that satisfies `schema`: the first enum value, `x` for text, zero for numbers, one
/// element per array. Works for any form, so a fake model can answer a project nobody wrote a
/// fake for.
pub fn sample(schema: &Value) -> Value {
    if let Some(c) = schema.get("const") {
        return c.clone();
    }
    if let Some(e) = schema.get("enum").and_then(Value::as_array) {
        return e.iter().find(|v| !v.is_null()).cloned().unwrap_or(Value::Null);
    }
    let ty = match schema.get("type") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).find(|s| *s != "null").unwrap_or("null").to_string(),
        _ => "object".into(),
    };
    match ty.as_str() {
        "object" => {
            let mut m = Map::new();
            for (k, v) in schema.get("properties").and_then(Value::as_object).into_iter().flatten() {
                m.insert(k.clone(), sample(v));
            }
            Value::Object(m)
        }
        "array" => {
            let max = schema.get("maxItems").and_then(Value::as_u64).unwrap_or(1);
            let min = schema.get("minItems").and_then(Value::as_u64).unwrap_or(0);
            let n = min.max(1).min(max);
            let item = schema.get("items").cloned().unwrap_or(Value::Null);
            Value::Array((0..n).map(|_| sample(&item)).collect())
        }
        "string" => {
            let min = schema.get("minLength").and_then(Value::as_u64).unwrap_or(1) as usize;
            Value::String("x".repeat(min.max(1)))
        }
        "integer" => json!(schema.get("minimum").and_then(Value::as_f64).map(|m| m.ceil() as i64).unwrap_or(0).max(0)),
        "number" => json!(schema.get("minimum").and_then(Value::as_f64).unwrap_or(0.0)),
        "boolean" => json!(false),
        _ => Value::Null,
    }
}

type Responder = dyn Fn(&ChatRequest, usize, &CancelToken) -> Say + Send + Sync;

/// A provider that records every request and answers from `respond` (the schema sample by default).
pub struct Fake {
    pub calls: AtomicUsize,
    pub seen: Mutex<Vec<ChatRequest>>,
    respond: Box<Responder>,
    pub in_flight: AtomicUsize,
    pub peak: AtomicUsize,
}

impl Fake {
    pub fn schema() -> Arc<Fake> {
        Fake::with(|_, _, _| Say::Schema)
    }

    pub fn with(respond: impl Fn(&ChatRequest, usize, &CancelToken) -> Say + Send + Sync + 'static) -> Arc<Fake> {
        Arc::new(Fake { calls: AtomicUsize::new(0), seen: Mutex::new(Vec::new()), respond: Box::new(respond), in_flight: AtomicUsize::new(0), peak: AtomicUsize::new(0) })
    }

    pub fn count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    pub fn requests(&self) -> Vec<ChatRequest> {
        self.seen.lock().unwrap().clone()
    }

    /// The user message of every request, in arrival order.
    pub fn prompts(&self) -> Vec<String> {
        self.requests().iter().map(|r| r.messages.last().map(|m| m.joined_text()).unwrap_or_default()).collect()
    }
}

impl Provider for Fake {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Ollama
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }

    fn stream(&self, req: &ChatRequest, cancel: &CancelToken, on: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(req.clone());
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        let say = (self.respond)(req, n, cancel);
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        if cancel.is_cancelled() {
            return Err(ProviderError::cancelled());
        }
        let text = match say {
            Say::Err(e) => return Err(e),
            Say::Json(v) => v.to_string(),
            Say::Text(t) => t,
            Say::Schema => sample(req.format.as_ref().unwrap_or(&Value::Null)).to_string(),
        };
        on(StreamEvent::TextDelta(text.clone()));
        Ok(Completion { text, stop: StopReason::EndTurn, usage: Usage { input: 120, output: 30, ..Default::default() }, ..Default::default() })
    }
}

/// True when the request's schema has a property of this name (how a test tells the forms apart).
pub fn asks_for(req: &ChatRequest, property: &str) -> bool {
    req.format.as_ref().and_then(|f| f.get("properties")).and_then(|p| p.get(property)).is_some()
}

// ---- the research menu's code steps, as fakes -----------------------------------------------

/// Fake versions of every function the research menu names (stage C writes the real ones).
pub fn research_tools() -> Toolbox {
    let mut t = Toolbox::new();
    t.code("search_sources", |input, _| {
        let v = input.item.as_ref().expect("a vector");
        let mut out = Finished::default();
        for n in 1..=2 {
            let mut h = input.new_item("hit", "hit@1", &[&v.id, &n.to_string()]);
            h.label = format!("Hit {n} for {}", input.sub);
            h.data = json!({"url": format!("https://example.org/{}/{n}", input.sub), "title": h.label, "channel": "web", "origin": "search",
                            "identity_confirmed": false, "retracted": false, "rank": n, "question": input.sub, "round": 1})
            .as_object()
            .cloned()
            .unwrap();
            out.emits.push(h);
        }
        Ok(out)
    });
    t.code("fetch_page", |input, _| {
        let h = input.item.as_ref().expect("a hit");
        let mut p = input.new_item("page", "page@1", &[&h.id]);
        p.label = h.data["title"].as_str().unwrap_or("").to_string();
        p.data = json!({"source_id": format!("s_{}", h.id), "url": h.data["url"], "title": p.label,
                        "channel": "web", "status": "ok", "credibility": 0.5, "links": 0, "question": input.sub})
        .as_object()
        .cloned()
        .unwrap();
        Ok(Finished { emits: vec![p], ..Default::default() })
    });
    let index = Arc::new(AtomicUsize::new(0));
    t.finish("analyze_finish", move |input, map| {
        let Some(mut data) = map else { return Ok(Finished::default()) };
        let page = input.item.as_ref().expect("a page");
        let n = index.fetch_add(1, Ordering::SeqCst) + 1;
        data.insert("index".into(), json!(n));
        data.insert("title".into(), page.data["title"].clone());
        data.insert("source".into(), page.data["url"].clone());
        data.insert("credibility".into(), json!(0.5));
        let label = format!("{n}. {}", page.data["title"].as_str().unwrap_or(""));
        Ok(Finished { data, label: Some(label), ..Default::default() })
    });
    t.code("extract_claims", |input, _| {
        let s = input.item.as_ref().expect("a source");
        let mut out = Finished::default();
        for (i, c) in s.data["claims"].as_array().into_iter().flatten().enumerate() {
            let mut it = input.new_item("claim", "claim@1", &[&s.id, &i.to_string()]);
            it.label = c["text"].as_str().unwrap_or("").to_string();
            it.data = json!({"id": it.id, "text": c["text"], "quote": c["quote"], "source": s.id, "sub": input.sub, "status": "new"}).as_object().cloned().unwrap();
            out.emits.push(it);
        }
        Ok(out)
    });
    t.finish("condense_apply", |input, map| {
        // Ids the model did not receive are dropped; a claim in two groups stays in the first;
        // a claim in no group becomes a group of its own, so none is lost.
        let known: Vec<String> = input.members.iter().map(|m| m.id.clone()).collect();
        let mut taken: Vec<String> = Vec::new();
        let mut groups: Vec<(Vec<String>, String)> = Vec::new();
        for g in map.iter().flat_map(|m| m.get("groups").and_then(Value::as_array)).flatten() {
            let members: Vec<String> = g["members"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|id| known.iter().any(|k| k == id) && !taken.iter().any(|t| t == id))
                .map(str::to_string)
                .collect();
            if !members.is_empty() {
                taken.extend(members.iter().cloned());
                groups.push((members, g["claim"].as_str().unwrap_or("").to_string()));
            }
        }
        for m in &input.members {
            if !taken.contains(&m.id) {
                groups.push((vec![m.id.clone()], m.data["text"].as_str().unwrap_or("").to_string()));
            }
        }
        let mut out = Finished::default();
        for (members, claim) in groups {
            let mut it = input.new_item("cclaim", "cclaim@1", &[&members.join(",")]);
            it.label = claim.clone();
            it.data = json!({"claim": claim, "sub": input.sub, "stance": "supported"}).as_object().cloned().unwrap();
            out.emits.push(it);
        }
        Ok(out)
    });
    t.finish("verify_finish", |_input, map| {
        let Some(map) = map else { return Ok(Finished::default()) };
        let mut data = Map::new();
        data.insert("verdict".into(), map["verdict"].clone());
        Ok(Finished { data, ..Default::default() })
    });
    t.code("validated_output", |input, _| {
        let n = input.project_handle.read(|st| st.items_of_kind("cclaim").len()).map_err(|e| e.to_string())?;
        Ok(Finished { output: Some(json!({"claims": [], "sources": [], "coverage": {"claims": n}})), ..Default::default() })
    });
    t.code("route_after_thinking", |input, _| {
        let keep = input
            .project_handle
            .read(|st| st.items_of_kind("decision").last().map(|d| d.data.get("decision").is_some_and(|v| v == "keep_researching")).unwrap_or(false))
            .map_err(|e| e.to_string())?;
        let mut out = Finished::default();
        if keep {
            let round = input.round;
            let before = input.project_handle.read(|st| st.phases.iter().find(|p| p.ty == "Report").map(|p| p.id.clone())).map_err(|e| e.to_string())?;
            out.effects.push(Effect::InsertPhase { before, phase: Phase::pending("Checkin", round) });
        }
        Ok(out)
    });
    t.payload("checkin_card", |ctx| {
        let mut card = engine::builtin::checkin_card(ctx);
        let gaps: Vec<Value> = ctx
            .state
            .items_of_kind("decision")
            .last()
            .and_then(|d| d.data.get("gaps").and_then(Value::as_array).cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|g| g["question"].as_str())
            .filter(|q| q.chars().count() >= 10)
            .map(|q| json!({"q": q}))
            .collect();
        card.payload["proposed_questions"] = Value::Array(gaps);
        card
    });
    t.code("cited_batches", |input, _| {
        let mut out = Finished::default();
        let mut b = input.new_item("sentence_batch", "batch@1", &["1"]);
        b.data = json!({"batch": "1"}).as_object().cloned().unwrap();
        out.emits.push(b);
        Ok(out)
    });
    // Forms with no rules in the fake: the model's reply is the data as it is.
    for name in ["vectors_finish", "lead_finish", "section_finish", "answer_finish", "audit_finish"] {
        t.finish(name, |_, map| Ok(Finished { data: map.unwrap_or_default(), ..Default::default() }));
    }
    for name in ["assemble_report", "deliver", "dispose", "remember"] {
        t.code(name, |_, _| Ok(Finished::default()));
    }
    t
}
