//! The **engine half of ADR-060's hybrid**: a local `llama-server`, driven over its
//! OpenAI-compatible SSE endpoint. `crate::hybrid` is the half that starts it and falls back.
//!
//! # What this is for, and what it is not
//!
//! Ollama charges a fixed **~225 ms per request** — its own reported `load_duration` on a model
//! that never left VRAM, i.e. the interval from request receipt to `sched.GetRunner` returning —
//! plus ~19% on prompt evaluation. Measured on this machine against Ollama's *own bundled*
//! `llama-server.exe`, pointed at the *same GGUF blob*: warm TTFT **52.0 ms** against **275.1 ms**.
//! The tax is per REQUEST, so a five-call turn pays 1.13 s of it before any thinking happens.
//!
//! **This is not a third provider.** ADR-060 was accepted as the hybrid `ollama/llama.cpp`: Ollama
//! stores, downloads and lists the models, and this serves them off the blob Ollama already holds.
//! Plain `ollama` remains selectable and remains the compiled default.
//!
//! # THIS FILE SUPERVISES NOTHING AND STILL DECIDES EVERYTHING ABOUT WHETHER THE ENGINE IS USABLE
//!
//! The lifetime lives in [`crate::hybrid`] — spawn, health wait, stderr capture, kill on drop,
//! adoption of an orphan. What lives here is the **reading**: [`Availability::probe`], which is
//! the single function every caller asks *"can this serve?"*, and which is therefore the single
//! place that question can be answered wrongly.
//!
//! It was, until 2026-08-27. `Ready` meant *"answered `/health`"*, and a `llama-server` running
//! entirely **on the CPU** answers `/health`, reports a complete `/props`, declares tool support,
//! and beats Ollama on TTFT while being five times slower on a whole turn. See
//! [`Availability::Ready`] for the table of every signal that read green. `Ready` now means
//! **on the GPU**, [`Offload`] is a required field, and a CPU server is
//! [`Availability::RunningOnCpu`] — a state `is_ready()` refuses.
//!
//! # Three things measured about this runtime that the code depends on
//!
//! * **Reasoning arrives on `reasoning_content`, on its own channel, never inside `content`** —
//!   168/168 trials. `ollama::parse_step`'s reader already accepts that spelling, and
//!   `--reasoning-format deepseek` (the default, and pinned in the launch command) is what keeps
//!   it true. `deepseek-legacy` would put `<think>` back into `content`; the fold below is correct
//!   under both, which is why it does not branch on a flag it cannot see.
//! * **`arguments` arrives as a JSON STRING**, fragmented across deltas and keyed by `index` —
//!   where Ollama sends an object. `ollama::parse_step` reads `.as_object()`, which returns `None`
//!   for a string, so an unreassembled call would arrive with the right tool name and **no
//!   arguments** and be refused by the permission layer for "no declared target": a model that
//!   never erred, reported as one that did.
//! * **`--jinja` renders the GGUF's own `tokenizer.chat_template`** and llama.cpp's parser converts
//!   its XML tool-call dialect back to OpenAI-shaped `tool_calls` — 168/168, zero markup leaking
//!   into `content`. It is the default on the bundled build and is pinned anyway, because a default
//!   is a property of a build.

use std::collections::BTreeMap;
use std::io::BufRead;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use marlowe_loop::{
    CallLimits, ContextView, DegradedPath, ModelCall, ModelDriver, ModelStep, ProviderError, Usage,
};
use marlowe_tools::{ExposedSet, ToolRegistry};

use crate::capability::ModelCapability;
use crate::http::{self, HttpError, LocalEndpoint};
use crate::ollama_store::{self, ResolveError, ResolvedModel, Sampling};

/// **11437, and the three numbers below it are the reason.**
///
/// | port | who owns it |
/// |---|---|
/// | 11434 | Ollama. This provider is explicitly a *third* option that must run **beside** it — the whole hybrid is "Ollama is still the model store" |
/// | 11435 | **Marlowe's own daemon** (`marlowe_daemon::DEFAULT_DAEMON_PORT`) |
/// | 11436 | a second daemon, by the convention `control_plane.rs` documents |
/// | 11437 | this |
///
/// # This was 11435, and a live run is what found it
///
/// `marlowe --status --provider llamacpp` reported *"something is listening on
/// http://127.0.0.1:11435 but it is not llama-server"* — and it was right, because what was
/// listening was **Marlowe's own daemon**. Every unit test passed throughout: nothing in the
/// process knows both constants, so nothing could compare them.
///
/// The constant moving is only half the fix. Two constants can drift again, and the collision that
/// matters is between the *configured* ports rather than the compiled defaults — so
/// `Daemon::set_provider` and `marlowe::resolve_provider` refuse a llamacpp port equal to the
/// daemon's own, whatever either has been set to.
pub const LLAMACPP_DEFAULT_PORT: u16 = 11437;

/// The default endpoint. Loopback only, enforced by the type.
pub fn default_endpoint() -> LocalEndpoint {
    LocalEndpoint::new("127.0.0.1", LLAMACPP_DEFAULT_PORT)
        .expect("127.0.0.1 is loopback by construction")
}

/// What is measured about `model` **through this runtime**, which is nothing.
///
/// # The measurement-transfer family, closed at the one place it would happen
///
/// `ollama::capability_for("qwen3.5:9b")` returns *12/12 well-formed, 12/12 correct target,
/// measured 2026-08-08*. That reading was taken through **Ollama's** built-in Go renderer and
/// parser — the manifest's config blob says `"renderer":"qwen3.5","parser":"qwen3.5"` — and
/// llama.cpp uses neither: it renders the GGUF's own Jinja template and parses an XML tool-call
/// dialect back into OpenAI shapes. Same model, same blob, **different system**, and this project
/// has a standing rule about carrying a measurement across that boundary.
///
/// So this is `unmeasured`, always. The reading that would replace it is
/// `marlowe-provider/tests/tool_call_probe.rs` pointed at a live `llama-server`; **that run, not
/// the latency table, is what gates any change of default** (ADR-060 §12).
///
/// **One definition**, called by the driver and by the daemon's `--status`, so the two cannot
/// end up saying different things about the same run.
pub fn capability_for(model: &str) -> ModelCapability {
    ModelCapability::unmeasured(model)
}

/// The `--status` line, with the clause that makes the caveat impossible to miss.
///
/// The number itself is deliberately **not** quoted here: a llamacpp status line containing
/// `12/12` reads as a disclosure however it is framed, which is the confusion this exists to
/// prevent.
pub fn disclosure_for(model: &str) -> String {
    format!(
        "{} · served by llama.cpp, NOT Ollama — any tool-call figure recorded for this model was \
         measured through Ollama's own renderer and parser, and llama.cpp uses neither",
        capability_for(model).disclosure()
    )
}

/// Where a request's sampling comes from.
///
/// **An explicit value, deliberately not a boolean with a default.** `--reranking off` is this
/// project's precedent: a default-off switch forgotten in a target string measures the un-reranked
/// system under a reranked label, and a default-inherit sampler runs a model at temperature 0.8
/// when it was published at 1.0 with `presence_penalty` 1.5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SamplingSource {
    /// Read the model's `.params` layer out of Ollama's store and send it. **The default**, because
    /// it is what Ollama would have applied and this provider's whole premise is "the same model,
    /// faster".
    OllamaParams,
    /// Send nothing and let `llama-server`'s own defaults govern. A **recorded choice** for a user
    /// serving a GGUF that Ollama never pulled — not a fallback, and never reached by omission.
    ServerDefaults,
}

/// Sampling as resolved, with where it came from — so the announcement can say.
#[derive(Debug, Clone, PartialEq)]
pub enum SamplingPlan {
    /// From `<store>/blobs/...`, and here is what it says.
    FromOllamaParams { model: String, sampling: Sampling },
    /// The model resolved and has **no** `.params` layer. A real state (`marlowe-red:9b` is like
    /// this) and a different fact from "we chose not to look".
    ModelHasNoParams { model: String },
    /// The user asked for the server's own defaults.
    ServerDefaults,
}

impl SamplingPlan {
    fn apply_to(&self, body: &mut serde_json::Value) {
        if let SamplingPlan::FromOllamaParams { sampling, .. } = self {
            sampling.apply_to(body);
        }
    }

    /// ADR-029: announced, never inferred. Moving the runtime moves the sampler, and a difference
    /// nobody can read is a difference nobody will find.
    pub fn disclosure(&self) -> String {
        match self {
            SamplingPlan::FromOllamaParams { model, sampling } => format!(
                "sampling from `{model}`'s Ollama .params layer: {}",
                sampling.disclosure()
            ),
            SamplingPlan::ModelHasNoParams { model } => format!(
                "`{model}` has no .params layer, so llama-server's own defaults govern \
                 (temperature 0.8, top_k 40, top_p 0.95, min_p 0.05, presence_penalty 0)"
            ),
            SamplingPlan::ServerDefaults => {
                "sampling NOT taken from Ollama (--llamacpp-sampling server); llama-server's own \
                 defaults govern"
                    .to_string()
            }
        }
    }
}

/// Resolve the sampling for one model, or say why not.
///
/// **A resolution failure is a refusal, not a shrug.** If the store cannot be read we do not know
/// what sampler this model was published with, so continuing would substitute llama.cpp's defaults
/// invisibly. [`SamplingSource::ServerDefaults`] is how a user says they meant that.
pub fn resolve_sampling(
    model: &str,
    source: SamplingSource,
) -> Result<SamplingPlan, ResolveError> {
    match source {
        SamplingSource::ServerDefaults => Ok(SamplingPlan::ServerDefaults),
        SamplingSource::OllamaParams => match ollama_store::resolve(model)?.sampling {
            Some(sampling) => Ok(SamplingPlan::FromOllamaParams {
                model: model.to_string(),
                sampling,
            }),
            None => Ok(SamplingPlan::ModelHasNoParams { model: model.to_string() }),
        },
    }
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// Is it on the GPU? — the reading `Ready` was making without taking
// ─────────────────────────────────────────────────────────────────────────────────────────

/// **The tokens-per-second below which this is not a GPU.**
///
/// # This constant is machine-scoped and saying so is half its value
///
/// Measured on this box (RTX 4080 SUPER, a 9B at `-ngl 99`): **~107 tok/s on the GPU** and
/// **10.0–10.4 tok/s on the CPU**, three runs. `30.0` sits 3x above the CPU reading and 3.5x
/// below the GPU one, which is the widest margin the two populations allow. It is not a
/// published figure and it is not derived from anything; it is a separator between two clusters
/// that were measured **here**, and this project has a standing rule that a measurement does not
/// transfer across a machine. A slower card and a larger model narrow the gap.
///
/// **`MARLOWE_GPU_FLOOR_TOK_S` moves it**, which is the escape hatch for exactly that case — not
/// a tuning knob, and not consulted unless set.
pub const GPU_FLOOR_TOK_PER_S: f64 = 30.0;

/// How many tokens the throughput probe generates.
///
/// One token is not a rate — it is dominated by the first-token path, which is prompt eval, and
/// **prompt eval is the thing that does NOT distinguish CPU from GPU here.** That is the entire
/// defect this reading exists to close: TTFT read 218 ms against Ollama's 426 ms — a 2x *win* —
/// on a server whose total turn time was 5x worse, because TTFT cannot see generation.
const OFFLOAD_PROBE_TOKENS: u32 = 16;

/// Where the forward pass is running. **A reading, never an assumption.**
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Offload {
    /// Generating at or above [`GPU_FLOOR_TOK_PER_S`], or the launch log said so outright.
    Gpu { tok_per_s: f64 },
    /// Generating below the floor. **The server is healthy and it is the wrong answer**: a
    /// `llama-server` on CPU is slower than Ollama on the GPU, so the hybrid's whole reason for
    /// existing is inverted while every other signal reads green.
    Cpu { tok_per_s: f64 },
    /// The reading could not be taken — the generation call failed or the log said nothing.
    ///
    /// **Neither `Gpu` nor `Cpu`, and it must not collapse into either.** Treating it as GPU is
    /// the defect; treating it as CPU would fall back to Ollama on a server that is probably
    /// fine. It is its own state and the caller decides, with the fact stated on screen.
    Unknown,
}

impl Offload {
    pub fn is_gpu(self) -> bool {
        matches!(self, Offload::Gpu { .. })
    }

    /// One clause, for a status line.
    ///
    /// # `tok_per_s` IS NOT ALWAYS A NUMBER, and `{:.0}` renders `NaN` without complaint
    ///
    /// Two of the three signals decide without producing a rate: the startup log says
    /// `offloaded 0/29`, or the VRAM delta says the card lost nothing. Both are legitimate `Cpu`
    /// readings with **no measurement attached**, and they carry `f64::NAN` to say so.
    ///
    /// Formatted naively that reaches the user as *"RUNNING ON CPU (NaN tok/s measured)"* — which
    /// reads as a bug in Marlowe rather than as a fact about the server, and sends the reader to
    /// the wrong place. The clause says how it was established instead.
    pub fn disclosure(self) -> String {
        match self {
            Offload::Gpu { tok_per_s } if tok_per_s.is_finite() => {
                format!("GPU-resident ({tok_per_s:.0} tok/s measured)")
            }
            Offload::Cpu { tok_per_s } if tok_per_s.is_finite() => {
                format!("RUNNING ON CPU ({tok_per_s:.0} tok/s measured)")
            }
            Offload::Gpu { .. } => "GPU-resident (from the server's own startup log)".to_string(),
            Offload::Cpu { .. } => {
                "RUNNING ON CPU (from the server's own startup log, or from the device memory it                  did not take)"
                    .to_string()
            }
            Offload::Unknown => "offload NOT MEASURED on this server".to_string(),
        }
    }
}

/// The floor in force, honouring the override. One definition, read by the probe and by the test
/// that asserts the two measured clusters fall on either side of it.
pub fn gpu_floor_tok_per_s() -> f64 {
    match std::env::var("MARLOWE_GPU_FLOOR_TOK_S") {
        Ok(v) => v.trim().parse::<f64>().unwrap_or(GPU_FLOOR_TOK_PER_S),
        Err(_) => GPU_FLOOR_TOK_PER_S,
    }
}

/// **Whether to take the offload reading now, or carry one already taken.** A required argument on
/// [`Availability::probe`], with no default.
///
/// # Why this is a parameter and not a decision the probe makes for itself
///
/// The reading costs a real generation — ~150 ms on the GPU, ~1.6 s on the CPU. `--status` is on
/// the path the surface hits **every tick**, so measuring there would put a model call in the
/// render loop. And a probe that decided for itself would have to default, and the default that
/// looks harmless — *don't measure, assume it's fine* — is precisely the state that shipped: a
/// `Ready` that could not tell 10 tok/s from 107.
///
/// So the reading is taken **once per server process**, at the moment the engine starts or is
/// adopted, and carried. A carried reading is a fact about a process we still hold, which is a
/// different and weaker claim than a fresh measurement — and the type says which one you have.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OffloadPolicy {
    /// Generate [`OFFLOAD_PROBE_TOKENS`] tokens and time them. Engine start, and `/model`.
    Measure,
    /// Use a reading already taken on this server process. `--status`, every turn.
    Carried(Offload),
}

/// Take the reading: one short generation, timed by the server itself.
///
/// `llama-server`'s `/completion` returns `timings.predicted_per_second`, which is **generation**
/// throughput and not first-token latency. That distinction is the whole point — see
/// [`OFFLOAD_PROBE_TOKENS`].
///
/// **Never panics and never blocks past its timeout.** A server that will not answer this is
/// [`Offload::Unknown`], which is a stated absence rather than a guess in either direction.
pub fn measure_offload(endpoint: &LocalEndpoint) -> Offload {
    let body = serde_json::json!({
        // A one-character prompt: the reading wanted is generation, and a long prompt would put
        // prompt-eval time into a denominator the server computes separately anyway.
        "prompt": " ",
        "n_predict": OFFLOAD_PROBE_TOKENS,
        // Deterministic and cheap. Sampling settings do not move throughput materially, and
        // pinning them keeps two readings on one machine comparable.
        "temperature": 0.0,
        "cache_prompt": false,
        "stream": false,
    });
    // Generous, because the CPU case is the slow one and timing out on it would report `Unknown`
    // for the exact state this is here to detect.
    let response = match http::post_json(endpoint, "/completion", &body, Duration::from_secs(60)) {
        Ok(v) => v,
        Err(_) => return Offload::Unknown,
    };
    let Some(rate) = response
        .pointer("/timings/predicted_per_second")
        .and_then(serde_json::Value::as_f64)
    else {
        return Offload::Unknown;
    };
    if rate >= gpu_floor_tok_per_s() {
        Offload::Gpu { tok_per_s: rate }
    } else {
        Offload::Cpu { tok_per_s: rate }
    }
}

/// Whether a server will actually accept a request carrying a tool definition.
///
/// # `/props`'s `supports_tools` is a DECLARATION and it does not fire for the failure it names
///
/// Measured 2026-08-27 on a `--no-jinja` server: `chat_template_caps.supports_tools` is **`true`**,
/// and every request carrying `tools` comes back **HTTP 500**. (`props-nojinja.json` against
/// `nojinja-tools-refusal.txt`.) So [`Availability::refuses_tools`] — which reads that field — is
/// a control asserted where it is declared rather than where it is enforced, the same family as
/// `inline_threshold_bytes == 0`.
///
/// This is the enforced form: **send a tool and see.** One request, one token, once per server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolSupport {
    /// A request carrying a tool definition was accepted.
    Accepted,
    /// It was refused. Marlowe's every action is a tool call, so this server is unusable.
    Refused { detail: String },
    /// The check could not be made — the server did not answer at all. **Not `Accepted`**: an
    /// unanswered probe is not evidence of anything, and defaulting it to the permissive value is
    /// how the declaration above came to be trusted in the first place.
    Unknown { detail: String },
}

impl ToolSupport {
    pub fn is_refusal(&self) -> bool {
        matches!(self, ToolSupport::Refused { .. })
    }
}

/// Ask the server to accept one tool. **The enforced counterpart to `supports_tools`.**
///
/// `max_tokens: 1` — the answer is irrelevant; the status code is the reading. A `--no-jinja`
/// server answers 500 here while reporting `supports_tools: true` two endpoints over.
pub fn probe_tool_support(endpoint: &LocalEndpoint, model: &str) -> ToolSupport {
    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1,
        "stream": false,
        "tools": [{
            "type": "function",
            "function": {
                "name": "marlowe_tool_support_probe",
                "description": "A probe. Never called.",
                "parameters": {"type": "object", "properties": {}},
            }
        }],
    });
    match http::post_json(endpoint, "/v1/chat/completions", &body, Duration::from_secs(60)) {
        Ok(_) => ToolSupport::Accepted,
        // A 4xx/5xx to a well-formed tools request is the refusal. It is the ONLY reading that
        // distinguishes a `--jinja` server from a `--no-jinja` one, because `/props` does not.
        Err(HttpError::Status { status, body, .. }) => ToolSupport::Refused {
            detail: format!("HTTP {status} to a request carrying one tool: {}",
                body.chars().take(200).collect::<String>()),
        },
        Err(e) => ToolSupport::Unknown { detail: e.to_string() },
    }
}

/// Read the offload out of a spawned server's own startup log. **Corroboration, never the
/// decider.**
///
/// # The string our own remedy told users to grep for is not a contract
///
/// `no usable GPU found, --gpu-layers option will be ignored` was **absent** from one failing
/// launch on this machine and **present** in another, both on the CPU, an hour apart. A check keyed
/// on it reads green on a broken build, which is this project's most-logged shape. So this function
/// is one input among three, it is allowed to say `Cpu` (a definite statement in the log is
/// evidence), and it is **never** allowed to say `Gpu` on the strength of silence — `Unknown` is
/// what an unremarkable log means.
///
/// # It reads the WHOLE log before deciding, and that is not tidiness
///
/// `ggml` probes backends in turn and a failure is not final: it can print
/// `load_backend: failed to load …cuda_v13\ggml-cuda.dll:` and then load `cuda_v12` successfully
/// two lines later. Returning `Cpu` on the first failing line would call a healthy GPU server a
/// CPU one, and the consequence of that is a fallback to Ollama on a server that was working —
/// slower, wrong, and with a confident reason. So the signals are collected and ranked:
///
/// | rank | line | why it wins |
/// |---|---|---|
/// | 1 | `load_tensors: offloaded N/M layers to GPU` | `ggml` stating the outcome. `0/M` is CPU, anything else is GPU. No threshold, no inference |
/// | 2 | `no usable GPU found` | an explicit refusal, when there is no layer line at all |
/// | 3 | a backend load that FAILED with no later success | the `PATH` defect's signature — and the failing line carries **no reason at all**, just a trailing colon |
///
/// Anything else is `Unknown`, and the timed generation settles it.
pub fn offload_from_log(lines: &[String]) -> Offload {
    let mut layers: Option<u32> = None;
    let mut said_no_gpu = false;
    let mut backend_failed = false;
    let mut backend_loaded = false;

    for line in lines {
        let l = line.to_ascii_lowercase();
        let gpu_backend =
            l.contains("cuda") || l.contains("hip") || l.contains("vulkan") || l.contains("metal");

        if l.contains("load_backend") && gpu_backend {
            if l.contains("failed to load") {
                backend_failed = true;
            } else if l.contains("loaded") {
                // A later success cancels an earlier failure: `ggml` probes `cuda_v13` then
                // `cuda_v12`, and only the one that loaded matters.
                backend_loaded = true;
            }
        }
        if l.contains("no usable gpu") || l.contains("no gpu found") {
            said_no_gpu = true;
        }
        // `load_tensors: offloaded 29/29 layers to GPU`. Last one wins — a reload restates it.
        if let Some(rest) = l.split("offloaded ").nth(1) {
            if let Some((frac, _)) = rest.split_once(" layers") {
                if let Some((got, _)) = frac.split_once('/') {
                    if let Ok(n) = got.trim().parse::<u32>() {
                        layers = Some(n);
                    }
                }
            }
        }
    }

    // Rank 1: `ggml` said how many layers it offloaded. Nothing beats that.
    if let Some(n) = layers {
        return if n == 0 {
            Offload::Cpu { tok_per_s: f64::NAN }
        } else {
            Offload::Gpu { tok_per_s: f64::NAN }
        };
    }
    // Rank 2: an explicit refusal.
    if said_no_gpu {
        return Offload::Cpu { tok_per_s: f64::NAN };
    }
    // Rank 3: every GPU backend load failed and none succeeded. This is the `PATH` defect, whose
    // failing line carries an EMPTY reason after the colon — nothing greppable, which is why the
    // signal here is the failure/success pair rather than any particular message.
    if backend_failed && !backend_loaded {
        return Offload::Cpu { tok_per_s: f64::NAN };
    }
    // A backend loaded and said nothing about layers. **Deliberately NOT `Gpu`**: loading is
    // necessary and not sufficient — a backend can initialise and then fail to allocate. The timed
    // generation is what settles it.
    Offload::Unknown
}

/// Free device memory in bytes, from `nvidia-smi`. `None` where there is no such tool.
///
/// # A second definition of a reading `marlowe-memory` already takes, and that is deliberate
///
/// `marlowe_memory::cue::dense::vram::free_bytes` does this too. Depending on that crate from here
/// would drag ONNX Runtime and a 4 GB CUDA library tree into the model provider, which is a far
/// worse trade than one 40-line shell-out. **The two are not required to agree**, and neither is
/// derived from the other: this one is used only as a delta across a spawn, where the absolute
/// value cancels.
fn free_device_bytes() -> Option<u64> {
    // **NO CONSOLE WINDOW ON WINDOWS.** Every one of these is a console-subsystem binary, so the
    // OS gives it a window unless told otherwise, and the window flashes for as long as the child
    // lives. Called on a probe path this reads to the user as *"a bunch of terminals opening and
    // closing"* -- which is exactly what it is, and it is what Matthew reported after the first
    // hybrid switch. Output is captured in every case, so the window shows nothing and costs
    // nothing to suppress. `CREATE_NO_WINDOW` is `0x0800_0000` from `winbase.h`; it is one constant
    // and is not worth a crate.
    let mut cmd = std::process::Command::new("nvidia-smi");
    cmd.args(["--query-gpu=memory.free", "--format=csv,noheader,nounits"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mib: u64 = text.lines().next()?.trim().parse().ok()?;
    Some(mib * 1024 * 1024)
}

/// The smallest allocation that counts as "it loaded onto the card".
///
/// A 9B at `-ngl 99` took **+5,523 MiB** in the working control and **0** in the failing one, so
/// any threshold in between separates them. 1 GiB is chosen low on purpose: the question this
/// answers is *"did it offload at all"*, not *"did it offload everything"*, and a small model on a
/// crowded card is still on the GPU.
const MIN_OFFLOAD_BYTES: u64 = 1024 * 1024 * 1024;

/// Did the card lose memory across the spawn? **A positive reading, and independent of anything
/// the server says about itself.**
///
/// `before` and `after` are [`free_device_bytes`] taken either side of the load. `None` when
/// either reading was unavailable — a machine with no `nvidia-smi` is a real state and inventing
/// an answer for it is how the last three defects here happened.
///
/// **It can be fooled and the direction matters.** Another process allocating during our spawn
/// makes free memory fall for a reason that is not us, which reads as GPU on a CPU server — so
/// this can only *confirm* CPU, never contradict a slow throughput reading. That is exactly how
/// [`crate::hybrid`] uses it.
pub fn offload_from_vram_delta(before: Option<u64>, after: Option<u64>) -> Offload {
    let (Some(before), Some(after)) = (before, after) else { return Offload::Unknown };
    match before.checked_sub(after) {
        Some(taken) if taken >= MIN_OFFLOAD_BYTES => Offload::Gpu { tok_per_s: f64::NAN },
        // It took nothing. On a server that reports `-ngl 99` and answers `/health`, taking no
        // device memory at all is unambiguous: the weights are in system RAM.
        Some(_) => Offload::Cpu { tok_per_s: f64::NAN },
        // Free memory went UP across our own spawn: something else released more than we took.
        // Nothing can be concluded.
        None => Offload::Unknown,
    }
}

/// Read free device memory now. Public so `crate::hybrid` can bracket its spawn.
pub fn device_free_bytes() -> Option<u64> {
    free_device_bytes()
}

/// What is wrong, specifically enough to fix. Mirrors [`crate::Availability`]'s shape because the
/// daemon treats the two the same way: a degraded run with a remedy, never a crash.
#[derive(Debug, Clone, PartialEq)]
pub enum Availability {
    /// Up, serving, tool-capable **and measured to be on the GPU**.
    ///
    /// # `Ready` used to mean "answered `/health`", and that was a `Ready` that lied
    ///
    /// Found live, 2026-08-27, on a `llama-server` that was **running entirely on CPU**:
    ///
    /// | signal | reading | what it should have said |
    /// |---|---|---|
    /// | `/health` | 200 | — it is 200 on CPU too |
    /// | `/props` | complete, `supports_tools: true` | — identical on CPU |
    /// | `Availability::probe` | **`Ready`** | not usable |
    /// | `--status` | *"serving … template reports tool support"* | — every clause true, the whole false |
    /// | the launch log | **zero** occurrences of `no usable GPU` | the string our own remedy told the user to grep for |
    /// | TTFT | **218 ms vs Ollama's 426 ms** | a 2x *win* |
    /// | total turn | **~4,400 ms vs ~800 ms** | 5x worse, same model, same one-word reply |
    ///
    /// TTFT is prompt eval on a cached prefix and a CPU does that acceptably; **generation is
    /// where a CPU collapses, and TTFT cannot see generation.** So the headline metric read green
    /// on a server broken in precisely the way this whole provider exists to fix.
    ///
    /// The field is required and there is no `Default`. A `Ready` that can be constructed without
    /// an offload reading is the state above, and it is reachable by omission — which is this
    /// project's most-logged failure family.
    Ready {
        /// What the server says it loaded, when it names it. `None` is reported rather than
        /// papered over with the configured name — those are two different facts and only one of
        /// them was observed.
        served: Option<String>,
        /// `-c` as the server was launched with it.
        n_ctx: Option<u32>,
        /// `/props`'s `chat_template_caps.supports_tools`, when the build reports it.
        supports_tools: Option<bool>,
        /// Where the forward pass runs. **Never `Cpu` here** — that is [`Availability::RunningOnCpu`].
        offload: Offload,
    },
    /// **Up, healthy, tool-capable, and running the forward pass on the CPU.**
    ///
    /// A distinct state rather than a flag on `Ready`, because the correct response is the
    /// opposite of `Ready`'s: this server is **slower than Ollama**, so continuing on it inverts
    /// the only reason the hybrid exists. `is_ready()` is false, so it cannot be switched to, it
    /// cannot be adopted, and it cannot set `Tier1Runtime::LlamaServerLoaded`.
    ///
    /// The usual cause on this machine is not a missing DLL: **the card holds one 9B at a time.**
    /// 16,376 MiB total, ~8,517 in use, ~7,529 free, and `-ngl 99` on a 9B wants ~9.5 GB. A
    /// leftover `llama-server` from a previous run is enough — and the second one **does not
    /// fail**, it quietly runs on the CPU.
    RunningOnCpu { endpoint: String, reading: Offload },
    /// Nothing is listening. The remedy is the whole launch command.
    EndpointDown { endpoint: String, launch: String },
    /// `llama-server` answers 503 with `{"status":"loading model"}` until the weights are in.
    Loading { endpoint: String },
    /// Something answered and it is not `llama-server`.
    NotLlamaServer { endpoint: String, detail: String },
    /// **The `num_ctx` defect in its new home.** The server's context is a LAUNCH FLAG here, not a
    /// per-request field, so a daemon packing 32,768 tokens into a server launched at 8,192
    /// truncates history, injected memory and tool results with nothing observing it. That is
    /// verbatim the defect `OllamaDriver::context_tokens` exists to prevent, arriving by a route
    /// where the driver cannot fix it — so it refuses instead.
    ContextTooSmall { endpoint: String, server_n_ctx: u32, needed: u32 },
}

impl Availability {
    pub fn is_ready(&self) -> bool {
        matches!(self, Availability::Ready { .. })
    }

    /// **`Some(false)` means every tool call on this server will silently fail**, and it is the one
    /// reading that must stop a switch rather than merely be printed.
    ///
    /// `/props` reports `chat_template_caps.supports_tools`. A template that cannot render a tool
    /// block does not error — it renders the conversation without one, the model narrates instead
    /// of acting, and the whole thing presents as *the model got worse* rather than as *the prompt
    /// changed*. ADR-060 §4 names that failure and says the alarming part is that it is silent.
    ///
    /// `None` is **unknown, not false**: a build that does not report the field is not a build that
    /// cannot do tools, and refusing on an absent reading would be inventing evidence.
    pub fn refuses_tools(&self) -> bool {
        matches!(self, Availability::Ready { supports_tools: Some(false), .. })
    }

    /// The offload reading, where one exists.
    pub fn offload(&self) -> Option<Offload> {
        match self {
            Availability::Ready { offload, .. } => Some(*offload),
            Availability::RunningOnCpu { reading, .. } => Some(*reading),
            _ => None,
        }
    }

    /// One line for the startup announcement: what the server says it is serving, in how big a
    /// window, **and on which processor**. Reported, never parsed.
    pub fn disclosure(&self) -> String {
        match self {
            Availability::Ready { served, n_ctx, supports_tools, offload } => {
                let name = served
                    .clone()
                    // Reported rather than substituted with the configured name: those are two
                    // different facts and only one of them was observed.
                    .unwrap_or_else(|| "(the server did not name what it loaded)".to_string());
                let window = n_ctx
                    .map(|n| format!("{n} tokens"))
                    .unwrap_or_else(|| "an unreported window".to_string());
                let tools = match supports_tools {
                    Some(true) => "template reports tool support",
                    Some(false) => "TEMPLATE REPORTS NO TOOL SUPPORT",
                    None => "template tool support not reported by this build",
                };
                // **The offload clause is in the disclosure, not only in the failure path.** The
                // live defect's `--status` line was *"serving … template reports tool support"* —
                // every clause true, and it named nothing that could have revealed a CPU server.
                format!("serving {name} in {window}; {tools}; {}", offload.disclosure())
            }
            other => other.remedy(),
        }
    }

    /// The line the user sees. **Names the remedy**, because a degraded state a user cannot act on
    /// is a crash with better manners.
    pub fn remedy(&self) -> String {
        match self {
            Availability::Ready { .. } => "ready".into(),
            Availability::RunningOnCpu { endpoint, reading } => format!(
                "the llama-server on {endpoint} is healthy and is running the model ON THE CPU \
                 ({}). It is SLOWER than Ollama, so the reason for using it is inverted. The \
                 usual cause on a 16 GB card is that the GPU is already full — a 9B at -ngl 99 \
                 wants ~9.5 GB, and a leftover llama-server from an earlier run is enough. Stop \
                 any other llama-server and any resident Ollama model, then try again",
                reading.disclosure()
            ),
            Availability::EndpointDown { endpoint, launch } => format!(
                "no model available — nothing is listening on {endpoint}. Start one with:\n{launch}"
            ),
            Availability::Loading { endpoint } => format!(
                "the llama-server on {endpoint} is still loading the model. It took 1.6–2.4 s from \
                 a warm page cache when this was measured; try again"
            ),
            Availability::NotLlamaServer { endpoint, detail } => format!(
                "no model available — something is listening on {endpoint} but it is not \
                 llama-server ({detail})"
            ),
            Availability::ContextTooSmall { endpoint, server_n_ctx, needed } => format!(
                "the llama-server on {endpoint} was launched with -c {server_n_ctx} and this \
                 daemon packs {needed} tokens of context. The window is a LAUNCH FLAG here, so it \
                 cannot be set per request: relaunch with `-c {needed}`, or start marlowe with \
                 `--context {server_n_ctx}`. Running as-is would truncate history, injected memory \
                 and tool results with nothing reporting it"
            ),
        }
    }

    /// §B5's status band carries this, in amber. Invariant 4: the flag is on the run, so silent
    /// degradation is not representable.
    pub fn degraded_path(&self) -> Option<DegradedPath> {
        (!self.is_ready()).then_some(DegradedPath::ModelUnavailable)
    }

    /// Ask the endpoint what it is. **No model call beyond the offload reading, no network beyond
    /// loopback.**
    ///
    /// `model` and `context_tokens` are here only so a down endpoint can be answered with the
    /// command that would fix it. `offload` decides whether the generation reading is taken now or
    /// carried from an earlier one — see [`OffloadPolicy`], and note that it has no default on
    /// purpose.
    pub fn probe(
        endpoint: &LocalEndpoint,
        model: &str,
        context_tokens: u32,
        offload: OffloadPolicy,
    ) -> Self {
        // `/health` is 200 only once the weights are IN. That is a positive reading of a state the
        // VRAM reserve then depends on — see `Tier1Runtime::LlamaServerLoaded`.
        match http::get_json(endpoint, "/health", Duration::from_secs(5)) {
            Ok(_) => {}
            Err(HttpError::Unreachable { .. }) => {
                return Availability::EndpointDown {
                    endpoint: endpoint.to_string(),
                    launch: launch_hint(model, endpoint, context_tokens),
                }
            }
            Err(HttpError::Status { status: 503, .. }) => {
                return Availability::Loading { endpoint: endpoint.to_string() }
            }
            Err(e) => {
                return Availability::NotLlamaServer {
                    endpoint: endpoint.to_string(),
                    detail: e.to_string(),
                }
            }
        }

        let props = match http::get_json(endpoint, "/props", Duration::from_secs(5)) {
            Ok(v) => v,
            Err(e) => {
                return Availability::NotLlamaServer {
                    endpoint: endpoint.to_string(),
                    detail: format!("/props did not answer: {e}"),
                }
            }
        };
        // **A llama-server-specific key, not merely valid JSON.** Anything can answer 200 on
        // `/health`; only this server reports a chat template and a generation-settings block, and
        // the whole point of this arm is that it is NOT Ollama.
        if props.get("default_generation_settings").is_none() && props.get("chat_template").is_none()
        {
            return Availability::NotLlamaServer {
                endpoint: endpoint.to_string(),
                detail: "/props has neither `default_generation_settings` nor `chat_template`"
                    .into(),
            };
        }

        let n_ctx = props
            .pointer("/default_generation_settings/n_ctx")
            .or_else(|| props.pointer("/n_ctx"))
            .and_then(serde_json::Value::as_u64)
            .map(|v| v.min(u32::MAX as u64) as u32);
        if let Some(n) = n_ctx {
            if n < context_tokens {
                return Availability::ContextTooSmall {
                    endpoint: endpoint.to_string(),
                    server_n_ctx: n,
                    needed: context_tokens,
                };
            }
        }

        let supports_tools = props
            .pointer("/chat_template_caps/supports_tools")
            .and_then(serde_json::Value::as_bool);

        // `/v1/models` is the documented OpenAI-compatible listing; llama-server names the loaded
        // model there. Absent, this reports `None` rather than substituting the configured name,
        // which would be a claim about a reading that never happened.
        let served = http::get_json(endpoint, "/v1/models", Duration::from_secs(5))
            .ok()
            .and_then(|v| {
                v.pointer("/data/0/id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .or_else(|| {
                props
                    .pointer("/model_path")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            });

        // **The last check, and the one that used to be missing entirely.** Everything above this
        // line reads identically on a CPU server; see [`Availability::Ready`]'s table.
        let reading = match offload {
            OffloadPolicy::Measure => measure_offload(endpoint),
            OffloadPolicy::Carried(r) => r,
        };
        if matches!(reading, Offload::Cpu { .. }) {
            return Availability::RunningOnCpu {
                endpoint: endpoint.to_string(),
                reading,
            };
        }

        Availability::Ready { served, n_ctx, supports_tools, offload: reading }
    }
}

/// The command that would start a server for `model`, or the resolution failure that stopped it.
///
/// **This is where [`crate::ollama_store`] is READ**, and it is the reason the resolver is not a
/// declared control with no reader. In v1 the user launches the server, so this string is the
/// product: it carries the blob path out of Ollama's manifest and `GGML_BACKEND_PATH` naming the
/// CUDA DLL **by file**, without which the bundled binary prints one warning and then serves at
/// ~10 tok/s on CPU with health 200 and every tool call still correct.
pub fn launch_hint(model: &str, endpoint: &LocalEndpoint, context_tokens: u32) -> String {
    let binary = match ollama_store::server_binary() {
        Ok(b) => b,
        Err(e) => return e.remedy(),
    };
    let resolved: ResolvedModel = match ollama_store::resolve(model) {
        Ok(r) => r,
        Err(e) => return e.remedy(),
    };
    let dll = ollama_store::backend_dll(&binary);
    // The CPU-trap warning lives inside `launch_command`, so no caller can omit it.
    resolved.launch_command(&binary, dll.as_deref(), endpoint.port(), context_tokens)
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// The socket, behind a port
// ─────────────────────────────────────────────────────────────────────────────────────────

/// Where the bytes come from. **A port so the fold can be driven without a server.**
///
/// The alternative is an `#[ignore]`d test against a live `llama-server`, which is a test that
/// silently passes when nothing is running — this project's recorded shape for a measurement that
/// never happened. Replacing the socket and nothing else means a test drives **the real driver,
/// the real body assembly and the real fold**.
pub trait LocalTransport: Send {
    fn open_stream(
        &mut self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<Box<dyn BufRead + Send>, HttpError>;
    /// For error strings. Never parsed.
    fn describe(&self) -> String;
}

/// The production transport: a raw loopback `TcpStream`, no TLS, no redirects.
pub struct HttpTransport {
    endpoint: LocalEndpoint,
    timeout: Duration,
}

impl HttpTransport {
    pub fn new(endpoint: LocalEndpoint, timeout: Duration) -> Self {
        Self { endpoint, timeout }
    }
}

impl LocalTransport for HttpTransport {
    fn open_stream(
        &mut self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<Box<dyn BufRead + Send>, HttpError> {
        http::post_stream(&self.endpoint, path, body, "text/event-stream", self.timeout)
    }

    fn describe(&self) -> String {
        self.endpoint.to_string()
    }
}

/// A transport that replays bytes captured from a real server. **`pub`, not `#[cfg(test)]`**: the
/// integration tests live in `tests/`, which compiles against this crate as an external consumer.
pub struct ScriptedTransport {
    replies: Mutex<Vec<Vec<u8>>>,
    seen: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl ScriptedTransport {
    /// `replies` are consumed front to back, one per call.
    pub fn new(replies: Vec<Vec<u8>>) -> Self {
        Self {
            replies: Mutex::new(replies.into_iter().rev().collect()),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// What the driver actually sent, readable after the call. Asserting on this is asserting on
    /// the bytes the real `request_body` produced, not on a copy built in a test.
    pub fn seen(&self) -> Arc<Mutex<Vec<serde_json::Value>>> {
        Arc::clone(&self.seen)
    }
}

impl LocalTransport for ScriptedTransport {
    fn open_stream(
        &mut self,
        _path: &str,
        body: &serde_json::Value,
    ) -> Result<Box<dyn BufRead + Send>, HttpError> {
        self.seen.lock().expect("scripted transport").push(body.clone());
        let next = self.replies.lock().expect("scripted transport").pop();
        match next {
            Some(bytes) => Ok(Box::new(std::io::BufReader::new(std::io::Cursor::new(bytes)))),
            None => Err(HttpError::Unreachable {
                endpoint: "scripted".into(),
                detail: "the script is exhausted".into(),
            }),
        }
    }

    fn describe(&self) -> String {
        "scripted".into()
    }
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// The driver
// ─────────────────────────────────────────────────────────────────────────────────────────

pub struct LlamaCppDriver {
    transport: Box<dyn LocalTransport>,
    /// Sent as `"model"`. `llama-server` serves whatever it was launched with and does not route on
    /// this, but it is what the request records, so it is the configured name rather than a
    /// placeholder.
    model: String,
    registry: ToolRegistry,
    capability: ModelCapability,
    thinking: bool,
    context_tokens: u32,
    sampling: SamplingPlan,
    raw_frames: Option<crate::ollama::RawFrameSink>,
    request_dump: Option<crate::ollama::RequestSink>,
}

impl LlamaCppDriver {
    /// The production constructor. **Every load-time refusal is here**, so a caller cannot skip
    /// one: `resolve_sampling` reads Ollama's store and returns a named [`ResolveError`] rather
    /// than letting the sampler differ silently.
    pub fn build(
        endpoint: LocalEndpoint,
        model: &str,
        registry: ToolRegistry,
        source: SamplingSource,
    ) -> Result<Self, ResolveError> {
        let sampling = resolve_sampling(model, source)?;
        Ok(Self::with_transport(
            Box::new(HttpTransport::new(endpoint, crate::ollama::DEFAULT_TIMEOUT)),
            model,
            registry,
            sampling,
        ))
    }

    /// The seam a test drives. Takes a resolved [`SamplingPlan`] because the resolution is the
    /// part that touches the machine.
    pub fn with_transport(
        transport: Box<dyn LocalTransport>,
        model: &str,
        registry: ToolRegistry,
        sampling: SamplingPlan,
    ) -> Self {
        Self {
            transport,
            model: model.to_string(),
            registry,
            // Through [`capability_for`], which is the same function `--status` calls.
            capability: capability_for(model),
            thinking: true,
            context_tokens: crate::ollama::DEFAULT_CONTEXT_TOKENS,
            sampling,
            raw_frames: None,
            request_dump: None,
        }
    }

    /// What is measured about this model through this runtime. See [`capability_for`], which
    /// is the same function `--status` calls -- so the driver and the status line cannot end
    /// up saying different things about the same run.
    pub fn capability(&self) -> &ModelCapability {
        &self.capability
    }

    pub fn sampling(&self) -> &SamplingPlan {
        &self.sampling
    }

    pub fn with_context_tokens(mut self, tokens: u32) -> Self {
        self.context_tokens = tokens;
        self
    }

    pub fn context_tokens(&self) -> u32 {
        self.context_tokens
    }

    pub fn with_thinking(mut self, on: bool) -> Self {
        self.thinking = on;
        self
    }

    pub fn with_raw_frames(mut self, sink: crate::ollama::RawFrameSink) -> Self {
        self.raw_frames = Some(sink);
        self
    }

    pub fn with_request_dump(mut self, sink: crate::ollama::RequestSink) -> Self {
        self.request_dump = Some(sink);
        self
    }

    /// The outbound request, built and returned rather than sent — same reason the other two
    /// adapters extract theirs: the only check worth having is one that looks at the bytes.
    ///
    /// # Six differences from the Ollama body, and every one of them is measured
    ///
    /// | this | Ollama | why |
    /// |---|---|---|
    /// | `arguments` a JSON **string** | an object | Ollama 400s on the string, llama.cpp 400s on the object |
    /// | `"type": "function"` on every call | absent | llama.cpp: HTTP 500 `Missing tool call type` on iteration 2 |
    /// | no `tool_name` | present | not in this dialect; `tool_call_id` is the pairing |
    /// | no `thinking` on history | present | not in this dialect |
    /// | no `options.num_ctx` | always sent | the window is a **launch flag** here, checked by `Availability::ContextTooSmall` |
    /// | `chat_template_kwargs.enable_thinking` | `"think": bool` | the Qwen template's own switch |
    ///
    /// **`chat_template_kwargs` is declared and NOT verified against this build.** It is the
    /// documented way to pass a variable into a Jinja chat template, and an unused variable in a
    /// Jinja render is a no-op rather than an error — so sending it cannot break a template that
    /// ignores it. What is *not* established is that this GGUF's template reads it. The reading
    /// that would settle it is one command against a running server:
    /// `POST /apply-template` with `chat_template_kwargs: {"enable_thinking": false}` and again
    /// with `true`, and compare the two renderings — the `<think>` prefill is the discriminator.
    pub fn request_body(
        &self,
        view: &ContextView,
        tools: &ExposedSet,
        limits: CallLimits,
    ) -> serde_json::Value {
        let mut body = serde_json::json!({
            // **`CallLimits::route` IS NOT READ HERE, DELIBERATELY, AND SAYING SO IS THE POINT.**
            // `llama-server` serves whatever weight set it was launched with and does not route
            // on this field (see this driver's own doc comment above, and
            // `ModelProviderChoice::LlamaCpp`'s in the daemon). Reading `limits.route` here would
            // change a string in a request and could not change which weights answer it — so a
            // test asserting `body["model"]` differed by route would be GREEN on a build where
            // every call in the tree ran the orchestrator's weights. That is instance #15
            // manufactured inside the fix for instance #16.
            //
            // A grep for readers of `limits.route` therefore returns one hit, in `ollama.rs`, and
            // this comment is what stops the next reader concluding the field is honoured
            // everywhere. Routing this path at all means one `llama-server` process per role, or
            // a load-time refusal of any `Routing` naming more than one model; ADR-069 §4.3 has
            // the argument and neither is built.
            "model": self.model,
            "messages": crate::wire::openai_messages(view),
            "stream": true,
            // **Without this there is no token count in a streamed response**, so the budget's
            // token accounting would read zero for every call and `Budget::exhausted` — the
            // backstop this project has exercised exactly once — would never fire on tokens.
            "stream_options": { "include_usage": true },
            // **The exact token count, live.** `include_usage` puts the total on the LAST chunk
            // only, which is a figure that arrives after the line it was wanted for has stopped
            // moving. This puts `timings.predicted_n` on EVERY chunk, so the thinking counter is
            // the engine's own running total rather than a count of frames.
            //
            // A count of frames is what the other local engine forces, and it is measurably not a
            // token count — see `OllamaDriver::call_streaming_split`'s header for the numbers.
            // This server gives the real one, so this server reports it.
            //
            // Verified against a `llama-server` on a real blob: 41 of 43 chunks carried `timings`,
            // and `predicted_n` finished on exactly the `completion_tokens` the usage chunk
            // reported. A build that ignores the field degrades to a frame count rather than to
            // zero, which is why `Fold` still charges one per chunk when no `timings` arrive.
            "timings_per_token": true,
            // The budget's hard cap, capped against the window for the reason the other two
            // adapters cap it: 200,000 tokens of output against a window that cannot hold them is
            // unbounded generation with extra steps.
            "max_tokens": limits
                .max_output_tokens
                .min((self.context_tokens / 4) as u64)
                .min(i32::MAX as u64) as i64,
            // Declared, never inherited — the same rule `OllamaDriver::thinking` follows, in this
            // dialect's spelling. See this function's header for what is and is not verified.
            "chat_template_kwargs": { "enable_thinking": self.thinking },
        });

        // **The sampler, sent explicitly.** Ollama applies the model's `.params` layer and this
        // server does not, so omitting these runs the same weights at a different temperature with
        // nothing reporting the change. See `SamplingPlan`.
        self.sampling.apply_to(&mut body);

        // `tools` is omitted, never sent empty: `[]` is a schema violation in this dialect, not
        // "no tools", and layer 1's `ExposedSet::empty()` renders to exactly that.
        if let Some(tools) = crate::wire::tools_field(crate::wire::openai_tool_schema(
            &self.registry,
            tools,
        )) {
            body["tools"] = tools;
        }
        body
    }
}

impl ModelDriver for LlamaCppDriver {
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

        let where_from = self.transport.describe();
        let reader = self
            .transport
            .open_stream("/v1/chat/completions", &body)
            .map_err(|e| ProviderError {
                detail: format!("{e}"),
                // Unreachable is worth a retry; a malformed body is not.
                //
                // **A crash mid-turn and a slow model are NOT distinguishable here**, and v1 does
                // not pretend otherwise: a dead server and a thinking one both end in
                // `Unreachable`, the second after the read timeout. What the supervisor would add
                // is a second signal this driver does not have — `child.try_wait()` — so the
                // degradation could carry an exit code. See ADR-060 §14.10.
                retriable: matches!(e, HttpError::Unreachable { .. }),
            })?;

        let mut stream = crate::sse::SseStream::new(reader);
        let mut fold = Fold::new(!self.thinking);
        // LOOP-EXEMPT: consuming a response stream, not a driving loop.
        while let Some(frame) = stream.next_frame() {
            let frame = frame.map_err(|e| ProviderError {
                detail: format!("{where_from}: {e}"),
                retriable: true,
            })?;
            let payload = match frame {
                crate::sse::Frame::Done => break,
                crate::sse::Frame::Data(d) => d,
            };
            let value: serde_json::Value =
                serde_json::from_str(&payload).map_err(|e| ProviderError {
                    detail: format!(
                        "malformed SSE payload from {where_from}: {e}; began: {}",
                        payload.chars().take(120).collect::<String>()
                    ),
                    retriable: true,
                })?;
            if let Some(sink) = self.raw_frames.as_mut() {
                sink(&value);
            }
            // **An error can arrive INSIDE a 200 stream.** Ignoring it produces an empty reply and
            // a run that looks like the model chose to say nothing.
            if let Some(err) = value.get("error") {
                let message = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("(no message)")
                    .to_string();
                return Err(ProviderError {
                    detail: format!("{where_from} reported mid-stream: {message}"),
                    retriable: true,
                });
            }
            fold.absorb(&value, on_delta, on_reasoning, on_retract);
        }
        fold.finish(on_delta, on_reasoning);

        let usage = Usage {
            prompt_tokens: fold.prompt_tokens,
            completion_tokens: fold.completion_tokens,
            // **Zero, and honestly zero.** A local server costs nothing, and inventing a price
            // here would put a number in the run record that no provider reported.
            micros_usd: 0,
            wall_ms: fold.wall_ms,
        };
        Ok(ModelCall { usage, step: fold.into_step() })
    }
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// The fold
// ─────────────────────────────────────────────────────────────────────────────────────────

#[derive(Default, Clone)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

/// One call's state, folded across SSE chunks.
///
/// **Deliberately not `OpenRouterDriver`'s `Accumulator`**, which is welded to `CallAttribution` —
/// a type about which upstream served a hosted request, a question that does not exist on
/// loopback. What is shared instead is every *decision*: the `closed` rule, the `held` buffer, the
/// `index`-keyed reassembly and `parse_step` itself.
struct Fold {
    text: String,
    splitter: crate::think::ThinkSplitter,
    /// `index` → fragments. **A `BTreeMap`, not a `HashMap`**: this map's order decides the order
    /// of the emitted tool-call batch, and a batch order that varies per process is a `repro` hash
    /// that varies per process.
    tool_calls: BTreeMap<u64, PartialCall>,
    /// Whether the think block is known shut, so `content` may stream as speech.
    ///
    /// **Starts `!thinking`, which is the OLLAMA rule and not OpenRouter's `true`.** The GGUF's
    /// template emits the opening `<think>` as part of the generation prompt, so it never appears
    /// on the wire and content could begin inside a block that was never announced. Under
    /// `--reasoning-format deepseek` (measured, 168/168) the first `reasoning_content` delta flips
    /// this to `true` immediately and nothing is ever held; under `deepseek-legacy` the tags come
    /// back inside `content` and the holding path is correct. **The value is right under both, so
    /// it does not branch on a launch flag this process cannot see.**
    closed: bool,
    held: String,
    prompt_tokens: u64,
    completion_tokens: u64,
    wall_ms: u64,
    /// The engine's running total, from `timings.predicted_n` on each chunk.
    ///
    /// **`0` means the server did not report it**, and the fold falls back to charging one token
    /// per chunk. That fallback is the weaker answer and it is deliberate: a server too old for
    /// `timings_per_token` should give a number that is slightly wrong, not a number that is
    /// zero, because a thinking line frozen at `0 tokens` reads as a hung model.
    predicted_n: u64,
    /// How much of `predicted_n` has been charged to a channel. The difference is what the engine
    /// generated and did not stream, and it is settled in [`Fold::finish`].
    charged: u64,
    /// Which channel was open when the last chunk landed, so an unstreamed token can be charged
    /// to it. `true` is reasoning — the state a call starts in, because a reasoning model emits
    /// its chain of thought before its answer and the opening delimiter is generated before both.
    last_was_reasoning: bool,
    /// Charged to speech since the last retraction, so a `</think>` can move the right number of
    /// tokens into the thinking block along with the text.
    speech_charged: u64,
    /// Charged to the `held` buffer, which has not chosen a channel yet.
    held_charged: u64,
}

impl Fold {
    fn new(closed: bool) -> Self {
        Self {
            text: String::new(),
            splitter: crate::think::ThinkSplitter::new(),
            tool_calls: BTreeMap::new(),
            closed,
            held: String::new(),
            prompt_tokens: 0,
            completion_tokens: 0,
            wall_ms: 0,
            predicted_n: 0,
            charged: 0,
            last_was_reasoning: true,
            speech_charged: 0,
            held_charged: 0,
        }
    }

    /// The tokens this chunk is worth: the engine's own increment where it reports one, and
    /// otherwise one, which is the assumption every other adapter is stuck with.
    ///
    /// Called **once per chunk that carried payload**, before the payload is routed, so a chunk
    /// that merged several tokens pays for all of them on the channel it landed on.
    fn charge(&mut self) -> u64 {
        if self.predicted_n == 0 {
            self.charged += 1;
            return 1;
        }
        let owed = self.predicted_n.saturating_sub(self.charged);
        self.charged = self.predicted_n;
        owed
    }

    fn absorb(
        &mut self,
        value: &serde_json::Value,
        on_delta: &mut dyn FnMut(&str),
        on_reasoning: &mut dyn FnMut(&str, u64),
        on_retract: &mut dyn FnMut(u64),
    ) {
        // Usage arrives on the last chunk when `stream_options.include_usage` is set. `timings` is
        // llama.cpp's own block and is read as well: two readers for one fact is normally the
        // shape this project deletes, but here they are two *sources* and either may be absent —
        // a zero token count would silently disable the budget's only hard cap.
        if let Some(u) = value.get("usage") {
            if let Some(n) = u.get("prompt_tokens").and_then(serde_json::Value::as_u64) {
                self.prompt_tokens = n;
            }
            if let Some(n) = u.get("completion_tokens").and_then(serde_json::Value::as_u64) {
                self.completion_tokens = n;
            }
        }
        if let Some(t) = value.get("timings") {
            if self.prompt_tokens == 0 {
                self.prompt_tokens =
                    t.get("prompt_n").and_then(serde_json::Value::as_u64).unwrap_or(0);
            }
            if self.completion_tokens == 0 {
                self.completion_tokens =
                    t.get("predicted_n").and_then(serde_json::Value::as_u64).unwrap_or(0);
            }
            // **Read on EVERY chunk, not only the last.** `timings_per_token` is what makes this
            // a running total; taking it only from the final chunk would leave the thinking line
            // frozen for the whole call and then jump, which is the behaviour this replaced.
            if let Some(n) = t.get("predicted_n").and_then(serde_json::Value::as_u64) {
                self.predicted_n = self.predicted_n.max(n);
            }
            let ms = |k: &str| t.get(k).and_then(serde_json::Value::as_f64).unwrap_or(0.0);
            let total = ms("prompt_ms") + ms("predicted_ms");
            if total > 0.0 {
                self.wall_ms = total.round().max(0.0) as u64;
            }
        }

        let Some(choice) = value.get("choices").and_then(|c| c.as_array()).and_then(|a| a.first())
        else {
            return;
        };
        // Streamed puts the payload under `delta`, non-streamed under `message`. Accepting both
        // means a `stream: false` fallback needs no second parser.
        let Some(delta) = choice.get("delta").or_else(|| choice.get("message")) else {
            return;
        };

        // **`reasoning_content` is what this server sends** — 168/168 first deltas, and the set of
        // reasoning keys ever observed is exactly that one. `reasoning` is read too because
        // `ollama::parse_step`'s own reader accepts both spellings and a driver that accepted fewer
        // would be the narrower of two answers to one question.
        // One charge per chunk, taken on the first channel this chunk feeds. A chunk carrying
        // both spellings of reasoning is still one chunk and pays once.
        let mut chunk_charged = false;
        for field in ["reasoning_content", "reasoning"] {
            if let Some(r) = delta.get(field).and_then(|r| r.as_str()) {
                if !r.is_empty() {
                    let charge = if chunk_charged { 0 } else { self.charge() };
                    chunk_charged = true;
                    self.last_was_reasoning = true;
                    on_reasoning(r, charge);
                    // The provider is separating the channels, so whatever arrives in `content` is
                    // outside the block.
                    self.closed = true;
                }
            }
        }

        if let Some(c) = delta.get("content").and_then(|c| c.as_str()) {
            if !c.is_empty() {
                let mut charge = if chunk_charged { 0 } else { self.charge() };
                let split = self.splitter.feed(c);
                if split.retract_speech {
                    if !self.held.is_empty() {
                        let held = std::mem::take(&mut self.held);
                        on_reasoning(&held, std::mem::take(&mut self.held_charged));
                    }
                    if !self.text.is_empty() {
                        self.text.clear();
                        // The text moves into the thinking block; so does what it cost.
                        on_retract(std::mem::take(&mut self.speech_charged));
                    }
                    self.closed = true;
                }
                for seg in &split.segments {
                    match seg {
                        crate::think::Segment::Reasoning(r) => {
                            self.last_was_reasoning = true;
                            on_reasoning(r, std::mem::take(&mut charge));
                        }
                        crate::think::Segment::Speech(t) => {
                            if self.closed {
                                self.last_was_reasoning = false;
                                self.speech_charged += std::mem::take(&mut charge);
                                on_delta(t);
                                self.text.push_str(t);
                            } else {
                                self.held_charged += std::mem::take(&mut charge);
                                self.held.push_str(t);
                            }
                        }
                    }
                }
                // Swallowed whole by the splitter's own partial-tag buffer. The token was still
                // generated, and it is owed to whichever channel resolves that buffer.
                self.held_charged += charge;
            }
        }

        // ── tool calls, reassembled across chunks ──────────────────────────────────────
        //
        // The verbatim wire, copied from the socket:
        //
        //     {"index":0,"id":"hxCojJBd…","type":"function","function":{"name":"read","arguments":"{"}}
        //     {"index":0,"function":{"arguments":"\"path\":\""}}
        //     {"index":0,"function":{"arguments":"src"}}   … etc
        //
        // `index` is how fragments of the SAME call are joined, and `arguments` is APPENDED, never
        // assigned — assigning would leave `}` as the whole argument object.
        if let Some(calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
            for (position, call) in calls.iter().enumerate() {
                let index = call
                    .get("index")
                    .and_then(serde_json::Value::as_u64)
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
                        entry.arguments.push_str(a);
                    }
                }
            }
        }
    }

    fn finish(&mut self, on_delta: &mut dyn FnMut(&str), on_reasoning: &mut dyn FnMut(&str, u64)) {
        let split = self.splitter.finish();
        for seg in &split.segments {
            match seg {
                // Charged `0`: these are the splitter's own buffer, and the chunks that filled it
                // paid when they arrived. Their charges are in `held_charged`, settled below.
                crate::think::Segment::Reasoning(r) => on_reasoning(r, 0),
                crate::think::Segment::Speech(t) => {
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
            // A call ending in a tool call has **not answered** — completion is the absence of an
            // action — so anything it said along the way is narration and belongs with the
            // reasoning. Same rule as the other two adapters'.
            if self.tool_calls.is_empty() && (self.closed || !self.splitter.saw_any_tag()) {
                let held = std::mem::take(&mut self.held);
                self.speech_charged += std::mem::take(&mut self.held_charged);
                on_delta(&held);
                self.text.push_str(&held);
            } else {
                let held = std::mem::take(&mut self.held);
                on_reasoning(&held, std::mem::take(&mut self.held_charged));
            }
        }

        // ── WHAT THE ENGINE GENERATED AND NEVER STREAMED ────────────────────────────────
        //
        // `</think>`, the stop token, anything the reasoning parser consumed. `predicted_n` — or
        // `completion_tokens` when the server is too old for `timings_per_token` — counts them;
        // no channel has been charged for them. The rule is the one in
        // [`ModelDriver::call_streaming_split`]: they belong to the channel that was open.
        //
        // Charged as a reasoning chunk with no text when that channel was reasoning, which is the
        // ordinary case — a model finishes thinking, emits its delimiter, then answers. When the
        // answer was last, the tokens are the stop sequence and there is nothing on screen they
        // belong to, so they are dropped rather than added to a thought that had already ended.
        let total = self.predicted_n.max(self.completion_tokens);
        let owed = total.saturating_sub(self.charged);
        if owed > 0 {
            self.charged = total;
            if self.last_was_reasoning {
                on_reasoning("", owed);
            }
        }
    }

    /// Reassemble into the message shape [`crate::ollama::parse_step`] expects, so the four
    /// loop-control tools are routed by ONE definition rather than by a third copy of it.
    fn into_step(self) -> ModelStep {
        let mut message = serde_json::json!({ "content": self.text });
        let calls: Vec<serde_json::Value> = self
            .tool_calls
            .values()
            .filter(|c| !c.name.is_empty())
            .map(|c| {
                // **The arguments arrived as a JSON string and `parse_step` reads `.as_object()`.**
                // Handing the string straight through would produce a call with the correct tool
                // name and NO arguments, refused by the permission layer for "no declared target"
                // — a model that never erred, reported as one that did.
                //
                // A fragment that did not reassemble into valid JSON becomes an **empty object**
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
        crate::ollama::parse_step(&message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_disclosure_never_quotes_a_figure_measured_through_another_runtime() {
        // **Asserted where the sentence is produced**, not where it is rendered. `--status`, the
        // startup announcement and any future surface all call `disclosure_for`; a caveat asserted
        // in one of them would be a property of that call site rather than of the claim.
        //
        // The figure is `12/12` for `qwen3.5:9b`, measured 2026-08-08 through Ollama's own
        // renderer and parser. A llamacpp status line containing those characters reads as a
        // disclosure however carefully it is framed.
        let d = disclosure_for(crate::ollama::DEFAULT_MODEL);
        assert!(!d.contains("12/12"), "{d}");
        assert!(d.contains("NOT MEASURED"), "{d}");
        assert!(d.contains("llama.cpp"), "the caveat must name the runtime: {d}");

        // **The control, and it is what makes the first line mean something.** The Ollama path
        // DOES disclose its measurement, and must keep doing so -- a build that returned
        // `unmeasured` for everything would pass every assertion above while deleting the
        // disclosure ADR-028 requirement 2 exists for.
        assert!(
            crate::ollama::capability_for(crate::ollama::DEFAULT_MODEL)
                .disclosure()
                .contains("12/12"),
            "the Ollama path stopped disclosing its own measured rate"
        );
    }


    /// **The log reader, on the two launches this machine actually produced.**
    ///
    /// # Every case here is a line that was observed, not one that was imagined
    ///
    /// The `PATH` failure prints `load_backend: failed to load …ggml-cuda.dll:` with **nothing
    /// after the colon**, and in one run it printed no `no usable GPU found` at all. That is why
    /// the reader keys on the failure/success pair rather than on any particular message.
    ///
    /// **Without the ranked version this test fails on `a_backend_that_failed_then_succeeded`**,
    /// which the first cut called CPU: it returned on the first failing line, so a `ggml` that
    /// probes `cuda_v13`, fails, then loads `cuda_v12` would have been reported as a CPU server —
    /// a fallback to Ollama on a working GPU, with a confident reason.
    #[test]
    fn the_startup_log_says_cpu_only_when_it_says_so_and_never_says_gpu_by_silence() {
        let l = |s: &str| vec![s.to_string()];

        // Rank 1: ggml stating the outcome. No threshold involved.
        assert!(matches!(
            offload_from_log(&l("load_tensors: offloaded 29/29 layers to GPU")),
            Offload::Gpu { .. }
        ));
        assert!(matches!(
            offload_from_log(&l("load_tensors: offloaded 0/29 layers to GPU")),
            Offload::Cpu { .. }
        ));

        // Rank 2: the explicit refusal, as emitted in the second failing launch.
        assert!(matches!(
            offload_from_log(&l(
                "warning: no usable GPU found, --gpu-layers option will be ignored"
            )),
            Offload::Cpu { .. }
        ));

        // Rank 3: the PATH defect. Note the EMPTY reason after the colon — there is nothing here
        // to grep for except the failure itself.
        assert!(
            matches!(
                offload_from_log(&l(
                    r"E load_backend: failed to load C:\Ollama\lib\ollama\cuda_v12\ggml-cuda.dll:"
                )),
                Offload::Cpu { .. }
            ),
            "the empty-reason backend failure is the PATH defect's only signature"
        );
    }

    /// **A backend that failed and then succeeded is a GPU server**, and calling it CPU would fall
    /// back to Ollama on a machine where nothing was wrong.
    ///
    /// `ggml` ships `cuda_v12` and `cuda_v13` side by side and probes them. A failure is not final.
    /// **This is the case the first version of `offload_from_log` got wrong**, because it returned
    /// on the first matching line rather than reading the whole log.
    #[test]
    fn a_backend_that_failed_then_succeeded_is_not_reported_as_cpu() {
        let log = vec![
            r"load_backend: failed to load C:\O\cuda_v13\ggml-cuda.dll:".to_string(),
            r"load_backend: loaded CUDA backend from C:\O\cuda_v12\ggml-cuda.dll".to_string(),
        ];
        assert!(
            !matches!(offload_from_log(&log), Offload::Cpu { .. }),
            "a later success cancels an earlier probe failure"
        );
        // And it is NOT promoted to `Gpu` either: a backend can initialise and then fail to
        // allocate. Loading is necessary and not sufficient, so the timed generation decides.
        assert_eq!(offload_from_log(&log), Offload::Unknown);
    }

    /// **The negative control, and it is the property the whole design turns on.** An ordinary,
    /// unremarkable log must be `Unknown` — never `Gpu`.
    ///
    /// Ask what the reader would report if a CPU server produced a quiet log: on a build that
    /// treated silence as success it would say GPU, the server would be kept, and the product
    /// would run at a tenth of the speed with every health signal green. That is precisely the
    /// live defect. **Absence of a complaint is not evidence of a GPU.**
    #[test]
    fn a_quiet_log_is_unknown_and_never_gpu() {
        assert_eq!(offload_from_log(&[]), Offload::Unknown);
        assert_eq!(
            offload_from_log(&[
                "llama_model_loader: loaded meta data with 30 key-value pairs".to_string(),
                "llm_load_print_meta: model type = 9B".to_string(),
                "main: server is listening on http://127.0.0.1:11437".to_string(),
            ]),
            Offload::Unknown,
            "silence was read as a GPU, which is the defect this function exists to close"
        );
    }

    /// The VRAM delta, one-directional by construction.
    ///
    /// **It may confirm CPU and must never contradict a slow reading**, because another process
    /// allocating during our spawn makes free memory fall for a reason that is not us. `hybrid.rs`
    /// uses it only after the timed generation has failed to produce a rate.
    #[test]
    fn the_vram_delta_reports_unknown_when_either_reading_is_missing() {
        const GB: u64 = 1024 * 1024 * 1024;
        assert!(matches!(
            offload_from_vram_delta(Some(12 * GB), Some(6 * GB)),
            Offload::Gpu { .. }
        ));
        // Took nothing. On a server launched with `-ngl 99` that answers /health, this is
        // unambiguous: the weights are in system RAM.
        assert!(matches!(
            offload_from_vram_delta(Some(12 * GB), Some(12 * GB)),
            Offload::Cpu { .. }
        ));
        // No `nvidia-smi`. A real state, and inventing an answer for it is how the last three
        // defects here happened.
        assert_eq!(offload_from_vram_delta(None, Some(6 * GB)), Offload::Unknown);
        assert_eq!(offload_from_vram_delta(Some(12 * GB), None), Offload::Unknown);
        // Free memory went UP across our own spawn: something else released more than we took.
        assert_eq!(offload_from_vram_delta(Some(6 * GB), Some(12 * GB)), Offload::Unknown);
    }

    /// **The floor separates the two clusters that were actually measured on this card**, and it is
    /// machine-scoped — which is stated at the constant rather than left for someone to discover.
    #[test]
    fn the_gpu_floor_sits_between_the_measured_cpu_and_gpu_rates() {
        // Measured: 10.0, 10.2, 10.4, 10.6 tok/s on CPU; 107.4 on GPU. A floor outside that gap
        // would classify one of the two populations wrongly.
        assert!(GPU_FLOOR_TOK_PER_S > 10.6, "the CPU readings must fall below the floor");
        assert!(GPU_FLOOR_TOK_PER_S < 107.4, "the GPU reading must fall above it");
        assert_eq!(gpu_floor_tok_per_s(), GPU_FLOOR_TOK_PER_S, "unset, the constant is in force");
    }

    #[test]
    fn a_down_endpoint_degrades_with_a_remedy_rather_than_crashing() {
        // Port 1 is reserved and nothing listens on it. The failure must be a typed state the
        // degradation path can read, and its remedy must name the command — invariant 4.
        // `Measure` even though nothing is listening: the offload reading is never reached,
        // because `/health` fails first. Passing `Carried` here would hide that ordering.
        let a = Availability::probe(
            &LocalEndpoint::new("127.0.0.1", 1).unwrap(),
            "qwen3.5:9b",
            32_768,
            OffloadPolicy::Measure,
        );
        assert!(matches!(a, Availability::EndpointDown { .. }), "{a:?}");
        assert!(!a.is_ready());
        assert_eq!(a.degraded_path(), Some(DegradedPath::ModelUnavailable));
        assert!(
            a.remedy().contains("Start one with"),
            "the remedy must carry the launch command: {}",
            a.remedy()
        );
    }

    #[test]
    fn a_context_smaller_than_the_assembler_packs_is_a_refusal_that_names_both_numbers() {
        // The `num_ctx` defect in its new home: the window is a LAUNCH flag here, so the driver
        // cannot fix it per request and must refuse instead of truncating silently.
        let a = Availability::ContextTooSmall {
            endpoint: "http://127.0.0.1:11435".into(),
            server_n_ctx: 8_192,
            needed: 32_768,
        };
        let r = a.remedy();
        assert!(r.contains("8192") && r.contains("32768"), "{r}");
        assert!(r.contains("-c 32768"), "the remedy must be runnable: {r}");
    }

    #[test]
    fn the_sampler_is_sent_when_it_resolved_and_absent_when_it_did_not() {
        // The control is the second half. Without it, `apply_to` returning early for EVERY plan
        // would pass the first assertion and the sampler would silently be llama.cpp's.
        let mut with = serde_json::json!({});
        SamplingPlan::FromOllamaParams {
            model: "qwen3.5:9b".into(),
            sampling: Sampling {
                temperature: Some(1.0),
                top_k: Some(20),
                presence_penalty: Some(1.5),
                ..Default::default()
            },
        }
        .apply_to(&mut with);
        assert_eq!(with["temperature"], serde_json::json!(1.0));
        assert_eq!(with["top_k"], serde_json::json!(20));
        assert_eq!(with["presence_penalty"], serde_json::json!(1.5));

        let mut without = serde_json::json!({});
        SamplingPlan::ServerDefaults.apply_to(&mut without);
        assert!(without.get("temperature").is_none(), "{without}");
    }

    #[test]
    fn the_two_absent_sampler_states_do_not_read_the_same() {
        // "the model publishes no parameters" and "you told me not to look" are different facts,
        // and a disclosure that rendered them identically would hide which one happened.
        let no_params = SamplingPlan::ModelHasNoParams { model: "marlowe-red:9b".into() };
        let chosen = SamplingPlan::ServerDefaults;
        assert_ne!(no_params.disclosure(), chosen.disclosure());
        assert!(no_params.disclosure().contains("marlowe-red:9b"));
        assert!(chosen.disclosure().contains("--llamacpp-sampling server"));
    }
}
