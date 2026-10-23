//! Optional live check of the employee runner against a real llama-server on the CPU.
//!
//! Ignored by default. Start Ollama's bundled `llama-server` on `marlowe-mini:4b-super` with
//! `-ngl 0` (CPU only, idle priority, ctx 4096, one slot, `--jinja`), then
//!
//! ```text
//! MARLOWE_LLAMA=127.0.0.1:18099 cargo test -p marlowe-project --test live_cpu -- --ignored --nocapture
//! ```
//!
//! The request body is built by the provider's own code (`plan_request`, `chat_body`,
//! `set_think_budget`), so what is checked is what Marlowe sends: a form call comes back as valid
//! JSON with thinking off, and with `think_budget` set the reasoning stops and the answer arrives.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use marlowe_model::{CancelToken, ChatRequest, Completion, Effort, ModelInfo, Provider, ProviderError, ProviderKind, StopReason, StreamEvent, SystemBlock, Usage};
use marlowe_project::engine::runner::{run_employee, EmployeeCall, RunnerConfig, Silent, ThinkSupport};
use marlowe_project::engine::{fence, Unlimited};
use marlowe_project::*;
use marlowe_provider::llama::chat::{chat_body, set_think_budget};
use marlowe_provider::llama::Thinking;
use marlowe_provider::ollama::{plan_request, BodyOptions};
use marlowe_provider::{http, LocalEndpoint};
use serde_json::{json, Value};

struct LlamaCpu {
    endpoint: LocalEndpoint,
    bodies: Mutex<Vec<Value>>,
}

impl Provider for LlamaCpu {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Ollama
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }

    fn stream(&self, req: &ChatRequest, _cancel: &CancelToken, on: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        let planned = plan_request(req, &BodyOptions::new(4096, Some(Value::Bool(req.effort != Effort::Off)))).map_err(|e| ProviderError::bad_request(e.to_string()))?;
        let mut body = chat_body(&planned.body, None, 0, &Thinking::Keeps, false);
        set_think_budget(&mut body, req.think_budget);
        body["stream"] = json!(false);
        self.bodies.lock().unwrap().push(body.clone());
        let v = http::post_json(&self.endpoint, "/v1/chat/completions", &body, Duration::from_secs(1200)).map_err(|e| ProviderError::network(e.to_string()))?;
        let msg = &v["choices"][0]["message"];
        let text = msg["content"].as_str().unwrap_or("").to_string();
        let reasoning = msg["reasoning_content"].as_str().unwrap_or("").to_string();
        let stop = if v["choices"][0]["finish_reason"] == "length" { StopReason::MaxTokens } else { StopReason::EndTurn };
        let usage = Usage { input: v["usage"]["prompt_tokens"].as_u64().unwrap_or(0), output: v["usage"]["completion_tokens"].as_u64().unwrap_or(0), ..Default::default() };
        eprintln!("  server: finish {:?}, {} prompt + {} output tokens, {} chars of reasoning, {} chars of answer", v["choices"][0]["finish_reason"], usage.input, usage.output, reasoning.len(), text.len());
        on(StreamEvent::TextDelta(text.clone()));
        Ok(Completion { text, stop, usage, ..Default::default() })
    }
}

fn provider() -> Option<LlamaCpu> {
    let url = std::env::var("MARLOWE_LLAMA").ok()?;
    Some(LlamaCpu { endpoint: LocalEndpoint::parse(&url).expect("MARLOWE_LLAMA is host:port on loopback"), bodies: Mutex::new(Vec::new()) })
}

fn project() -> ProjectState {
    let reg = Registry::builtin();
    let plan = parse_plan(
        &json!({"objective": "Find the newest and upcoming Kimi, DeepSeek and Qwen models and say what is confirmed and what is rumoured.",
                "questions": ["Which Kimi models are newest or upcoming?"],
                "fields": [{"key": "model_name"}, {"key": "status", "values": ["released", "announced", "rumoured", "unknown"]}]}),
        &reg,
        1,
    )
    .unwrap();
    ProjectState::new("p_00000001".into(), plan, &reg, 1)
}

const PAGE: &str = "Moonshot AI released Kimi K2.6 on 2026-10-20. It is a 1-trillion-parameter mixture-of-experts model with a 256k context window, \
available through the official API. The company said a larger model is in training and gave no date.";

#[test]
#[ignore = "needs a CPU llama-server: see the module docs"]
fn a_form_call_returns_valid_json_with_thinking_off() {
    let Some(p) = provider() else { return eprintln!("MARLOWE_LLAMA is not set") };
    let st = project();
    let form = st.forms.get("analyze@1").unwrap();
    let schema = model_schema(form, &st.plan);
    let call = EmployeeCall {
        model: "marlowe-mini:4b-super".into(),
        system: vec![SystemBlock { text: format!("You are the analyst on a project. Do exactly one task and answer with the form only.\nObjective: {}\nQuestion: {}", st.plan.objective, st.plan.questions[0].text), cache: true }],
        prompt: fence::fence("Page", PAGE, 3000),
        schema,
        think: marlowe_project::spec::Think::Off,
        think_budget: None,
        max_output: 1200,
        window_room: Some(2000),
        attempts: 3,
    };
    let began = Instant::now();
    let cfg = RunnerConfig { retry_pause: Duration::from_millis(0), think: ThinkSupport::Budgeted };
    let ok = run_employee(&p, &Unlimited, &CancelToken::new(), &cfg, &call, &mut Ok, &mut Silent).unwrap_or_else(|f| panic!("{f:?}"));
    eprintln!("thinking off: {} attempt(s), {} tokens, {:?}\n{}", ok.attempts, ok.usage.total(), began.elapsed(), serde_json::to_string_pretty(&ok.value).unwrap());
    let body = &p.bodies.lock().unwrap()[0];
    assert_eq!(body["response_format"]["type"], "json_schema");
    assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
    assert!(body.get("thinking_budget_tokens").is_none());
    assert!(body.get("tools").is_none());
    assert!(["Critical", "High", "Medium", "Low", "Skip"].contains(&ok.value["relevancy"].as_str().unwrap()));
}

#[test]
#[ignore = "needs a CPU llama-server: see the module docs"]
fn a_thinking_form_call_with_a_budget_stops_reasoning_and_answers() {
    let Some(p) = provider() else { return eprintln!("MARLOWE_LLAMA is not set") };
    let st = project();
    let form = st.forms.get("verify@1").unwrap();
    let schema = model_schema(form, &st.plan);
    let call = EmployeeCall {
        model: "marlowe-mini:4b-super".into(),
        system: vec![SystemBlock { text: "You check one claim against the passages given. Answer with the form only.".into(), cache: true }],
        prompt: format!("Claim: Kimi K2.6 has a 256k context window.\n{}", fence::fence("Passage 1", PAGE, 3000)),
        schema,
        think: marlowe_project::spec::Think::Think,
        think_budget: Some(200),
        max_output: 400,
        window_room: Some(1500),
        attempts: 3,
    };
    let began = Instant::now();
    let cfg = RunnerConfig { retry_pause: Duration::from_millis(0), think: ThinkSupport::Budgeted };
    let ok = run_employee(&p, &Unlimited, &CancelToken::new(), &cfg, &call, &mut Ok, &mut Silent).unwrap_or_else(|f| panic!("{f:?}"));
    eprintln!("thinking on, budget 200: {} attempt(s), {} tokens, {:?}\n{}", ok.attempts, ok.usage.total(), began.elapsed(), serde_json::to_string_pretty(&ok.value).unwrap());
    let body = &p.bodies.lock().unwrap()[0];
    assert_eq!(body["thinking_budget_tokens"], 200);
    assert_eq!(body["chat_template_kwargs"]["enable_thinking"], true);
    assert_eq!(body["n_predict"], 600, "the answer bound plus the reasoning allowance");
    assert!(["upheld", "partial", "refuted", "unsupported"].contains(&ok.value["verdict"].as_str().unwrap()));
}
