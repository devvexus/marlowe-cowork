//! [`OpenRouterProvider`]: `marlowe_model::Provider` over OpenRouter's `/chat/completions`.
//!
//! The blocking HTTPS read runs on a worker thread that forwards decoded SSE payloads over a
//! channel; the calling thread folds them and polls the [`CancelToken`]. `marlowe-net` gives no
//! handle on its socket, and on Windows a blocked read is not woken by a shutdown anyway, so this
//! is what makes cancel prompt. An abandoned worker exits at its next frame or read timeout.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError};
use marlowe_model::{
    CancelToken, ChatRequest, Completion, ModelInfo, Provider, ProviderError, ProviderErrorKind,
    ProviderKind, StreamEvent,
};
use marlowe_provider::sse::{Frame, SseStream};
use serde_json::Value;

use crate::catalog::{self, CatalogEntry, ReasoningMeta};
use crate::request::request_body;
use crate::retry::{self, Disposition};
use crate::secret::ApiKey;
use crate::stream::Accumulator;
use crate::transport::{TlsTransport, Transport};

/// What OpenRouter is told the client is. Constants: nothing user-supplied reaches a header.
const REFERER: &str = "https://github.com/marlowe-cowork";
const TITLE: &str = "Marlowe";

/// Catalog bodies above this are refused.
const MAX_CATALOG_BYTES: usize = 16 * 1024 * 1024;

/// How often the caller wakes to check for cancellation.
const POLL: Duration = Duration::from_millis(100);

pub struct OpenRouterProvider {
    transport: Arc<dyn Transport>,
    key: Option<ApiKey>,
    reasoning: Mutex<HashMap<String, ReasoningMeta>>,
}

impl OpenRouterProvider {
    /// Over HTTPS. Without a key the catalog still works; `stream` fails with `Auth`.
    pub fn new(key: Option<ApiKey>) -> Self {
        Self::with_transport(Arc::new(TlsTransport::new()), key)
    }

    pub fn with_transport(transport: Arc<dyn Transport>, key: Option<ApiKey>) -> Self {
        Self { transport, key, reasoning: Mutex::new(HashMap::new()) }
    }

    /// The stored credential if given, else `OPENROUTER_API_KEY`.
    pub fn from_key(stored: Option<&str>) -> Self {
        Self::new(stored.and_then(ApiKey::new).or_else(ApiKey::from_environment))
    }

    pub fn has_key(&self) -> bool {
        self.key.is_some()
    }

    fn redact(&self, text: &str) -> String {
        match &self.key {
            Some(k) => k.redact(text),
            None => text.to_string(),
        }
    }

    fn err(&self, kind: ProviderErrorKind, detail: impl AsRef<str>) -> ProviderError {
        ProviderError::new(kind, self.redact(detail.as_ref()))
    }

    /// The full catalog. Also caches each model's reasoning metadata for effort mapping.
    pub fn catalog(&self) -> Result<Vec<CatalogEntry>, ProviderError> {
        let mut resp = self
            .transport
            .get("/models")
            .map_err(|e| self.err(ProviderErrorKind::Network, e.to_string()))?;
        let body = resp.read_capped(MAX_CATALOG_BYTES);
        if !(200..300).contains(&resp.status) {
            return Err(ProviderError::from_status(
                resp.status,
                self.redact(&format!("OpenRouter answered HTTP {} for its model list: {}", resp.status, first_line(&body))),
            ));
        }
        let entries = catalog::parse_catalog(&body).map_err(|e| self.err(ProviderErrorKind::Server, e))?;
        let mut cache = self.reasoning.lock().unwrap_or_else(|e| e.into_inner());
        for e in &entries {
            cache.insert(e.info.id.clone(), e.reasoning.clone());
        }
        Ok(entries)
    }

    /// The body `stream` would send. Uses cached catalog metadata when present.
    pub fn build_body(&self, req: &ChatRequest) -> Value {
        let meta = self.reasoning.lock().unwrap_or_else(|e| e.into_inner()).get(&req.model).cloned();
        request_body(req, catalog::reasoning_field(req.effort, meta.as_ref()))
    }

    fn headers(&self, key: &ApiKey) -> Vec<(&'static str, String)> {
        vec![
            ("Authorization", format!("Bearer {}", key.expose_for_authorization_header())),
            ("HTTP-Referer", REFERER.to_string()),
            ("X-Title", TITLE.to_string()),
            ("Accept", "text/event-stream".to_string()),
        ]
    }

    fn explain_status(&self, status: u16, model: &str, body: &str) -> ProviderError {
        let said = upstream_message(body);
        let detail = match status {
            400 => format!("OpenRouter rejected the request (HTTP 400) for `{model}`. {said}"),
            401 => format!(
                "OpenRouter refused the API key (HTTP 401). Check it at https://openrouter.ai/keys \
                 and set it again in the Library. {said}"
            ),
            402 => format!(
                "OpenRouter reports no credit (HTTP 402). Add credit at https://openrouter.ai/credits \
                 or choose a `:free` model. {said}"
            ),
            403 => format!("OpenRouter refused the request (HTTP 403), usually moderation or model access. {said}"),
            404 => format!(
                "OpenRouter has no model `{model}` (HTTP 404). Browse https://openrouter.ai/models. {said}"
            ),
            413 => format!("The request is larger than `{model}` accepts (HTTP 413). {said}"),
            _ => format!("OpenRouter returned HTTP {status} for `{model}`. {said}"),
        };
        ProviderError::from_status(status, self.redact(detail.trim_end()))
    }
}

/// Messages from the worker thread.
enum Msg {
    Status { status: u16, retry_after: Option<String>, body: Option<String> },
    Data(String),
    Done,
    Eof,
    Fail { detail: String, transient: bool },
}

fn spawn_attempt(
    transport: Arc<dyn Transport>,
    headers: Vec<(&'static str, String)>,
    body: Arc<Vec<u8>>,
    cancel: CancelToken,
) -> Receiver<Msg> {
    let (tx, rx) = crossbeam_channel::unbounded();
    let spawned = thread::Builder::new().name("openrouter-stream".into()).spawn(move || {
        let borrowed: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let resp = transport.post("/chat/completions", &borrowed, &body);
        drop(borrowed);
        drop(headers);
        let mut resp = match resp {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(Msg::Fail { detail: e.to_string(), transient: e.is_transient() });
                return;
            }
        };
        if !(200..300).contains(&resp.status) {
            let body = resp.read_capped(2_000);
            let _ = tx.send(Msg::Status { status: resp.status, retry_after: resp.retry_after.clone(), body: Some(body) });
            return;
        }
        if tx.send(Msg::Status { status: resp.status, retry_after: None, body: None }).is_err() {
            return;
        }
        let mut sse = SseStream::new(&mut resp.body);
        loop {
            if cancel.is_cancelled() {
                return;
            }
            let msg = match sse.next_frame() {
                None => Msg::Eof,
                Some(Ok(Frame::Done)) => Msg::Done,
                Some(Ok(Frame::Data(d))) => Msg::Data(d),
                Some(Err(e)) => Msg::Fail { detail: e.to_string(), transient: true },
            };
            let last = !matches!(msg, Msg::Data(_));
            if tx.send(msg).is_err() || last {
                return;
            }
        }
    });
    if let Err(e) = spawned {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let _ = tx.send(Msg::Fail { detail: format!("could not start the stream thread: {e}"), transient: false });
        return rx;
    }
    rx
}

/// Next message, or `None` once cancelled. A vanished worker reads as a failure.
fn recv(rx: &Receiver<Msg>, cancel: &CancelToken) -> Option<Msg> {
    loop {
        if cancel.is_cancelled() {
            return None;
        }
        match rx.recv_timeout(POLL) {
            Ok(m) => return Some(m),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Some(Msg::Fail { detail: "the stream thread ended unexpectedly".into(), transient: false })
            }
        }
    }
}

/// Sleeps up to `d`, returning false if cancelled first.
fn sleep_cancellable(d: Duration, cancel: &CancelToken) -> bool {
    let until = Instant::now() + d;
    while Instant::now() < until {
        if cancel.is_cancelled() {
            return false;
        }
        thread::sleep(POLL.min(until.saturating_duration_since(Instant::now())));
    }
    !cancel.is_cancelled()
}

impl Provider for OpenRouterProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenRouter
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(self.catalog()?.into_iter().map(|e| e.info).collect())
    }

    fn stream(
        &self,
        req: &ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<Completion, ProviderError> {
        let Some(key) = &self.key else {
            return Err(ProviderError::auth(
                "no OpenRouter API key: add one in the Library or set OPENROUTER_API_KEY",
            ));
        };
        let body = serde_json::to_vec(&self.build_body(req))
            .map_err(|e| ProviderError::bad_request(format!("the request would not serialize: {e}")))?;
        let body = Arc::new(body);
        let cancelled = ProviderError::cancelled;

        let mut last: Option<ProviderError> = None;
        for attempt in 1..=retry::MAX_ATTEMPTS {
            if cancel.is_cancelled() {
                return Err(cancelled());
            }
            let rx = spawn_attempt(self.transport.clone(), self.headers(key), body.clone(), cancel.clone());
            let wait = match recv(&rx, cancel).ok_or_else(cancelled)? {
                Msg::Fail { detail, transient } => {
                    let e = self.err(
                        if transient { ProviderErrorKind::Network } else { ProviderErrorKind::BadRequest },
                        format!("{detail} (attempt {attempt} of {})", retry::MAX_ATTEMPTS),
                    );
                    if !transient {
                        return Err(e);
                    }
                    last = Some(e);
                    retry::backoff(attempt, None)
                }
                Msg::Status { status, retry_after, body } if !(200..300).contains(&status) => {
                    let body = body.unwrap_or_default();
                    let e = self.explain_status(status, &req.model, &body);
                    match retry::disposition(status) {
                        Disposition::Retry => {
                            last = Some(e);
                            retry::backoff(attempt, retry_after.as_deref())
                        }
                        _ => return Err(e),
                    }
                }
                Msg::Status { .. } => return self.consume(&rx, cancel, on),
                Msg::Data(_) | Msg::Done | Msg::Eof => {
                    return Err(ProviderError::server("the stream began without a status"))
                }
            };
            if attempt < retry::MAX_ATTEMPTS && !sleep_cancellable(wait, cancel) {
                return Err(cancelled());
            }
        }
        Err(last.unwrap_or_else(|| ProviderError::network("no attempt was made")))
    }
}

impl OpenRouterProvider {
    fn consume(
        &self,
        rx: &Receiver<Msg>,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<Completion, ProviderError> {
        let mut acc = Accumulator::new(on);
        let mut finished = false;
        loop {
            match recv(rx, cancel).ok_or_else(ProviderError::cancelled)? {
                Msg::Data(payload) => {
                    let value: Value = serde_json::from_str(&payload).map_err(|e| {
                        self.err(
                            ProviderErrorKind::Server,
                            format!(
                                "malformed SSE payload from OpenRouter: {e}; began: {}",
                                payload.chars().take(120).collect::<String>()
                            ),
                        )
                    })?;
                    if value
                        .get("choices")
                        .and_then(Value::as_array)
                        .and_then(|a| a.first())
                        .and_then(|c| c.get("finish_reason"))
                        .is_some_and(|f| !f.is_null())
                    {
                        finished = true;
                    }
                    acc.chunk(&value).map_err(|e| ProviderError { detail: self.redact(&e.detail), ..e })?;
                }
                Msg::Done => break,
                Msg::Eof if finished => break,
                Msg::Eof => {
                    return Err(ProviderError::network("OpenRouter closed the stream before it finished"))
                }
                Msg::Fail { detail, .. } => {
                    return Err(self.err(ProviderErrorKind::Network, format!("the stream broke: {detail}")))
                }
                Msg::Status { .. } => {}
            }
        }
        Ok(acc.finish())
    }
}

fn upstream_message(body: &str) -> String {
    let msg = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("error")?.get("message")?.as_str().map(str::to_string));
    match msg {
        Some(m) => format!("Upstream said: {}", m.chars().take(300).collect::<String>()),
        None => {
            let line = first_line(body);
            if line.is_empty() {
                String::new()
            } else {
                format!("Upstream said: {line}")
            }
        }
    }
}

fn first_line(body: &str) -> String {
    body.trim().lines().next().unwrap_or("").chars().take(300).collect()
}
