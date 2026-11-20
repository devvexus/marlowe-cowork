//! The local provider: the models in Ollama's store, served by Marlowe's own llama.cpp engine.
//!
//! Ollama is only a source of model files. The catalog and each model's capabilities are read from
//! the store on disk ([`crate::ollama_store::list_in`], [`crate::llama::eligible`]); Ollama's
//! server is never asked anything and need not run. Every request goes to the engine
//! ([`crate::llama`]) as the body Ollama would have sent its llama-server.
//!
//! The Ollama-shaped body is still the one the request is planned as (`request.rs`): images ride
//! as base64 when the model has a projector, every request is checked against its window before
//! it is sent (A15), and tool-call arguments are repaired against the request's schemas and calls
//! written as text recovered (`repair.rs`, `scan.rs`), counted in [`RepairStats`] (A4).

mod leak;
mod models;
pub mod repair;
mod request;
pub mod scan;
mod stream;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use marlowe_model::image::BlobStore;
use marlowe_model::{
    CancelToken, ChatRequest, Completion, ContextAllocations, EngineShape, Lane, ModelInfo, Provider, ProviderError, ProviderKind,
    StreamEvent,
};
use serde_json::Value;

use crate::http::{self, HttpError};
use crate::llama::render::RenderGuard;
use crate::llama::{chat as llama_chat, conversation_key, eligible, LlamaEngine, Want};
use crate::ollama_store;

pub use leak::{defang_tool_markup, recover_leaked_call};
pub use models::{is_cloud_tag, model_info, think_value, ModelMeta, ThinkMode};
pub use repair::{RepairSnapshot, RepairStats};
pub use request::{
    is_context_overflow, plan_request, request_body, request_body_with, BodyOptions, Planned, CONTEXT_OVERFLOW,
};
pub use stream::{clip_detail, is_tool_parse_error, Folder};

/// Context window a model with no context policy gets.
pub const DEFAULT_CONTEXT_TOKENS: u32 = 32_768;

pub struct OllamaProvider {
    /// Ollama's model store.
    store: PathBuf,
    /// The window for a model with no context policy.
    num_ctx: u32,
    /// Per-model windows from the context policy, and the planner's engine shapes.
    alloc: ContextAllocations,
    meta: Mutex<HashMap<String, ModelMeta>>,
    /// Where image bytes live (`<home>\blobs`). Without one, images are sent as a text note.
    blobs: Option<Arc<dyn BlobStore>>,
    stats: Arc<RepairStats>,
    /// Marlowe's own llama-server, which serves every model (see [`crate::llama`]).
    llama: Option<LlamaEngine>,
    /// Checks that each request to the engine renders as the last prompt plus its reply.
    render: RenderGuard,
    /// Whether the conductor's requests to the engine ask for the generated tokens (the exact text
    /// of the reply). Cleared for good when the server refuses them.
    raw_capture: AtomicBool,
}

impl Default for OllamaProvider {
    fn default() -> Self {
        Self::new(ollama_store::store_root())
    }
}

impl OllamaProvider {
    /// The models in the store at `store`.
    pub fn new(store: PathBuf) -> Self {
        Self {
            store,
            num_ctx: DEFAULT_CONTEXT_TOKENS,
            alloc: ContextAllocations::new(),
            meta: Mutex::new(HashMap::new()),
            blobs: None,
            stats: Arc::new(RepairStats::new()),
            llama: None,
            render: RenderGuard::default(),
            raw_capture: AtomicBool::new(true),
        }
    }

    /// Serves every model through `engine`. Without one a request is an error naming why.
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

    /// Caps the window of a model with no policy (the model's own limit still applies).
    pub fn with_context_tokens(mut self, tokens: u32) -> Self {
        self.num_ctx = tokens.max(512);
        self
    }

    /// Shares the per-model context allocations the library keeps.
    pub fn with_allocations(mut self, alloc: ContextAllocations) -> Self {
        self.alloc = alloc;
        self
    }

    /// The window requests for `model` use: its policy's allocation, else the default; never
    /// above the model's own maximum.
    pub fn context_for(&self, model: &str, meta: &ModelMeta) -> u32 {
        let want = self.alloc.get(model).unwrap_or(self.num_ctx);
        meta.context.map_or(want, |c| c.min(want))
    }

    pub fn context_tokens(&self) -> u32 {
        self.num_ctx
    }

    pub fn store(&self) -> &std::path::Path {
        &self.store
    }

    /// What is known about `model`, read from the store once.
    pub fn meta(&self, model: &str) -> Result<ModelMeta, ProviderError> {
        if let Some(m) = self.meta.lock().unwrap_or_else(|e| e.into_inner()).get(model) {
            return Ok(m.clone());
        }
        let m = eligible::resolve(&self.store, model).map_err(|why| ProviderError::bad_request(format!("{model} can't run: {why}")))?;
        self.meta.lock().unwrap_or_else(|e| e.into_inner()).insert(model.to_string(), m.meta.clone());
        Ok(m.meta)
    }

    fn body_options<'a>(&'a self, req: &ChatRequest, meta: &ModelMeta, num_ctx: u32) -> BodyOptions<'a> {
        BodyOptions { vision: meta.has("vision"), blobs: self.blobs.as_deref(), ..BodyOptions::new(num_ctx, think_value(meta.think_mode(), req.effort)) }
    }

    /// The Ollama-shaped body a request is planned as, for inspection (no window check).
    pub fn build_body(&self, req: &ChatRequest, meta: &ModelMeta) -> Value {
        request_body_with(req, &self.body_options(req, meta, self.context_for(&req.model, meta)))
    }

    /// The planned body, or the `context_overflow` error a request would stop on.
    pub fn plan(&self, req: &ChatRequest, meta: &ModelMeta) -> Result<Planned, ProviderError> {
        plan_request(req, &self.body_options(req, meta, self.context_for(&req.model, meta)))
    }

    /// The planner's shape for `model`, else one slot of the model's window on the GPU (the engine
    /// still checks the room and runs it split when it does not fit).
    fn shape_for(&self, model: &str, meta: &ModelMeta) -> EngineShape {
        self.alloc.engine_shape(model).unwrap_or_else(|| EngineShape {
            slots: 1,
            ctx_per_slot: self.context_for(model, meta),
            kv: "f16".into(),
            split: false,
        })
    }
}

impl Provider for OllamaProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Ollama
    }

    /// Every chat model in the store. One llama.cpp cannot run is still listed, so choosing it says
    /// why instead of the model being missing.
    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let mut out = Vec::new();
        for name in ollama_store::list_in(&self.store) {
            let meta = match eligible::resolve(&self.store, &name) {
                Ok(m) => m.meta,
                Err(_) => ModelMeta::default(),
            };
            if !meta.can_chat() {
                continue;
            }
            out.push(model_info(&name, &meta, self.alloc.get(&name).unwrap_or(self.num_ctx)));
            self.meta.lock().unwrap_or_else(|e| e.into_inner()).insert(name, meta);
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
        let Some(engine) = &self.llama else {
            return Err(ProviderError::bad_request(format!(
                "{} is a local model and this run has no llama.cpp engine to serve it",
                req.model
            )));
        };
        let meta = self.meta(&req.model)?;
        // The request is on its way once, however long the engine takes to start.
        let mut sent = false;
        let mut once = |ev: StreamEvent| {
            if matches!(ev, StreamEvent::Sent) && std::mem::replace(&mut sent, true) {
                return;
            }
            on(ev)
        };
        let shape = self.shape_for(&req.model, &meta);
        self.stream_llama(engine, shape, req, &meta, cancel, &mut once)
    }
}

impl OllamaProvider {
    /// One call on Marlowe's own llama-server: the window check at the slot's size, a slot for the
    /// conversation, the request Ollama would have sent llama-server, its answer through the
    /// folder that repairs and recovers tool calls.
    fn stream_llama(
        &self,
        engine: &LlamaEngine,
        shape: EngineShape,
        req: &ChatRequest,
        meta: &ModelMeta,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<Completion, ProviderError> {
        let opts = self.body_options(req, meta, shape.ctx_per_slot);
        let planned = plan_request(req, &opts)?;
        let thinking = planned.body.get("think").is_some_and(|t| t != &Value::Bool(false));
        let want = Want {
            model: req.model.clone(),
            shape,
            lane: req.lane,
            key: conversation_key(req),
            need_mib: self.alloc.engine_need_mib(&req.model),
        };
        // A cold server is the model's load, and it happens inside `acquire`: the call is on its
        // way now, so the live meter opens the load line here and ends it when the server is up
        // (its real time), and the wait is not mistaken for the model reading the prompt.
        if !engine.runs(&want) {
            on(StreamEvent::Sent);
        }
        let lease = engine.acquire(&want, cancel)?;
        // The user's conversation is the one that can be read ahead, so it is the one whose exact
        // generated text is worth asking the server for.
        let mut raw_asked = req.lane == Lane::Conductor && self.raw_capture.load(Ordering::Relaxed);
        let mut body = llama_chat::chat_body(&planned.body, Some(&lease.defaults), lease.slot, &lease.thinking, raw_asked);
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
                body = llama_chat::chat_body(&planned.body, Some(&lease.defaults), lease.slot, &lease.thinking, false);
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
        let sent = llama_chat::chat_body(&planned.body, Some(&lease.defaults), lease.slot, &lease.thinking, false);
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
        let p = OllamaProvider::new(PathBuf::from("no-store")).with_allocations(alloc.clone());
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

    #[test]
    fn without_an_engine_a_request_is_an_error_and_nothing_is_sent_anywhere() {
        let p = OllamaProvider::new(PathBuf::from("no-store"));
        let e = p.stream(&ChatRequest::new("dusk", vec![Message::user("hi")]), &CancelToken::new(), &mut |_| {}).unwrap_err();
        assert!(e.detail.contains("no llama.cpp engine"), "{}", e.detail);
    }
}
