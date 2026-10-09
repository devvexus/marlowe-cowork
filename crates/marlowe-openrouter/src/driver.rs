//! The driver. Speaks OpenRouter's OpenAI-compatible `/chat/completions`, returns a [`ModelStep`].
//!
//! # What is reused from the Ollama adapter, and why reuse is the safer choice here
//!
//! [`marlowe_provider::ollama::parse_step`] turns a provider message into a `ModelStep`, and it is
//! where the four **loop-control** tools are routed — the seam whose first real end-to-end run
//! cost 155 seconds because `done` went to a tool host with no executor for it. Writing a second
//! `parse_step` here would give the two providers two ideas of what `ask` means, and the one that
//! drifted would be the hosted one nobody runs by default. So the wire shapes are normalised into
//! Ollama's message shape and handed to the existing function.
//!
//! `ThinkSplitter` is reused for the same reason: a `</think>` reaching the screen is the loudest
//! possible statement that the channel split was wrong, and that logic should exist once.
//!
//! # The three wire differences that are not cosmetic
//!
//! | | Ollama `/api/chat` | OpenRouter `/chat/completions` |
//! |---|---|---|
//! | framing | NDJSON, ends at EOF | **SSE**, ends at `data: [DONE]` |
//! | `tool_calls[].function.arguments` | a JSON **object** | a JSON **string**, delivered in fragments across chunks and keyed by `index` |
//! | output cap | `options.num_predict` | `max_tokens` |
//!
//! The middle one is the one that bites. An OpenAI-shaped stream sends `{"index":0,"function":
//! {"arguments":"{\"pa"}}` and then `{"index":0,"function":{"arguments":"th\":\"a.md\"}"}}`.
//! Treating each chunk as a complete call produces a call with truncated arguments — which is
//! well-formed against the schema and useless, the failure `param_description` already warns
//! about in another form.

use std::collections::BTreeMap;
use std::io::BufRead;

use marlowe_loop::{
    CallLimits, ContextView, DegradedPath, ModelCall, ModelDriver, ModelStep, ProviderError,
    Usage,
};
use marlowe_provider::ModelCapability;
use marlowe_tools::{ExposedSet, ToolRegistry};

use crate::attribution::{CallAttribution, RunAttribution};
use crate::retry::{self, Disposition};
use crate::secret::ApiKey;
use crate::transport::Transport;

/// A sink for the outbound request body. **`--dev` only**, and the same contract as the Ollama
/// adapter's: this is the bytes the *running process* sent, which a test on a constructed body
/// cannot see. See `marlowe_provider::ollama::RequestSink`.
pub type RequestSink = Box<dyn FnMut(&serde_json::Value) + Send>;

/// A sink for raw SSE payloads, before interpretation. `--dev` only.
pub type RawFrameSink = Box<dyn FnMut(&serde_json::Value) + Send>;

/// A sink for per-call attribution. **This is how the run record learns which upstream answered.**
pub type AttributionSink = Box<dyn FnMut(&CallAttribution) + Send>;

/// The context window used when none is given.
///
/// **Declared and always sent**, for the reason `DEFAULT_CONTEXT_TOKENS` is: an omitted window is
/// a window somebody else chose. Larger than the local default because the models this path
/// exists to reach have larger ones, and because a benchmark case that does not fit is a case
/// scored as a failure of the harness.
pub const DEFAULT_CONTEXT_TOKENS: u32 = 128_000;

/// **No default model, and that is deliberate.**
///
/// There is no OpenRouter slug this project has measured, and picking one would put a model name
/// in the code that a future reader would take for a recommendation. `--openrouter-model` is
/// required whenever the provider is selected; the refusal names the flag.
pub const NO_DEFAULT_MODEL: &str = "";

pub struct OpenRouterDriver {
    transport: Box<dyn Transport>,
    key: ApiKey,
    model: String,
    registry: ToolRegistry,
    capability: ModelCapability,
    context_tokens: u32,
    /// **Zero unless overridden.** See `ADR-046` §6: bit-identical reproduction is not available
    /// on this path, and `temperature: 0` is the strongest thing that is.
    temperature: f32,
    seed: Option<u64>,
    /// `provider.order`, with `allow_fallbacks: false`. Pins the upstream so two runs are
    /// comparable — the one control OpenRouter offers that actually bears on reproducibility.
    pin_upstream: Option<Vec<String>>,
    request_dump: Option<RequestSink>,
    raw_frames: Option<RawFrameSink>,
    attribution_sink: Option<AttributionSink>,
    attribution: RunAttribution,
}

impl OpenRouterDriver {
    pub fn new(
        transport: Box<dyn Transport>,
        key: ApiKey,
        model: &str,
        registry: ToolRegistry,
    ) -> Self {
        Self {
            transport,
            key,
            model: model.to_string(),
            registry,
            // **Never `default_capability()`.** That struct carries `qwen3.5:9b`'s measured
            // 12/12 from 2026-08-08, and handing it back for a hosted model would report one
            // model's number under another's name. Nothing about any OpenRouter model has been
            // measured on this machine and the disclosure says exactly that.
            capability: ModelCapability::unmeasured(model),
            context_tokens: DEFAULT_CONTEXT_TOKENS,
            temperature: 0.0,
            seed: None,
            pin_upstream: None,
            request_dump: None,
            raw_frames: None,
            attribution_sink: None,
            attribution: RunAttribution::default(),
        }
    }

    pub fn with_context_tokens(mut self, tokens: u32) -> Self {
        self.context_tokens = tokens;
        self
    }

    pub fn with_temperature(mut self, t: f32) -> Self {
        self.temperature = t;
        self
    }

    pub fn with_seed(mut self, seed: Option<u64>) -> Self {
        self.seed = seed;
        self
    }

    /// Pin the serving upstream. `provider.order` plus `allow_fallbacks: false`.
    pub fn with_pinned_upstream(mut self, order: Vec<String>) -> Self {
        self.pin_upstream = (!order.is_empty()).then_some(order);
        self
    }

    pub fn with_request_dump(mut self, sink: RequestSink) -> Self {
        self.request_dump = Some(sink);
        self
    }

    pub fn with_raw_frames(mut self, sink: RawFrameSink) -> Self {
        self.raw_frames = Some(sink);
        self
    }

    pub fn with_attribution_sink(mut self, sink: AttributionSink) -> Self {
        self.attribution_sink = Some(sink);
        self
    }

    pub fn capability(&self) -> &ModelCapability {
        &self.capability
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn context_tokens(&self) -> u32 {
        self.context_tokens
    }

    /// Everything this driver has been told about who answered. Read after a run.
    pub fn attribution(&self) -> &RunAttribution {
        &self.attribution
    }

    /// **Every string that leaves this crate goes through here.**
    ///
    /// The `ApiKey` type stops *us* printing the key. It cannot stop an upstream quoting the
    /// credential it was sent back inside an error body — which then travels into a
    /// `ProviderError`, onto the screen, and into the journal. One constructor, so a new error
    /// site cannot skip the redaction by forgetting about it.
    fn provider_error(&self, detail: impl std::fmt::Display, retriable: bool) -> ProviderError {
        ProviderError { detail: self.key.redact(&detail.to_string()), retriable }
    }

    /// The exposed tools, in OpenAI's function schema.
    ///
    /// **One definition, in [`marlowe_provider::wire::openai_tool_schema`].** This was a verbatim
    /// copy of the Ollama adapter's, and the copies had already drifted: only that one honoured a
    /// parameter's own `description`, so every hosted request described `run`'s `task` as "text"
    /// while the local one described what it is for. ADR-060 §6 collapsed them.
    fn tool_schema(&self, exposed: &ExposedSet) -> serde_json::Value {
        marlowe_provider::wire::openai_tool_schema(&self.registry, exposed)
    }

    /// The outbound request, built and returned rather than sent.
    ///
    /// **Extracted for the same reason the Ollama adapter extracts its own**: the only check worth
    /// having on "does the persona reach the model" and "does the key reach the model" is one that
    /// looks at the bytes that go to the provider.
    pub fn request_body(
        &self,
        view: &ContextView,
        tools: &ExposedSet,
        limits: CallLimits,
    ) -> serde_json::Value {
        // **Built by [`marlowe_provider::wire::openai_messages`], which is also what the local
        // llama.cpp adapter sends.** The three things it does that a naive builder does not —
        // one system message at position 0, `arguments` as a JSON string with `"type": "function"`,
        // and demoting a `tool` message that answers no call — were each found by a live HTTP 400
        // or 500 on a shipped path, and a second copy is how the next one gets found twice.
        let messages = marlowe_provider::wire::openai_messages(view);

        let mut body = serde_json::json!({
            // **`CallLimits::route` IS NOT READ HERE EITHER, and unlike llama.cpp this one is a
            // gap rather than an impossibility.** OpenRouter genuinely dispatches on this field,
            // so the value sent decides which weights answer — but `OpenRouterDriver` holds a
            // single `model: String` rather than a `Routing`, and giving it one is a daemon-side
            // construction change (`ModelProviderChoice::OpenRouter { model }`) that this session
            // does not make. ADR-046 keeps the path opt-in and unreachable without an explicit
            // `--provider openrouter`, which bounds it; it does not close it.
            //
            // Recorded here rather than left silent because the failure is the readable kind: a
            // later grep for readers of `limits.route` returns a hit in `ollama.rs` and reads as
            // a positive result on a path where nothing honours it.
            "model": self.model,
            "messages": messages,
            "stream": true,
            // **The budget's hard cap, handed to the provider**, capped against the window for
            // the reason the Ollama adapter caps it: 200,000 tokens of output against a window
            // that cannot hold them is unbounded generation with extra steps.
            "max_tokens": limits
                .max_output_tokens
                .min((self.context_tokens / 4) as u64)
                .min(i32::MAX as u64) as i64,
            // ADR-046 §6. The strongest determinism control this path offers.
            "temperature": self.temperature,
            // **Without this there is no cost and no token count in a streamed response**, and
            // the run record's spend would be the zero it has always been.
            "usage": { "include": true },
        });
        if let Some(seed) = self.seed {
            // Honoured by some upstreams and ignored by others. Sent, and the ADR says plainly
            // that sending it is not the same as it working.
            body["seed"] = serde_json::json!(seed);
        }
        // **`tools` is omitted, never sent empty.** The dialect allows the key to be absent or to
        // hold at least one entry; `[]` is neither. Layer 1's `ExposedSet::empty()` renders to
        // exactly that, so *every* quarantined read on this provider was a malformed request.
        if let Some(tools) = marlowe_provider::wire::tools_field(self.tool_schema(tools)) {
            body["tools"] = tools;
        }
        if let Some(order) = &self.pin_upstream {
            body["provider"] = serde_json::json!({
                "order": order,
                // The pin is the point. With fallbacks allowed, `order` is a preference and the
                // run can still be served by something else — which is the unrecorded-upstream
                // problem with a control that looks like it closed it.
                "allow_fallbacks": false,
            });
        }
        body
    }

    /// The headers for one request. **The only place the key is exposed.**
    fn headers<'a>(&'a self, referer: &'a str, title: &'a str) -> Vec<(&'a str, String)> {
        vec![
            (
                "Authorization",
                format!("Bearer {}", self.key.expose_for_authorization_header()),
            ),
            // OpenRouter's attribution headers. Constants, not user input.
            ("HTTP-Referer", referer.to_string()),
            ("X-Title", title.to_string()),
            ("Accept", "text/event-stream".to_string()),
        ]
    }
}

/// What Marlowe identifies itself as to OpenRouter. Constants so nothing user-supplied reaches a
/// header — see `marlowe_net`'s `validate_header`, which refuses control characters, and which
/// exists because these values used to be the obvious place to put a model name.
const REFERER: &str = "https://github.com/marlowe-harness";
const TITLE: &str = "Marlowe";

impl ModelDriver for OpenRouterDriver {
    fn call(
        &mut self,
        view: &ContextView,
        tools: &ExposedSet,
        limits: CallLimits,
    ) -> Result<ModelCall, ProviderError> {
        self.call_streaming(view, tools, limits, &mut |_| {})
    }

    fn streams(&self) -> bool {
        true
    }

    fn call_streaming(
        &mut self,
        view: &ContextView,
        tools: &ExposedSet,
        limits: CallLimits,
        on_delta: &mut dyn FnMut(&str),
    ) -> Result<ModelCall, ProviderError> {
        self.call_streaming_split(view, tools, limits, on_delta, &mut |_, _| {}, &mut |_| {})
    }

    fn call_streaming_split(
        &mut self,
        view: &ContextView,
        tools: &ExposedSet,
        limits: CallLimits,
        on_delta: &mut dyn FnMut(&str),
        on_reasoning: &mut dyn FnMut(&str, u64),
        on_retract: &mut dyn FnMut(u64),
    ) -> Result<ModelCall, ProviderError> {
        let body = self.request_body(view, tools, limits);
        if let Some(sink) = self.request_dump.as_mut() {
            sink(&body);
        }
        let serialized = serde_json::to_vec(&body).map_err(|e| ProviderError {
            detail: format!("the request body would not serialize: {e}"),
            retriable: false,
        })?;

        // **Wall time is measured with `marlowe_net::age::Mark`**, the workspace's fenced
        // monotonic read. It offers `elapsed()` and no way to obtain a time value, so a duration
        // taken here cannot become a timestamp in a payload even by accident — which is what
        // `determinism_guard`'s clock fence is protecting.
        let started = marlowe_net::age::Mark::now();

        let mut attempt: u32 = 0;
        let mut last: Option<ProviderError> = None;
        // LOOP-EXEMPT: bounded HTTP retry, not a driving loop. The bound is `MAX_ATTEMPTS`.
        while attempt < retry::MAX_ATTEMPTS {
            attempt += 1;
            let headers = self.headers(REFERER, TITLE);
            let borrowed: Vec<(&str, &str)> =
                headers.iter().map(|(k, v)| (*k, v.as_str())).collect();

            match self.transport.post("/chat/completions", &borrowed, &serialized) {
                Err(e) => {
                    let retriable = e.is_transient();
                    last = Some(self.provider_error(&e, retriable));
                    if !retriable || attempt >= retry::MAX_ATTEMPTS {
                        break;
                    }
                    std::thread::sleep(retry::backoff(attempt, None));
                }
                Ok(mut response) => match retry::disposition(response.status) {
                    Disposition::Proceed => {
                        let mut acc = Accumulator::new(&self.model);
                        acc.attempts = attempt;
                        let outcome = self.consume(
                            &mut response.body,
                            &mut acc,
                            on_delta,
                            on_reasoning,
                            on_retract,
                        );
                        // Recorded whatever happened: a call that failed halfway still spent
                        // tokens, and a run record that omits it under-reports the cost.
                        acc.call.attempts = attempt;
                        acc.call.upstream_pinned = self.pin_upstream.is_some();
                        let record = acc.call.clone();
                        if let Some(sink) = self.attribution_sink.as_mut() {
                            sink(&record);
                        }
                        self.attribution.push(record);
                        outcome?;
                        let usage = Usage {
                            prompt_tokens: acc.call.prompt_tokens,
                            completion_tokens: acc.call.completion_tokens,
                            micros_usd: acc.call.micros_usd,
                            wall_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                        };
                        return Ok(ModelCall { usage, step: acc.into_step() });
                    }
                    Disposition::Retry => {
                        let detail = response.read_capped(2_000);
                        last = Some(self.provider_error(
                            format_args!(
                                "{} returned HTTP {} (attempt {attempt} of {}): {detail}",
                                self.transport.host(),
                                response.status,
                                retry::MAX_ATTEMPTS
                            ),
                            true,
                        ));
                        if attempt >= retry::MAX_ATTEMPTS {
                            break;
                        }
                        std::thread::sleep(retry::backoff(
                            attempt,
                            response.retry_after.as_deref(),
                        ));
                    }
                    Disposition::Fail => {
                        let detail = response.read_capped(2_000);
                        return Err(self.provider_error(
                            format_args!(
                                "{}",
                                crate::availability::explain_status(
                                    response.status,
                                    &self.model,
                                    &detail
                                )
                            ),
                            false,
                        ));
                    }
                },
            }
        }

        Err(last.unwrap_or_else(|| self.provider_error("no attempt was made", false)))
    }
}

impl OpenRouterDriver {
    /// Fold one SSE stream into `acc`, emitting deltas as they land.
    fn consume(
        &mut self,
        body: &mut dyn BufRead,
        acc: &mut Accumulator,
        on_delta: &mut dyn FnMut(&str),
        on_reasoning: &mut dyn FnMut(&str, u64),
        on_retract: &mut dyn FnMut(u64),
    ) -> Result<(), ProviderError> {
        let mut stream = crate::sse::SseStream::new(body);
        // LOOP-EXEMPT: consuming a response stream, not a driving loop.
        while let Some(frame) = stream.next_frame() {
            let frame = frame.map_err(|e| self.provider_error(e, true))?;
            let payload = match frame {
                crate::sse::Frame::Done => break,
                crate::sse::Frame::Data(d) => d,
            };
            let value: serde_json::Value = match serde_json::from_str(&payload) {
                Ok(v) => v,
                Err(e) => {
                    return Err(self.provider_error(
                        format_args!(
                            "malformed SSE payload from {}: {e}; began: {}",
                            self.transport.host(),
                            payload.chars().take(120).collect::<String>()
                        ),
                        true,
                    ))
                }
            };
            if let Some(sink) = self.raw_frames.as_mut() {
                sink(&value);
            }
            // **An error can arrive INSIDE a 200 stream.** OpenRouter opens the response, then
            // reports a mid-stream upstream failure as an `error` object rather than a status
            // code. Ignoring it produces an empty reply and a run that looks like the model chose
            // to say nothing.
            if let Some(err) = value.get("error") {
                let message = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("(no message)")
                    .to_string();
                return Err(self.provider_error(
                    format_args!("{} reported mid-stream: {message}", self.transport.host()),
                    true,
                ));
            }
            acc.absorb(&value, on_delta, on_reasoning, on_retract);
        }
        acc.finish(on_delta, on_reasoning);
        Ok(())
    }
}

/// One call's state, folded across chunks.
struct Accumulator {
    text: String,
    splitter: marlowe_provider::ThinkSplitter,
    /// `index` → (id, name, argument fragments). **A `BTreeMap`, not a `HashMap`**: the order of
    /// this map decides the order of the emitted tool-call batch, and a batch order that varies
    /// per process is a `repro` hash that varies per process.
    tool_calls: BTreeMap<u64, PartialCall>,
    call: CallAttribution,
    attempts: u32,
    /// Whether reasoning has been seen on its own channel. Once it has, `content` is outside the
    /// think block and may stream — the same rule the Ollama adapter measured.
    closed: bool,
    held: String,
    /// Tokens charged to each channel, and the total charged so far. A remote provider batches
    /// tokens into a chunk as it pleases, so a chunk here is **one charge, not one token** while
    /// the stream runs; `usage` arrives at the end and settles the difference. See
    /// [`Accumulator::finish`].
    reasoning_charged: u64,
    speech_charged: u64,
    held_charged: u64,
    charged: u64,
    /// Which channel was open when the last chunk landed, so the settlement knows where the
    /// tokens the provider counted but never streamed belong.
    last_was_reasoning: bool,
}

#[derive(Default, Clone)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

impl Accumulator {
    fn new(model: &str) -> Self {
        Self {
            text: String::new(),
            splitter: marlowe_provider::ThinkSplitter::new(),
            tool_calls: BTreeMap::new(),
            call: CallAttribution::requested(model),
            attempts: 1,
            reasoning_charged: 0,
            speech_charged: 0,
            held_charged: 0,
            charged: 0,
            last_was_reasoning: true,
            // **Starts CLOSED, unlike the Ollama path, and the difference is measured rather than
            // assumed.** Ollama's chat template emits the opening `<think>` before the stream
            // begins, so content can start inside a block that was never announced. OpenRouter's
            // dialect carries reasoning in its own `reasoning` field and content in `content`; a
            // model that emits a literal `<think>` in content still puts the splitter inside, and
            // a bare `</think>` still retracts, so both backstops remain.
            closed: true,
            held: String::new(),
        }
    }

    fn absorb(
        &mut self,
        value: &serde_json::Value,
        on_delta: &mut dyn FnMut(&str),
        on_reasoning: &mut dyn FnMut(&str, u64),
        on_retract: &mut dyn FnMut(u64),
    ) {
        // ── attribution, from whichever chunk carries it ───────────────────────────────
        //
        // Read from EVERY chunk rather than only the last: OpenRouter puts `provider` and `model`
        // on the first chunk and the usage on the last, and a reader that looked at one of them
        // would silently record half of what arrived.
        // **Through the setters, which sanitise.** These three strings are chosen by a server and
        // end up on a terminal and in a run record; see `attribution::clean_field`.
        if let Some(p) = value.get("provider").and_then(|p| p.as_str()) {
            self.call.set_upstream(p);
        }
        if let Some(m) = value.get("model").and_then(|m| m.as_str()) {
            self.call.set_served_model(m);
        }
        if let Some(id) = value.get("id").and_then(|i| i.as_str()) {
            self.call.set_generation_id(id);
        }
        if let Some(u) = value.get("usage") {
            if let Some(n) = u.get("prompt_tokens").and_then(|v| v.as_u64()) {
                self.call.prompt_tokens = n;
            }
            if let Some(n) = u.get("completion_tokens").and_then(|v| v.as_u64()) {
                self.call.completion_tokens = n;
            }
            // `cost` is in USD credits. **Reported by the provider, never estimated here** — a
            // per-token price table committed to this repo is stale the week it is written, and a
            // wrong cost that looks precise is worse than no cost at all.
            if let Some(c) = u.get("cost").and_then(|v| v.as_f64()) {
                self.call.micros_usd = (c * 1_000_000.0).round().max(0.0) as u64;
            }
        }

        let Some(choice) = value.get("choices").and_then(|c| c.as_array()).and_then(|a| a.first())
        else {
            return;
        };
        // A non-streamed response puts the payload under `message`; a streamed one under `delta`.
        // Accepting both means a `stream: false` fallback needs no second parser.
        let delta = choice.get("delta").or_else(|| choice.get("message"));
        let Some(delta) = delta else { return };

        // ── reasoning ──────────────────────────────────────────────────────────────────
        // OpenRouter normalises every upstream's chain of thought into `reasoning`. Some models
        // also send `reasoning_content`. Both are read; neither is required.
        // One charge per chunk, on the first channel it feeds.
        let mut chunk_charged = false;
        for field in ["reasoning", "reasoning_content"] {
            if let Some(r) = delta.get(field).and_then(|r| r.as_str()) {
                if !r.is_empty() {
                    let charge = u64::from(!chunk_charged);
                    chunk_charged = true;
                    self.charged += charge;
                    self.reasoning_charged += charge;
                    self.last_was_reasoning = true;
                    on_reasoning(r, charge);
                    self.closed = true;
                }
            }
        }

        if let Some(c) = delta.get("content").and_then(|c| c.as_str()) {
            if !c.is_empty() {
                let mut charge = u64::from(!chunk_charged);
                self.charged += charge;
                let split = self.splitter.feed(c);
                if split.retract_speech {
                    if !self.held.is_empty() {
                        let held = std::mem::take(&mut self.held);
                        let owed = std::mem::take(&mut self.held_charged);
                        self.reasoning_charged += owed;
                        on_reasoning(&held, owed);
                    }
                    if !self.text.is_empty() {
                        self.text.clear();
                        let owed = std::mem::take(&mut self.speech_charged);
                        self.reasoning_charged += owed;
                        on_retract(owed);
                    }
                    self.closed = true;
                }
                for seg in &split.segments {
                    match seg {
                        marlowe_provider::Segment::Reasoning(r) => {
                            let owed = std::mem::take(&mut charge);
                            self.reasoning_charged += owed;
                            self.last_was_reasoning = true;
                            on_reasoning(r, owed);
                        }
                        marlowe_provider::Segment::Speech(t) => {
                            if self.closed {
                                self.speech_charged += std::mem::take(&mut charge);
                                self.last_was_reasoning = false;
                                on_delta(t);
                                self.text.push_str(t);
                            } else {
                                self.held_charged += std::mem::take(&mut charge);
                                self.held.push_str(t);
                            }
                        }
                    }
                }
                // Swallowed by the splitter's partial-tag buffer; owed to whichever channel
                // resolves it.
                self.held_charged += charge;
            }
        }

        // ── tool calls, reassembled across chunks ──────────────────────────────────────
        if let Some(calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
            for (position, call) in calls.iter().enumerate() {
                // `index` is how fragments of the SAME call are joined. Absent, the array position
                // is the next-best key — wrong only for a provider that both omits `index` and
                // reorders, which would be broken in a way no key could fix.
                let index = call
                    .get("index")
                    .and_then(|i| i.as_u64())
                    .unwrap_or(position as u64);
                let entry = self.tool_calls.entry(index).or_default();
                if let Some(id) = call.get("id").and_then(|i| i.as_str()) {
                    if !id.is_empty() {
                        entry.id = id.to_string();
                    }
                }
                if let Some(f) = call.get("function") {
                    if let Some(n) = f.get("name").and_then(|n| n.as_str()) {
                        if !n.is_empty() {
                            entry.name.push_str(n);
                        }
                    }
                    if let Some(a) = f.get("arguments").and_then(|a| a.as_str()) {
                        // **Appended, not assigned.** The fragments are the arguments.
                        entry.arguments.push_str(a);
                    }
                }
            }
        }
    }

    /// Flush the buffers, then settle the count against what the provider says it generated.
    ///
    /// # A remote chunk is not a token, and unlike the local engines this one says so
    ///
    /// Every upstream batches tokens into SSE chunks differently, so the running charge here is a
    /// count of chunks and is a **lower bound**. `usage.completion_tokens` is the provider's own
    /// figure and arrives on the last chunk; the difference is what it counted and did not stream
    /// in a shape this could attribute. It goes to the channel that was open, which is
    /// [`ModelDriver::call_streaming_split`]'s rule and the same one both local adapters follow.
    fn finish(&mut self, on_delta: &mut dyn FnMut(&str), on_reasoning: &mut dyn FnMut(&str, u64)) {
        let split = self.splitter.finish();
        for seg in &split.segments {
            match seg {
                // `0`: charged when the chunks that filled the buffer arrived.
                marlowe_provider::Segment::Reasoning(r) => on_reasoning(r, 0),
                marlowe_provider::Segment::Speech(t) => {
                    if self.closed {
                        on_delta(t);
                        self.text.push_str(t);
                    } else {
                        self.held.push_str(t);
                    }
                }
            }
        }
        if !self.held.is_empty() {
            // A call ending in a tool call has not answered, so anything it said along the way is
            // narration and belongs with the reasoning. Same rule as the Ollama adapter's.
            if self.tool_calls.is_empty() && (self.closed || !self.splitter.saw_any_tag()) {
                let held = std::mem::take(&mut self.held);
                self.speech_charged += std::mem::take(&mut self.held_charged);
                on_delta(&held);
                self.text.push_str(&held);
            } else {
                let held = std::mem::take(&mut self.held);
                let owed = std::mem::take(&mut self.held_charged);
                self.reasoning_charged += owed;
                on_reasoning(&held, owed);
            }
        }

        let owed = self.call.completion_tokens.saturating_sub(self.charged);
        if owed > 0 && self.last_was_reasoning {
            self.charged = self.call.completion_tokens;
            self.reasoning_charged += owed;
            on_reasoning("", owed);
        }
    }

    /// Reassemble into the message shape `marlowe_provider::ollama::parse_step` expects, so the
    /// loop-control routing exists once.
    fn into_step(self) -> ModelStep {
        let mut message = serde_json::json!({ "content": self.text });
        let calls: Vec<serde_json::Value> = self
            .tool_calls
            .values()
            .filter(|c| !c.name.is_empty())
            .map(|c| {
                // The arguments arrived as a JSON **string**; `parse_step` wants an object.
                // A fragment that did not reassemble into valid JSON becomes an empty object
                // rather than a guess: a call with invented arguments is a harness action
                // attributed to the model.
                let args: serde_json::Value = serde_json::from_str(c.arguments.trim())
                    .unwrap_or(serde_json::Value::Object(Default::default()));
                serde_json::json!({
                    "id": c.id,
                    "function": { "name": c.name, "arguments": args }
                })
            })
            .collect();
        if !calls.is_empty() {
            message["tool_calls"] = serde_json::Value::Array(calls);
        }
        marlowe_provider::ollama::parse_step(&message)
    }
}

/// Whether an OpenRouter failure should be surfaced as a degraded run rather than a crash.
/// Invariant 4: the flag is on the run, so silent degradation is not representable.
pub fn degraded_path() -> DegradedPath {
    DegradedPath::ModelUnavailable
}
