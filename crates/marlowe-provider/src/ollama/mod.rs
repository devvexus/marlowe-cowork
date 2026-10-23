//! The Ollama provider: `/api/chat` NDJSON streaming, catalog via `/api/tags` + `/api/show`.
//!
//! Beyond the wire: images ride as base64 `images` when the model reports the `vision`
//! capability (`request.rs`); every request is checked against its `num_ctx` before it is sent
//! (A15); tool-call arguments are repaired against the request's schemas and calls written as
//! text are recovered (`repair.rs`, `scan.rs`), counted in [`RepairStats`] for the bench (A4).

mod leak;
mod models;
pub mod repair;
mod request;
pub mod scan;
mod stream;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use marlowe_model::image::BlobStore;
use marlowe_model::{
    CancelToken, ChatRequest, Completion, ContextAllocations, Lane, ModelInfo, Provider, ProviderError, ProviderKind,
    StreamEvent,
};
use serde_json::{json, Value};

use crate::http::{self, HttpError, LocalEndpoint, NdjsonError, NdjsonStream};
use crate::llama::render::RenderGuard;
use crate::llama::{chat as llama_chat, conversation_key, LlamaEngine, Want};

pub use leak::{defang_tool_markup, recover_leaked_call};
pub use models::{
    is_cloud_tag, model_info, parse_show, parse_tags, think_value, ModelMeta, TagEntry, ThinkMode,
};
pub use repair::{RepairSnapshot, RepairStats};
pub use request::{
    is_context_overflow, plan_request, request_body, request_body_with, BodyOptions, Planned, CONTEXT_OVERFLOW,
};
pub use stream::{clip_detail, is_tool_parse_error, Folder};

/// Context window sent as `num_ctx` when the model allows it. Ollama's own default is 2048.
pub const DEFAULT_CONTEXT_TOKENS: u32 = 32_768;

/// Longest a streaming read may wait for the next byte (model load included).
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(180);

/// Timeout for catalog calls.
pub const CATALOG_TIMEOUT: Duration = Duration::from_secs(10);

pub struct OllamaProvider {
    endpoint: LocalEndpoint,
    /// The window for a model with no context policy.
    num_ctx: u32,
    /// Per-model windows from the context policy: one allocation per model, sent on every
    /// request, so Ollama never reloads a runner because two agents asked for different sizes.
    alloc: ContextAllocations,
    idle: Duration,
    meta: Mutex<HashMap<String, ModelMeta>>,
    /// Where image bytes live (`<home>\blobs`). Without one, images are sent as a text note.
    blobs: Option<Arc<dyn BlobStore>>,
    stats: Arc<RepairStats>,
    /// Marlowe's own llama-server, for the models the planner gave it (see [`crate::llama`]).
    llama: Option<LlamaEngine>,
    /// Checks that each request to the engine renders as the last prompt plus its reply.
    render: RenderGuard,
    /// Whether the conductor's requests to the engine ask for the generated tokens (the exact text
    /// of the reply). Cleared for good when the server refuses them.
    raw_capture: AtomicBool,
}

impl Default for OllamaProvider {
    fn default() -> Self {
        Self::new(LocalEndpoint::default_ollama())
    }
}

impl OllamaProvider {
    pub fn new(endpoint: LocalEndpoint) -> Self {
        Self {
            endpoint,
            num_ctx: DEFAULT_CONTEXT_TOKENS,
            alloc: ContextAllocations::new(),
            idle: DEFAULT_IDLE_TIMEOUT,
            meta: Mutex::new(HashMap::new()),
            blobs: None,
            stats: Arc::new(RepairStats::new()),
            llama: None,
            render: RenderGuard::default(),
            raw_capture: AtomicBool::new(true),
        }
    }

    /// Serves the models the context policy assigns to Marlowe's own llama-server through
    /// `engine`; every other model stays on Ollama.
    pub fn with_llama_engine(mut self, engine: LlamaEngine) -> Self {
        self.llama = Some(engine);
        self
    }

    /// The blob store `Part::Image` bytes are read from (the one `read` and paste write to).
    pub fn with_blobs(mut self, blobs: Arc<dyn BlobStore>) -> Self {
        self.blobs = Some(blobs);
        self
    }

    /// Counts repaired tool calls in `stats` instead of the provider's own counters.
    pub fn with_repair_stats(mut self, stats: Arc<RepairStats>) -> Self {
        self.stats = stats;
        self
    }

    /// How many tool calls were clean, repaired, recovered from text or unusable (A4).
    pub fn repair_stats(&self) -> Arc<RepairStats> {
        self.stats.clone()
    }

    /// From an optional base URL (`http://127.0.0.1:11434`). Only loopback hosts are accepted.
    pub fn from_base_url(url: Option<&str>) -> Result<Self, ProviderError> {
        match url {
            None => Ok(Self::default()),
            Some(u) => LocalEndpoint::parse(u).map(Self::new).ok_or_else(|| {
                ProviderError::bad_request(format!("'{u}' is not a loopback Ollama address"))
            }),
        }
    }

    /// Caps `num_ctx` for every request (the model's own limit still applies).
    pub fn with_context_tokens(mut self, tokens: u32) -> Self {
        self.num_ctx = tokens.max(512);
        self
    }

    /// Shares the per-model context allocations the library keeps.
    pub fn with_allocations(mut self, alloc: ContextAllocations) -> Self {
        self.alloc = alloc;
        self
    }

    /// The `num_ctx` requests for `model` send: its policy's allocation, else the default; never
    /// above the model's own maximum.
    pub fn context_for(&self, model: &str, meta: &ModelMeta) -> u32 {
        let want = self.alloc.get(model).unwrap_or(self.num_ctx);
        meta.context.map_or(want, |c| c.min(want))
    }

    pub fn with_idle_timeout(mut self, idle: Duration) -> Self {
        self.idle = idle;
        self
    }

    pub fn endpoint(&self) -> &LocalEndpoint {
        &self.endpoint
    }

    pub fn context_tokens(&self) -> u32 {
        self.num_ctx
    }

    /// True when something answering like Ollama is listening.
    pub fn is_running(&self, timeout: Duration) -> bool {
        http::get_json(&self.endpoint, "/api/tags", timeout)
            .ok()
            .and_then(|v| parse_tags(&v))
            .is_some()
    }

    fn cached(&self, model: &str) -> Option<ModelMeta> {
        self.meta.lock().unwrap_or_else(|e| e.into_inner()).get(model).cloned()
    }

    fn remember(&self, model: &str, meta: ModelMeta) {
        self.meta.lock().unwrap_or_else(|e| e.into_inner()).insert(model.to_string(), meta);
    }

    fn show(&self, model: &str) -> Result<ModelMeta, ProviderError> {
        let v = http::post_json(&self.endpoint, "/api/show", &json!({ "model": model }), CATALOG_TIMEOUT)
            .map_err(|e| self.error(e, Some(model)))?;
        Ok(parse_show(&v))
    }

    /// What is known about `model`, asking `/api/show` once if needed.
    pub fn meta(&self, model: &str) -> Result<ModelMeta, ProviderError> {
        if let Some(m) = self.cached(model) {
            if m.is_complete() {
                return Ok(m);
            }
        }
        let shown = self.show(model)?;
        let merged = match self.cached(model) {
            Some(m) => models::merge(&m, &shown),
            None => shown,
        };
        self.remember(model, merged.clone());
        Ok(merged)
    }

    fn body_options<'a>(&'a self, req: &ChatRequest, meta: &ModelMeta) -> BodyOptions<'a> {
        BodyOptions {
            vision: meta.has("vision"),
            blobs: self.blobs.as_deref(),
            ..BodyOptions::new(self.context_for(&req.model, meta), think_value(meta.think_mode(), req.effort))
        }
    }

    /// The body `stream` would send, for inspection (no window check).
    pub fn build_body(&self, req: &ChatRequest, meta: &ModelMeta) -> Value {
        request_body_with(req, &self.body_options(req, meta))
    }

    /// The body `stream` sends, or the `context_overflow` error it would stop on.
    pub fn plan(&self, req: &ChatRequest, meta: &ModelMeta) -> Result<Planned, ProviderError> {
        plan_request(req, &self.body_options(req, meta))
    }

    fn error(&self, e: HttpError, model: Option<&str>) -> ProviderError {
        match e {
            HttpError::Cancelled => ProviderError::cancelled(),
            HttpError::Unreachable { detail, .. } => ProviderError::network(format!(
                "Ollama is not reachable at {} ({detail}). Start it with `ollama serve`.",
                self.endpoint
            )),
            HttpError::Status { status, body, .. } => {
                let said = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
                    .unwrap_or(body);
                let detail = match (status, model) {
                    (404, Some(m)) => format!("Ollama has no model `{m}`. Run `ollama pull {m}`. ({said})"),
                    _ => format!("Ollama returned HTTP {status}: {said}"),
                };
                if status >= 500 && stream::is_tool_parse_error(&said) {
                    return ProviderError::malformed_call(stream::clip_detail(&said));
                }
                ProviderError::from_status(status, detail)
            }
            HttpError::Malformed { detail, .. } => {
                ProviderError::server(format!("malformed response from Ollama: {detail}"))
            }
        }
    }
}

impl Provider for OllamaProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Ollama
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let v = http::get_json(&self.endpoint, "/api/tags", CATALOG_TIMEOUT)
            .map_err(|e| self.error(e, None))?;
        let tags = parse_tags(&v).ok_or_else(|| {
            ProviderError::server(format!("{} answered /api/tags without a `models` list", self.endpoint))
        })?;
        let mut out = Vec::with_capacity(tags.len());
        for t in tags {
            let meta = if t.meta.is_complete() {
                t.meta
            } else {
                match self.show(&t.name) {
                    Ok(shown) => models::merge(&t.meta, &shown),
                    Err(_) => t.meta,
                }
            };
            if !meta.can_chat() {
                continue;
            }
            out.push(model_info(&t.name, &meta, self.alloc.get(&t.name).unwrap_or(self.num_ctx)));
            self.remember(&t.name, meta);
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    fn stream(
        &self,
        req: &ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<Completion, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::cancelled());
        }
        let meta = self.meta(&req.model)?;
        if let Some(engine) = &self.llama {
            match self.alloc.engine_shape(&req.model) {
                Some(shape) => return self.stream_llama(engine, shape, req, &meta, cancel, on),
                // Ollama serves it now (the setting was turned off): never in memory twice.
                None => {
                    engine.unload(Some(&req.model));
                }
            }
        }
        let body = self.plan(req, &meta)?.body;
        let thinking = body.get("think").is_some_and(|t| t != &Value::Bool(false));

        // One slot per model: a request made here replaces the engine's cache for the next one.
        marlowe_model::prefix::note_request(&req.model);
        // The model's metadata and the body are ready; what follows is the engine's time.
        on(StreamEvent::Sent);
        let reader = http::post_stream(
            &self.endpoint,
            "/api/chat",
            &body,
            "application/x-ndjson",
            cancel,
            self.idle,
        )
        .map_err(|e| self.error(e, Some(&req.model)))?;

        let mut frames = NdjsonStream::new(reader);
        let mut folder = Folder::new(thinking, on).with_tools(&req.tools).with_stats(self.stats.clone());
        while let Some(frame) = frames.next_value() {
            if cancel.is_cancelled() {
                return Err(ProviderError::cancelled());
            }
            match frame {
                Ok(v) => folder.frame(&v)?,
                Err(NdjsonError::Io(e)) => {
                    return Err(self.error(http::read_error(&self.endpoint, e, Some(cancel)), None))
                }
                Err(NdjsonError::Json(detail)) => {
                    return Err(ProviderError::server(format!("malformed frame from Ollama: {detail}")))
                }
            }
            if folder.is_done() {
                break;
            }
        }
        if cancel.is_cancelled() {
            return Err(ProviderError::cancelled());
        }
        if !folder.is_done() {
            return Err(ProviderError::network("Ollama closed the stream before it finished"));
        }
        Ok(folder.finish())
    }
}

impl OllamaProvider {
    /// One call on Marlowe's own llama-server: the window check at the slot's size, a slot for the
    /// conversation, the request Ollama would have sent llama-server, its answer through the same
    /// folder as Ollama's.
    fn stream_llama(
        &self,
        engine: &LlamaEngine,
        shape: marlowe_model::EngineShape,
        req: &ChatRequest,
        meta: &ModelMeta,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<Completion, ProviderError> {
        let opts = BodyOptions::new(shape.ctx_per_slot, think_value(meta.think_mode(), req.effort));
        let planned = plan_request(req, &opts)?;
        let thinking = planned.body.get("think").is_some_and(|t| t != &Value::Bool(false));
        let want = Want { model: req.model.clone(), shape, lane: req.lane, key: conversation_key(req) };
        let lease = engine.acquire(&want, cancel)?;
        // The user's conversation is the one that can be read ahead, so it is the one whose exact
        // generated text is worth asking the server for.
        let mut raw_asked = req.lane == Lane::Conductor && self.raw_capture.load(Ordering::Relaxed);
        let mut body = llama_chat::chat_body(&planned.body, lease.sampling.as_ref(), lease.slot, &lease.thinking, raw_asked);
        llama_chat::set_think_budget(&mut body, req.think_budget);
        // The prompt must extend the last one plus its reply, as the model's template renders it.
        self.render.before(&lease.endpoint, &req.model, want.key, &body);
        on(StreamEvent::Sent);
        let post = |body: &Value| http::post_stream(&lease.endpoint, "/v1/chat/completions", body, "text/event-stream", cancel, LLAMA_IDLE_TIMEOUT);
        let reader = match post(&body) {
            Err(e) if raw_asked && refuses_token_probs(&e) => {
                // This server cannot send the tokens with the stream: the template's printing of
                // the reply stands in for them from now on.
                self.raw_capture.store(false, Ordering::Relaxed);
                raw_asked = false;
                body = llama_chat::chat_body(&planned.body, lease.sampling.as_ref(), lease.slot, &lease.thinking, false);
                llama_chat::set_think_budget(&mut body, req.think_budget);
                post(&body)
            }
            other => other,
        }
        .map_err(llama_error)?;
        let mut folder = Folder::new(thinking, on).with_tools(&req.tools).with_stats(self.stats.clone());
        let mut raw = llama_chat::RawText::default();
        let timings = llama_chat::read_stream_raw(reader, &mut folder, cancel, &mut raw)?;
        let done = folder.finish();
        let generated = raw.text().filter(|_| raw_asked && raw.covers(timings.predicted_n));
        if raw_asked && generated.is_none() {
            // This server's chat stream leaves out the tokens its parser swallows, so the text is
            // never the whole reply: stop asking, and let the template's printing stand in.
            self.raw_capture.store(false, Ordering::Relaxed);
        }
        let ahead = self.hold_reply(&lease, want.key, req, &opts, &body, &done, generated.as_deref());
        match ahead.filter(|_| req.lane == Lane::Conductor) {
            Some(prefix) => lease.read_ahead(prefix),
            None => drop(lease),
        }
        Ok(done)
    }

    /// Renders the conversation with the model's reply on the end: the prompt its cache holds now.
    /// Returns the text to read into the slot ahead of the next request, when the turn ended and
    /// the template will print it differently once the next user message follows it.
    #[allow(clippy::too_many_arguments)]
    fn hold_reply(
        &self,
        lease: &crate::llama::engine::Lease,
        key: u64,
        req: &ChatRequest,
        opts: &BodyOptions<'_>,
        body: &Value,
        done: &Completion,
        generated: Option<&str>,
    ) -> Option<String> {
        let message = done.kept_message()?;
        let mut next = req.clone();
        next.messages.push(message);
        let planned = plan_request(&next, opts).ok()?;
        let sent = llama_chat::chat_body(&planned.body, lease.sampling.as_ref(), lease.slot, &lease.thinking, false);
        let reply = sent.get("messages").and_then(Value::as_array).and_then(|m| m.last()).cloned()?;
        let tail = if done.text.trim().is_empty() {
            done.thinking.as_ref().map(|t| t.text.as_str()).unwrap_or("")
        } else {
            done.text.as_str()
        };
        self.render.after(&lease.endpoint, &req.model, key, body, &reply, tail, !done.tool_calls.is_empty(), generated)
    }
}

/// Whether the server turned the request down for asking for the generated tokens.
fn refuses_token_probs(e: &HttpError) -> bool {
    match e {
        HttpError::Status { status, body, .. } => (400..500).contains(status) && body.to_ascii_lowercase().contains("prob"),
        _ => false,
    }
}

/// Longest a llama.cpp stream may stay silent: a prefill reports progress per batch, so a long
/// read is never this quiet.
const LLAMA_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

fn llama_error(e: HttpError) -> ProviderError {
    match e {
        HttpError::Cancelled => ProviderError::cancelled(),
        HttpError::Unreachable { detail, .. } => {
            ProviderError::network(format!("the llama.cpp server did not answer ({detail}); the next request starts it again"))
        }
        HttpError::Status { status, body, .. } => match serde_json::from_str::<Value>(&body) {
            Ok(v) if v.get("error").is_some() => {
                let mut e = llama_chat::error_from(&v["error"]);
                if e.kind == marlowe_model::ProviderErrorKind::Server && status < 500 {
                    e = ProviderError::from_status(status, e.detail);
                }
                e
            }
            _ => ProviderError::from_status(status, format!("llama.cpp returned HTTP {status}: {body}")),
        },
        HttpError::Malformed { detail, .. } => ProviderError::server(format!("malformed response from llama.cpp: {detail}")),
    }
}

#[cfg(test)]
mod alloc_tests {
    use super::*;
    use marlowe_model::Message;

    #[test]
    fn num_ctx_follows_the_policy_and_never_exceeds_the_model() {
        let alloc = ContextAllocations::new();
        let p = OllamaProvider::default().with_allocations(alloc.clone());
        let meta = ModelMeta { capabilities: Some(vec!["completion".into()]), context: Some(262_144), family: None };
        let req = ChatRequest::new("dusk", vec![Message::user("hi")]);
        assert_eq!(p.build_body(&req, &meta)["options"]["num_ctx"], DEFAULT_CONTEXT_TOKENS);
        alloc.set("dusk", 47_104);
        assert_eq!(p.build_body(&req, &meta)["options"]["num_ctx"], 47_104);
        // Another model keeps the default: one allocation per model.
        let other = ChatRequest::new("other", vec![Message::user("hi")]);
        assert_eq!(p.build_body(&other, &meta)["options"]["num_ctx"], DEFAULT_CONTEXT_TOKENS);
        // The backstop: never above what the model supports.
        alloc.set("dusk", 1_000_000);
        assert_eq!(p.build_body(&req, &meta)["options"]["num_ctx"], 262_144);
    }
}
