//! The Ollama provider: `/api/chat` NDJSON streaming, catalog via `/api/tags` + `/api/show`.

mod leak;
mod models;
mod request;
mod stream;

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use marlowe_model::{
    CancelToken, ChatRequest, Completion, ModelInfo, Provider, ProviderError, ProviderKind,
    StreamEvent,
};
use serde_json::{json, Value};

use crate::http::{self, HttpError, LocalEndpoint, NdjsonError, NdjsonStream};

pub use leak::{defang_tool_markup, recover_leaked_call};
pub use models::{
    is_cloud_tag, model_info, parse_show, parse_tags, think_value, ModelMeta, TagEntry, ThinkMode,
};
pub use request::request_body;
pub use stream::Folder;

/// Context window sent as `num_ctx` when the model allows it. Ollama's own default is 2048.
pub const DEFAULT_CONTEXT_TOKENS: u32 = 32_768;

/// Longest a streaming read may wait for the next byte (model load included).
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(180);

/// Timeout for catalog calls.
pub const CATALOG_TIMEOUT: Duration = Duration::from_secs(10);

pub struct OllamaProvider {
    endpoint: LocalEndpoint,
    num_ctx: u32,
    idle: Duration,
    meta: Mutex<HashMap<String, ModelMeta>>,
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
            idle: DEFAULT_IDLE_TIMEOUT,
            meta: Mutex::new(HashMap::new()),
        }
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

    /// The body `stream` would send, for inspection.
    pub fn build_body(&self, req: &ChatRequest, meta: &ModelMeta) -> Value {
        let num_ctx = meta.context.map_or(self.num_ctx, |c| c.min(self.num_ctx));
        request_body(req, num_ctx, think_value(meta.think_mode(), req.effort))
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
            out.push(model_info(&t.name, &meta, self.num_ctx));
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
        let body = self.build_body(req, &meta);
        let thinking = body.get("think").is_some_and(|t| t != &Value::Bool(false));

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
        let mut folder = Folder::new(thinking, on);
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
