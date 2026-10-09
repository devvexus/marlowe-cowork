//! **ADR-060's hybrid: Ollama stores the models, `llama-server` runs them, and when it cannot,
//! Ollama runs them and the user is told why.**
//!
//! ```text
//!   ollama pull / ollama list        →  the model store, the downloader, the inventory
//!   ~/.ollama/models/blobs/sha256-…  →  the GGUF
//!   llama-server -m <that blob>      →  the engine that answers          52.0 ms warm TTFT
//!            │
//!            └─ cannot serve ────────→  Ollama answers instead          275.1 ms warm TTFT
//!                                       and the reason is on screen for the whole session
//! ```
//!
//! # This is not a third provider, and the difference is the whole file
//!
//! The first cut of `llamacpp.rs` was a third provider: the *user* launched the server, Marlowe
//! connected to it, and a server that was not up meant every turn degraded with a launch command
//! in it. That is a defensible increment and it is not what was decided. *"Ollama provides the
//! models, llama.cpp runs them. Best of both worlds. Simple on the user."* — so Marlowe starts the
//! server, off the blob Ollama already holds, and the user does nothing.
//!
//! # The fallback is the mitigation, not a nicety
//!
//! This depends on `~/.ollama/models` — an undocumented layout inside another program's cache
//! directory. Ollama can change it in a patch release and there is no contract saying they will
//! not. **Every failure mode of that dependency lands in [`EngineFailure`] and every one of them
//! continues on Ollama.** Marlowe does not stop working because a directory moved.
//!
//! What makes that safe rather than dishonest is the second half of the requirement — *"llama
//! fails fall back to ollama but surface to user why"*. A silent fallback would show the user the
//! provider they picked while serving from the other one, at five times the latency, with no way
//! to find out. So:
//!
//! * the reason is **specific** — which failure, with the server's own log line where there is
//!   one;
//! * it **persists for the session**, in `StatusReport::degraded`, which §B5 renders in amber in
//!   the status band on every frame. Not a flash, not a one-time notice;
//! * it is **not** `DegradedPath::ProviderFailedOver`, whose headline is *"failed over · secondary
//!   provider"* — a claim about a hosted secondary that did not happen here.
//!
//! # A HEALTHY SERVER ON THE CPU IS A FAILURE CASE, AND IT IS THE COMMON ONE
//!
//! `nvidia-smi` on this box: **16,376 MiB total, ~8,517 used, ~7,529 free.** A 9B at `-ngl 99`
//! wants **~9.5 GB**. So **one `llama-server` fits on this card at a time** — and the second one
//! does not fail. It loads, it answers `/health`, it reports tool support, it beats Ollama on
//! TTFT, and it runs the forward pass on the CPU at ~10 tok/s against ~107.
//!
//! A leftover server from a previous run is enough to cause it. That makes "GPU full" the ordinary
//! case rather than an exotic one, and it is why [`start`] treats a CPU reading exactly as it
//! treats a crash: **fall back to Ollama, and say the GPU could not be used.** Keeping a CPU
//! `llama-server` would be wrong twice — slower than the thing it replaced, under a label claiming
//! it is faster.
//!
//! # The latch, and why retrying every turn would be worse
//!
//! Once the engine has fallen back it **stays** fallen back until the user asks again
//! (`/provider ollama/llama.cpp`). The alternative — retry at every turn — makes the band flicker
//! between two engines, spends a spawn attempt and a health wait on each turn that fails, and
//! produces a session where the answer to *"which engine served that reply"* is different for
//! every reply and recorded nowhere. A latched reason is one a person can read ten minutes later
//! and act on.

#![allow(clippy::result_large_err)]

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::deadline::Started;
use crate::http::LocalEndpoint;
use crate::llamacpp::{self, Availability, Offload, OffloadPolicy};
use crate::ollama_store::{self, LaunchPlan, ResolvedModel};

/// How long a spawned server may take to answer `/health` before this gives up.
///
/// **Generous on purpose, and it does not cost what it looks like.** The wait polls every
/// [`POLL_INTERVAL`] and checks the child for exit on each pass, so the failures that are *fast* —
/// CUDA out of memory, a corrupt blob, a bad flag — return in well under a second with the
/// server's own last log line attached. This deadline is only ever paid by a server that is
/// genuinely still loading, and a 5.8 GB blob off a cold page cache is a real 30–60 s. Measured
/// warm on this machine: **1.59 s**.
// **30 s, and the number is chosen against what the INTERFACE can survive, not against what a
// server might conceivably need.**
//
// This wait happens on the thread that answers `/provider`, so every second of it is a second the
// TUI does not repaint. It was 90 s. A start that was going to fail therefore froze the surface for
// a minute and a half before the fallback — which works, and which the user could have had
// immediately — got a chance to fire. Matthew hit exactly that and reported it as "TUI frozen".
//
// What 30 s has to cover, measured on this machine: a warm start is **1.9–2.3 s** end to end, and a
// cold one is bounded by reading a 6.7 GB blob off disk — ~34 s at 200 MB/s, faster on anything
// NVMe. So 30 s covers every warm start with an order of magnitude to spare and most cold ones.
//
// **Being wrong here is cheap and being wrong the other way is not.** Expiring early costs a
// fallback to Ollama with the reason on screen and `/provider ollama/llama.cpp` to retry — the
// documented, working path. Expiring late costs a frozen interface, which the user cannot
// distinguish from a hang. The asymmetry is the whole argument.
//
// The real fix is that this should not be on the interface thread at all (CLAUDE.md: heavy work
// never runs on the main daemon thread). That is a restructure of `set_provider`, and it is
// recorded in STATE.md rather than attempted here.
pub const HEALTH_DEADLINE: Duration = Duration::from_secs(30);

/// How often the health wait polls. Small enough that a warm start is not rounded up.
pub const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How many lines of the server's stderr are kept for a failure report.
///
/// A `llama-server` writes several hundred lines at startup and the useful one is at the end. Kept
/// in a bounded ring rather than a growing buffer, so a server that runs for hours cannot become a
/// memory leak in the daemon that started it.
const KEPT_LOG_LINES: usize = 80;

/// The opening words of every fallback line, and **the string `classify_degradation` matches on**.
///
/// A constant rather than two literals: the projection's arm and the sentence it classifies are
/// otherwise free to drift, and the drift is silent — an unmatched remedy falls to
/// `DegradedPath::Unclassified`, whose headline is *"degraded · see the Status tab"*. Not wrong,
/// exactly; the specific claim replaced by a general one, on the single sentence this whole change
/// exists to put on screen.
pub const FELL_BACK_MARKER: &str = "llama.cpp is NOT serving";

/// The provider name, spelled once here.
///
/// `marlowe-provider` sits below `marlowe-view` and does not depend on it, so the name cannot be
/// imported. It is asserted equal to `marlowe_view::HYBRID` by
/// `marlowe-daemon/tests/hybrid_engine.rs`, which is a test at the seam rather than a dependency
/// edge added to make a string reachable.
pub const HYBRID_PROVIDER_NAME: &str = "ollama/llama.cpp";

// ─────────────────────────────────────────────────────────────────────────────────────────
// What went wrong, specifically enough that the sentence on screen is worth reading
// ─────────────────────────────────────────────────────────────────────────────────────────

/// Why `llama-server` is not serving. **One variant per thing that can actually happen**, each
/// carrying what was observed rather than a category.
///
/// # `Debug` is not the user-facing form
///
/// [`Self::cause`] is. It is a sentence, it names a path or a port or a log line, and it is what
/// goes into the persistent band. A `{:?}` of this type in the surface would be this project's
/// most-logged shape: a value *adjacent* to the answer.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineFailure {
    /// `llama-server` is not on this machine. Every path tried is named — on a layout that has
    /// changed under us, *where we looked* is the entire cost of the change.
    BinaryMissing { looked: String },
    /// Ollama's store would not yield a GGUF for this model. The undocumented-layout dependency,
    /// failing exactly where ADR-060 said it would.
    BlobUnresolvable { model: String, detail: String },
    /// Something is on the port and it is not a `llama-server` we can use.
    PortUnavailable { port: u16, detail: String },
    /// The spawn itself failed — a permission, a missing DLL beside the binary, an exec error.
    SpawnFailed { binary: PathBuf, detail: String },
    /// It started and died before `/health` came up.
    ExitedBeforeHealthy { code: String, last_log: String },
    /// It is running and has not answered in [`HEALTH_DEADLINE`].
    NeverBecameHealthy { waited_secs: u64, last_log: String },
    /// It was serving and then stopped. The mid-session case.
    ExitedMidSession { code: String, last_log: String },
    /// **It came up healthy and is running the model on the CPU.**
    ///
    /// The one failure here whose every other signal reads green — see
    /// `llamacpp::Availability::Ready`'s table. On a 16 GB card the usual cause is that the GPU is
    /// already full, and a leftover `llama-server` from an earlier run is enough.
    GpuUnavailable { reading: Offload, detail: String },
    /// `/props` reports a chat template that cannot render a tool block.
    ///
    /// **Not a degradation to shrug at.** Marlowe's every action is a tool call, and a template
    /// that cannot render one does not error: the model narrates instead of acting, and the whole
    /// thing presents as *the model got worse* rather than as *the prompt changed* (ADR-060 §4).
    TemplateHasNoTools { port: u16, detail: String },
    /// It answered, but not with the model we asked for.
    ///
    /// Not hypothetical: **Windows permitted two processes to bind port 11437 simultaneously**, so
    /// *"is something listening"* is not an identity check. This is the identity check.
    ServingSomethingElse { port: u16, served: String, wanted: String },
}

impl EngineFailure {
    /// **The clause a person reads.** Specific, and the specificity is the requirement.
    pub fn cause(&self) -> String {
        match self {
            EngineFailure::BinaryMissing { looked } => format!(
                "llama-server is not on this machine — looked in {looked}. Ollama bundles it, so \
                 installing Ollama normally provides it; MARLOWE_LLAMA_SERVER points at your own"
            ),
            EngineFailure::BlobUnresolvable { model, detail } => format!(
                "`{model}` could not be resolved in Ollama's model store — {detail}. This is the \
                 dependency ADR-060 accepted with eyes open: the store layout is not a documented \
                 interface, so an Ollama upgrade can break it"
            ),
            EngineFailure::PortUnavailable { port, detail } => format!(
                "port {port} is taken by something that is not a usable llama-server ({detail}). \
                 Start marlowe with `--llamacpp-port <N>` to use another"
            ),
            EngineFailure::SpawnFailed { binary, detail } => {
                format!("llama-server at {} could not be started — {detail}", binary.display())
            }
            EngineFailure::ExitedBeforeHealthy { code, last_log } => format!(
                "llama-server exited ({code}) while loading the model. Its last output was: \
                 {last_log}"
            ),
            EngineFailure::NeverBecameHealthy { waited_secs, last_log } => format!(
                "llama-server was still not answering /health after {waited_secs} s. Its last \
                 output was: {last_log}"
            ),
            EngineFailure::ExitedMidSession { code, last_log } => format!(
                "llama-server exited ({code}) part-way through the session. Its last output was: \
                 {last_log}"
            ),
            EngineFailure::GpuUnavailable { reading, detail } => format!(
                "THE GPU COULD NOT BE USED — llama-server came up healthy and is running the \
                 model on the CPU at {}, which is slower than Ollama rather than faster. \
                 {detail}",
                // **Not `disclosure()`.** That string is written to stand alone in a status
                // line and reads `RUNNING ON CPU (10 tok/s measured)`, which nests a
                // parenthesis inside a parenthesis here. Only the rate is wanted.
                match reading {
                    Offload::Cpu { tok_per_s } | Offload::Gpu { tok_per_s }
                        if tok_per_s.is_finite() =>
                    {
                        format!("{tok_per_s:.0} tok/s")
                    }
                    // The log or the VRAM delta decided it, so there is no rate. Saying so
                    // beats printing `NaN tok/s`, which reads as a bug in Marlowe.
                    _ => "a speed the generation probe could not measure".to_string(),
                }
            ),
            EngineFailure::TemplateHasNoTools { port, detail } => format!(
                "the llama-server on port {port} REFUSED a request carrying a tool ({detail}), \
                 and every action Marlowe takes is a tool call. Note that /props reports \
                 `supports_tools: true` on exactly this server — the declaration is not the \
                 enforcement, so this was measured by sending one. Relaunch it with `--jinja`"
            ),
            EngineFailure::ServingSomethingElse { port, served, wanted } => format!(
                "a llama-server is already on port {port} serving `{served}`, not `{wanted}`. It \
                 was not started by this daemon, so it is left alone rather than killed"
            ),
        }
    }

    /// The whole persistent line: **what is serving, why it is not the other one, and what it
    /// costs.**
    ///
    /// # Every clause is load-bearing and none of it is decoration
    ///
    /// * *"is NOT serving"* — the user picked `ollama/llama.cpp` and the picker still shows that,
    ///   because it is still what they picked. This sentence is the only thing that can say the
    ///   engine half of it is not happening.
    /// * the cause — [`Self::cause`], specific, with a log line where there is one.
    /// * *"Answers are unaffected"* — the failure this must not induce is a user concluding the
    ///   replies got worse and going to look at the model.
    /// * the number — 225 ms per request is what they lost, measured (ADR-060), not adjectival.
    /// * the retry — a degraded state a user cannot act on is a crash with better manners.
    pub fn fallback_line(&self) -> String {
        // **Trimmed, because some causes end in a full stop and some do not.** Without this
        // the single most important sentence in the change reads `... retry with `/provider
        // ollama/llama.cpp`.. Answers are unaffected` -- a doubled stop in the one line a
        // person is meant to read carefully.
        let cause = self.cause();
        let cause = cause.trim_end().trim_end_matches('.');
        format!(
            "{FELL_BACK_MARKER} this session — Ollama is. Cause: {cause}. Answers are \
             unaffected: same model, same weights, roughly 225 ms/request slower (ADR-060). \
             `/provider {HYBRID_PROVIDER_NAME}` retries the engine."
        )
    }
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// The supervised server
// ─────────────────────────────────────────────────────────────────────────────────────────

/// **Unload whatever Ollama is holding on the card, before `llama-server` asks for its own copy.**
///
/// # This is not tidiness. Two copies of a 9B do not fit on 16 GB, and the second one does not fail
///
/// `qwen3.5:9b` is **6.7 GB resident under Ollama**. A `llama-server` at `-ngl 99` wants its own
/// ~6.7 GB, plus a KV cache that takes it toward 9.5 GB at 32k context. The card is 16,376 MiB and
/// something is always using some of it. Measured on this machine with an Ollama runner warm:
/// **11,069 MiB used, 4,977 free** — not enough, and `llama-server` **does not refuse**. It loads,
/// answers `/health`, reports `supports_tools: true`, and runs on the CPU at a tenth of the speed.
///
/// **This is the common case, not an edge case.** Anyone who used Ollama before switching has a
/// resident runner, because that is what Ollama's scheduler is for.
///
/// # In this mode Ollama is the STORE, not the engine
///
/// That is the whole decision. A warm Ollama runner while llama.cpp serves is 6.7 GB of pure
/// waste, and it is the thing that will push the engine onto the CPU. So the runner goes; the
/// store, the downloader and the inventory stay, and none of them needs VRAM.
///
/// # Documented API only, unlike the store layout
///
/// `GET /api/ps` lists resident models; `POST /api/generate` with `keep_alive: 0` unloads one.
/// Both are documented Ollama endpoints, which matters here: the rest of this module reads
/// `~/.ollama/models` and accepts that an Ollama release can break it. **This part cannot break
/// that way**, so a store-layout change costs the blob resolution and not this.
///
/// # What it deliberately does NOT do, and the cost of that
///
/// It unloads **every** resident model, not only the one about to be served. A second model on
/// the card is just as fatal to the offload and Ollama gives no way to know whose it is. That is a
/// real cost: another tool's warm runner is evicted, and it will reload on that tool's next
/// request at Ollama's usual load time. The trade is accepted because the user has explicitly
/// asked for `ollama/llama.cpp`, which says llama.cpp is their engine — and because the
/// alternative is a silent CPU fallback, which is the failure this whole change exists to remove.
///
/// **Returns what it unloaded, so the announcement can say.** An eviction nobody is told about is
/// a 6.7 GB change to the machine's state made on the user's behalf without a record.
pub fn unload_resident_ollama_models() -> Vec<String> {
    let ollama = LocalEndpoint::default_ollama();
    let Ok(ps) = crate::http::get_json(&ollama, "/api/ps", Duration::from_secs(5)) else {
        // Ollama is not up, or does not answer. Nothing is resident, or we cannot tell — and in
        // neither case is there anything to do. **Not an error**: the hybrid must still start.
        return Vec::new();
    };
    let Some(models) = ps.get("models").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    let mut unloaded = Vec::new();
    for m in models {
        let Some(name) = m.get("name").or_else(|| m.get("model")).and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        // `keep_alive: 0` is Ollama's documented "unload now". An empty prompt means no generation
        // happens; the request exists only to carry the keep-alive.
        let body = serde_json::json!({ "model": name, "prompt": "", "keep_alive": 0 });
        // 10 s, not 30, and for the same reason as `HEALTH_DEADLINE`: this runs on the thread
        // that answers `/provider`, and it is paid ONCE PER RESIDENT MODEL. Three of them at
        // 30 s was 90 s of frozen interface before `llama-server` had even been asked to
        // start. An unload that does not answer in 10 s has not failed silently -- the
        // offload check that follows will find the card still occupied and fall back with a
        // cause the user can read.
        if crate::http::post_json(&ollama, "/api/generate", &body, Duration::from_secs(10)).is_ok() {
            unloaded.push(name.to_string());
        }
    }
    unloaded
}

/// A `llama-server` this process started, or deliberately adopted.
///
/// # Lifetime, stated rather than implied
///
/// * **Started** by [`start`], synchronously, and not returned until `/health` is 200 **and** the
///   offload reading says GPU. `/health` 200 is true only once the weights are IN; that positive
///   reading is what `Tier1Runtime::LlamaServerLoaded` requires before the VRAM reserve may
///   believe the bytes are already out of `memory.free`.
/// * **Stopped** on `Drop`. The daemon holds exactly one; dropping the daemon, switching provider
///   and switching model all drop it, and each is a place where a leaked 9.5 GB process would be
///   invisible until the next allocation failed — which, on a card that fits one 9B, is
///   immediately and as a *silent CPU fallback*.
/// * **Orphaned** if the daemon is killed outright — `Drop` does not run under `TerminateProcess`.
///   Not fixed here and not hidden either: the next [`start`] finds the orphan listening, checks
///   what it is serving, and **adopts it** when it matches. A stale server for another model is
///   reported by name rather than killed, because a process this daemon did not start may belong
///   to someone else.
pub struct SupervisedServer {
    child: Option<Child>,
    endpoint: LocalEndpoint,
    model: String,
    blob: PathBuf,
    /// The last lines the server wrote. Read on failure, and the reason a CUDA OOM reaches the
    /// user as *"cudaMalloc failed: out of memory"* rather than as *"exit code 1"*.
    log: Arc<Mutex<Vec<String>>>,
    /// **Where the forward pass runs, measured once on THIS process.** Carried rather than
    /// re-measured, because `--status` is on the path the surface hits every tick and the reading
    /// costs a real generation.
    offload: Offload,
    /// How long the start took. Reported on `/model`, where a person is waiting for it.
    pub startup: Duration,
    /// True when this server was already running and was adopted rather than spawned. It is then
    /// **not** killed on drop: we did not start it.
    pub adopted: bool,
}

impl SupervisedServer {
    pub fn endpoint(&self) -> &LocalEndpoint {
        &self.endpoint
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn blob(&self) -> &std::path::Path {
        &self.blob
    }

    /// The offload reading taken when this server started. **A fact about this process**, which is
    /// a weaker and honest claim compared with a fresh measurement — and the reason `--status` can
    /// report it without putting a model call in the render loop.
    pub fn offload(&self) -> Offload {
        self.offload
    }

    /// **Has it died since we last looked?** `None` while it is still up.
    ///
    /// Called at the start of every turn. `try_wait` does not block and does not reap a live
    /// child, so this costs one syscall on the turn path and buys the mid-session case — the one
    /// failure in ADR-060's list that cannot be caught at startup, and the one that would
    /// otherwise present as the model becoming unreachable for no stated reason.
    pub fn exited(&mut self) -> Option<EngineFailure> {
        if self.adopted {
            // We hold no handle to a process we did not start. Its death shows up as the driver
            // failing to connect, which the caller turns into a failure from the other side.
            return None;
        }
        let child = self.child.as_mut()?;
        match child.try_wait() {
            Ok(Some(status)) => {
                let failure = EngineFailure::ExitedMidSession {
                    code: describe_status(&status),
                    last_log: self.last_log(),
                };
                self.child = None;
                Some(failure)
            }
            Ok(None) => None,
            Err(e) => {
                let failure = EngineFailure::ExitedMidSession {
                    code: format!("its status could not be read: {e}"),
                    last_log: self.last_log(),
                };
                self.child = None;
                Some(failure)
            }
        }
    }

    /// The last non-empty line the server wrote, or a stated absence.
    ///
    /// **Never an empty string.** A failure line reading *"Its last output was: "* with nothing
    /// after it looks like a truncation bug in Marlowe and sends the reader to the wrong place.
    pub fn last_log(&self) -> String {
        let log = self.log.lock().expect("the llama-server log lock was poisoned");
        match log.iter().rev().find(|l| !l.trim().is_empty()) {
            Some(l) => format!("\"{}\"", l.trim()),
            None => "(it wrote nothing to stderr)".to_string(),
        }
    }

    /// Every line kept, oldest first. For `--dev`, and for the offload reading.
    pub fn log_tail(&self) -> Vec<String> {
        self.log.lock().expect("the llama-server log lock was poisoned").clone()
    }

    /// Stop it, and wait. Idempotent.
    pub fn stop(&mut self) {
        let Some(mut child) = self.child.take() else { return };
        let _ = child.kill();
        // **Waited on, not merely killed.** `kill` returns before the process has released its
        // device memory, and the next thing this daemon does after stopping a server is usually
        // start another one on the same card — where 9.5 GB not yet freed is not an error, it is
        // a silent CPU fallback.
        let _ = child.wait();
    }
}

impl Drop for SupervisedServer {
    fn drop(&mut self) {
        if self.adopted {
            return;
        }
        self.stop();
    }
}

fn describe_status(status: &std::process::ExitStatus) -> String {
    match status.code() {
        Some(c) => format!("exit code {c}"),
        None => "killed by a signal".to_string(),
    }
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// Starting it
// ─────────────────────────────────────────────────────────────────────────────────────────

/// Start (or adopt) a `llama-server` for `model` on `endpoint`.
///
/// **Every failure is an [`EngineFailure`], never a panic and never a `None` the caller has to
/// interpret.** The caller's only correct response to any of them is the same one: fall back to
/// Ollama and put [`EngineFailure::fallback_line`] on the screen for the session.
pub fn start(
    model: &str,
    endpoint: &LocalEndpoint,
    context_tokens: u32,
) -> Result<SupervisedServer, EngineFailure> {
    start_with_deadline(model, endpoint, context_tokens, HEALTH_DEADLINE)
}

/// As [`start`], with the health deadline supplied. Tests use this; the product uses [`start`].
pub fn start_with_deadline(
    model: &str,
    endpoint: &LocalEndpoint,
    context_tokens: u32,
    deadline: Duration,
) -> Result<SupervisedServer, EngineFailure> {
    // **The store is read FIRST, before anything is spawned or any port is touched.** It is the
    // failure most likely to be permanent — a moved layout, a model that was never pulled — and
    // the cheapest to detect. Reporting it before a port error means the user reads the cause
    // rather than a consequence.
    let resolved: ResolvedModel = ollama_store::resolve(model).map_err(|e| {
        EngineFailure::BlobUnresolvable { model: model.to_string(), detail: e.remedy() }
    })?;

    // **The card is cleared BEFORE the port is probed and long before anything spawns.** On a
    // 16 GB card a warm Ollama runner (6.7 GB) plus a llama-server (~9.5 GB at 32k) do not fit,
    // and the llama-server does not fail -- it runs on the CPU with every health signal green. See
    // [`unload_resident_ollama_models`].
    //
    // Placed after the blob resolution deliberately: if the store cannot be read there is nothing
    // to start, and evicting somebody's warm runner to then not start anything would be a cost
    // paid for no benefit.
    let unloaded = unload_resident_ollama_models();
    if !unloaded.is_empty() {
        eprintln!(
            "marlowe: unloaded {} from Ollama to free the card — in this mode Ollama is the \
             store, not the engine, and two copies of a 9B do not fit on 16 GB",
            unloaded.join(", ")
        );
    }

    // Already something there? Then either it is ours to use or the port is unavailable — and the
    // difference is decided by asking what it is serving, not by assuming.
    if let Some(outcome) = adopt_if_usable(endpoint, model, &resolved, context_tokens) {
        return outcome;
    }

    let binary = ollama_store::server_binary().map_err(|e| match e {
        ollama_store::ResolveError::NoServerBinary { looked } => {
            EngineFailure::BinaryMissing { looked }
        }
        other => EngineFailure::BinaryMissing { looked: other.remedy() },
    })?;
    let dll = ollama_store::backend_dll(&binary);
    let plan: LaunchPlan =
        resolved.launch_plan(&binary, dll.as_deref(), endpoint.port(), context_tokens);

    // **Read BEFORE the spawn**, because the signal is a delta and the absolute value is
    // meaningless on a shared card. `None` on a machine with no `nvidia-smi`, which is a real
    // state and is reported as `Unknown` rather than assumed either way.
    let free_before = llamacpp::device_free_bytes();

    // **`Started`, not `Instant::now`.** `determinism_guard.rs` fences real clock reads by path
    // and this file is not a fence; `crate::deadline` is. It exposes `elapsed()` and no way to
    // obtain a time value, so nothing stamped here can reach a journal, a memory id or a repro
    // hash. See that module for why reusing `marlowe_net::age::Mark` -- the same type, already
    // fenced -- was tried first and had to be reverted: it dragged `rustls` into the crate whose
    // empty TLS surface is ADR-028's evidence.
    let started = Started::now();
    let mut cmd = Command::new(&plan.binary);
    cmd.args(&plan.args)
        // stdout to null and stderr to a pipe we drain. **Not inherited**: a supervised child
        // writing onto the daemon's own stdout corrupts the surface, and a piped stream nobody
        // reads fills its buffer and blocks the server mid-load.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(dll) = &plan.backend_dll {
        // **The variable that decides WHICH FILE to load.** Necessary, and on its own it is not
        // sufficient — see the line below, which is the other 10x.
        cmd.env("GGML_BACKEND_PATH", dll);
    }
    // **THE OTHER HALF, AND OMITTING IT COSTS 10x WITH EVERY HEALTH SIGNAL STILL GREEN.**
    // `GGML_BACKEND_PATH` names the backend DLL; it says nothing about where THAT DLL's own
    // imports (`cudart`, `cublas`) live, and the Windows loader resolves those against `PATH`.
    // Measured back to back, same binary, same blob, same argv: `GGML_BACKEND_PATH` alone took
    // **no** device memory and ran at **10.6 tok/s**; with this line, **+5,523 MiB** and
    // **107.4 tok/s**.
    //
    // `plan.path_value()` prefixes the inherited PATH with the same directories
    // `LaunchPlan::command_line` prints — one list, two renderings, so the command a user is shown
    // and the process Marlowe starts cannot disagree about which directories are needed.
    if !plan.path_prefix.is_empty() {
        cmd.env("PATH", plan.path_value());
    }

    // **NO CONSOLE WINDOW. `llama-server.exe` is a console subsystem binary, so Windows gives it
    // one unless told otherwise, and a black box appears on the user's desktop for the life of the
    // session.**
    //
    // This is not cosmetic. The engine is meant to be an implementation detail of `/provider
    // ollama/llama.cpp` -- Ollama's own runner is invisible, and a window that appears only on the
    // fast path teaches the user that the fast path is the broken one. It is also a window with no
    // close semantics: closing it kills the engine mid-turn, and the supervisor would report a
    // crash the user caused and cannot connect to what they did.
    //
    // `CREATE_NO_WINDOW` (0x0800_0000) suppresses it. Stdout and stderr are already captured
    // (`Stdio::piped`), so nothing is lost -- the log the offload check reads is unaffected.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd.spawn().map_err(|e| EngineFailure::SpawnFailed {
        binary: plan.binary.clone(),
        detail: e.to_string(),
    })?;

    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    if let Some(stderr) = child.stderr.take() {
        let sink = Arc::clone(&log);
        // CLAUDE.md: heavy work never runs on the thread that serves the interface. This is not
        // heavy, but it is UNBOUNDED IN TIME — it lives as long as the server does — so it gets
        // its own thread rather than something the daemon's loop has to poll.
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut l = sink.lock().expect("the llama-server log lock was poisoned");
                if l.len() == KEPT_LOG_LINES {
                    l.remove(0);
                }
                l.push(line);
            }
        });
    }

    let mut server = SupervisedServer {
        child: Some(child),
        endpoint: endpoint.clone(),
        model: model.to_string(),
        blob: resolved.blob.clone(),
        log,
        offload: Offload::Unknown,
        startup: Duration::ZERO,
        adopted: false,
    };

    // The health wait. Polls, and checks for a dead child on every pass, so a fast failure is
    // reported fast and in the server's own words.
    //
    // **`OffloadPolicy::Carried(Unknown)` here, not `Measure`.** Timing a generation on every pass
    // of a poll loop would cost a model call every 100 ms; the offload reading is taken ONCE,
    // after health, below.
    loop {
        if let Some(child) = server.child.as_mut() {
            if let Ok(Some(status)) = child.try_wait() {
                let failure = EngineFailure::ExitedBeforeHealthy {
                    code: describe_status(&status),
                    last_log: server.last_log(),
                };
                server.child = None;
                return Err(failure);
            }
        }
        let liveness = Availability::probe(
            endpoint,
            model,
            context_tokens,
            OffloadPolicy::Carried(Offload::Unknown),
        );
        if liveness.is_ready() {
            if liveness.refuses_tools() {
                return Err(EngineFailure::TemplateHasNoTools {
                    port: endpoint.port(),
                    detail: "/props reports `supports_tools: false`".into(),
                });
            }
            // **The identity check, after our own spawn.** Windows allowed two processes to bind
            // 11437 at once, so the server answering on our port is not necessarily the one we
            // just started. Compared against the BLOB, because `llama-server` was launched with a
            // path and reports a path — the tag `qwen3.5:9b` never enters its vocabulary.
            if let Availability::Ready { served, .. } = &liveness {
                if !serves_blob(served.as_deref(), &resolved.blob) {
                    return Err(EngineFailure::ServingSomethingElse {
                        port: endpoint.port(),
                        served: served.clone().unwrap_or_else(|| "(it did not say)".into()),
                        wanted: resolved.blob.display().to_string(),
                    });
                }
            }
            server.startup = started.elapsed();
            break;
        }
        if started.elapsed() >= deadline {
            return Err(EngineFailure::NeverBecameHealthy {
                waited_secs: deadline.as_secs(),
                last_log: server.last_log(),
            });
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    // ── Is it actually on the GPU? ──────────────────────────────────────────────────────
    //
    // **THREE SIGNALS, AND THE ONE THIS PROJECT'S OWN REMEDY TEXT NAMED IS THE WEAKEST.**
    //
    // `no usable GPU found` was ABSENT from one failing launch on this machine and PRESENT in
    // another, an hour apart, both on the CPU. A check keyed on it reads green on a broken build.
    // So it is corroboration, and the deciding order is:
    //
    // | signal | says | cost | can it be fooled |
    // |---|---|---|---|
    // | the startup log | `Cpu` definitely, never `Gpu` by silence | free | a quiet log is `Unknown`, which is the honest reading |
    // | the VRAM delta | took **+5,523 MiB** or **0** | one `nvidia-smi` | another process allocating during our spawn fakes a GPU reading — so it may only CONFIRM cpu |
    // | `timings.predicted_per_second` | **17 vs 178**, a 10x gap | one 16-token generation | needs a machine-scoped floor |
    //
    // **The throughput reading is the decider** because it measures the property actually cared
    // about — how fast this server generates — rather than a proxy for it. The other two make the
    // *cause* specific, which is what the user reads.
    let from_log = llamacpp::offload_from_log(&server.log_tail());
    let from_vram = llamacpp::offload_from_vram_delta(free_before, llamacpp::device_free_bytes());
    let measured = llamacpp::measure_offload(endpoint);
    server.offload = match measured {
        // A real rate settles it in either direction.
        r @ (Offload::Gpu { .. } | Offload::Cpu { .. }) => r,
        // The generation probe could not run. Fall through to the cheaper readings rather than
        // guessing — and note that `Unknown` here is NOT promoted to `Gpu`, because that promotion
        // is the entire defect.
        Offload::Unknown => match (from_log, from_vram) {
            (Offload::Cpu { .. }, _) | (_, Offload::Cpu { .. }) => Offload::Cpu { tok_per_s: f64::NAN },
            (Offload::Gpu { .. }, _) | (_, Offload::Gpu { .. }) => Offload::Gpu { tok_per_s: f64::NAN },
            _ => Offload::Unknown,
        },
    };
    if !server.offload.is_gpu() {
        // The cause is assembled from whichever signals actually fired, so the sentence names the
        // real problem rather than a category. The `PATH` case is called out by name because it is
        // the one a user can fix and the one our own printed remedy used to cause.
        let mut detail = String::new();
        if matches!(from_log, Offload::Cpu { .. }) {
            detail.push_str(
                "The server's own startup log says the GPU backend did not load. If it reads \
                 `load_backend: failed to load …ggml-cuda.dll:` with nothing after the colon, the \
                 DLL was found and its own imports were not: that is a PATH problem, not a \
                 missing file. ",
            );
        }
        if matches!(from_vram, Offload::Cpu { .. }) {
            detail.push_str("It took no device memory at all across the load. ");
        }
        detail.push_str(&format!(
            "On this card a 9B at -ngl 99 wants ~9.5 GB, so ONE llama-server fits at a time and a \
             second one does not fail — it quietly runs on the CPU. Stop any other llama-server, \
             unload any resident Ollama model, then retry with `/provider {HYBRID_PROVIDER_NAME}`."
        ));
        let reading = server.offload;
        // Dropped here, which kills it. **Leaving a CPU server running would hold the port and,
        // where it did allocate, the card** — making the next attempt fail the same way. The
        // failure would reproduce itself.
        drop(server);
        return Err(EngineFailure::GpuUnavailable { reading, detail });
    }

    // ── Will it accept a tool? ──────────────────────────────────────────────────────────
    //
    // **Asked, not read off `/props`.** `chat_template_caps.supports_tools` is `true` on a
    // `--no-jinja` server that answers HTTP 500 to every request carrying tools, so the field is a
    // declaration with nothing enforcing it — and Marlowe's every action is a tool call. This
    // sends one.
    //
    // `Unknown` does NOT block. An unanswered probe is not evidence of a refusal any more than it
    // is evidence of support, and refusing a healthy server on a probe that timed out would fall
    // back to Ollama for no reason.
    let tools = llamacpp::probe_tool_support(endpoint, model);
    if let llamacpp::ToolSupport::Refused { detail } = tools {
        drop(server);
        return Err(EngineFailure::TemplateHasNoTools { port: endpoint.port(), detail });
    }

    Ok(server)
}

/// Does the path a server reports name the blob we resolved?
///
/// Tolerant at both ends because `llama-server` reports `model_path` in one build and a bare file
/// name in another, and because Windows paths differ by separator and case. **Never true for an
/// empty reading** — a server that did not say what it loaded has not been shown to be ours.
fn serves_blob(served: Option<&str>, blob: &std::path::Path) -> bool {
    let Some(served) = served else { return false };
    let norm = |s: &str| s.to_lowercase().replace('\\', "/");
    let (a, b) = (norm(served), norm(&blob.to_string_lossy()));
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b || a.ends_with(&b) || b.ends_with(&a) {
        return true;
    }
    let file = |s: &str| s.rsplit('/').next().unwrap_or(s).to_string();
    let (fa, fb) = (file(&a), file(&b));
    !fa.is_empty() && fa == fb
}

/// `None` when the port is free and a spawn should proceed. `Some` when something is already there
/// and the question is settled either way.
///
/// # Adoption is deliberate, and refusing to kill is the other half of it
///
/// A `llama-server` on the port serving the blob we want is the state a daemon killed with
/// `TerminateProcess` leaves behind — `Drop` does not run, the child outlives its parent, and the
/// next start finds a perfectly good server it did not create. Killing it and starting an
/// identical one would spend 1.6 s and 9.5 GB of allocation churn reaching the state already
/// present, on a card where that allocation is the scarce thing. **Adopting it and not killing it
/// on drop** is the honest pair: we use what is there, and we do not stop a process we did not
/// start.
///
/// **The adopted server's offload is MEASURED, not assumed.** There is no log to read — it was
/// started by someone else — so the timed generation is the only reading available, and the
/// alternative (adopt it and hope) is exactly the state this change exists to make unreachable.
fn adopt_if_usable(
    endpoint: &LocalEndpoint,
    model: &str,
    resolved: &ResolvedModel,
    context_tokens: u32,
) -> Option<Result<SupervisedServer, EngineFailure>> {
    let liveness = Availability::probe(
        endpoint,
        model,
        context_tokens,
        OffloadPolicy::Carried(Offload::Unknown),
    );
    match &liveness {
        // Nothing is there. The port is ours to take.
        Availability::EndpointDown { .. } => None,
        Availability::Ready { served, .. } => {
            if liveness.refuses_tools() {
                return Some(Err(EngineFailure::TemplateHasNoTools {
                    port: endpoint.port(),
                    detail: "/props reports `supports_tools: false`".into(),
                }));
            }
            // **IDENTITY FIRST, and the order is not stylistic.** The two checks below each send
            // a real request to whatever is on this port; doing that to a process we have not yet
            // established is ours means generating tokens on a stranger's server. `serves_blob`
            // costs nothing — the reading is already in hand from `/props` — so it goes first.
            if !serves_blob(served.as_deref(), &resolved.blob) {
                return Some(Err(EngineFailure::ServingSomethingElse {
                    port: endpoint.port(),
                    served: served.clone().unwrap_or_else(|| "(it did not say)".into()),
                    wanted: resolved.blob.display().to_string(),
                }));
            }
            // **The enforced check, on a server we did not launch and whose flags we cannot see.**
            // `--no-jinja` reports `supports_tools: true` and 500s every tools request, so
            // `refuses_tools()` above cannot catch it: the field is a declaration and this is the
            // enforcement.
            if let llamacpp::ToolSupport::Refused { detail } =
                llamacpp::probe_tool_support(endpoint, model)
            {
                return Some(Err(EngineFailure::TemplateHasNoTools {
                    port: endpoint.port(),
                    detail,
                }));
            }
            let reading = llamacpp::measure_offload(endpoint);
            if !reading.is_gpu() {
                return Some(Err(EngineFailure::GpuUnavailable {
                    reading,
                    detail: format!(
                        "This server was already running on port {} and was not started by \
                         Marlowe, so it is left alone rather than killed. Stop it yourself and \
                         retry with `/provider {HYBRID_PROVIDER_NAME}`.",
                        endpoint.port()
                    ),
                }));
            }
            Some(Ok(SupervisedServer {
                child: None,
                endpoint: endpoint.clone(),
                model: model.to_string(),
                blob: resolved.blob.clone(),
                log: Arc::new(Mutex::new(vec![
                    "(adopted a llama-server that was already running; its output goes wherever \
                     it was started from)"
                        .to_string(),
                ])),
                offload: reading,
                startup: Duration::ZERO,
                adopted: true,
            }))
        }
        // A server that is loading, one whose window is too small, one running on the CPU, or
        // something that is not a llama-server at all. Each is "the port is taken and not by
        // anything usable", and each reaches the user with `Availability`'s own remedy inside.
        other => Some(Err(EngineFailure::PortUnavailable {
            port: endpoint.port(),
            detail: other.remedy(),
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property the whole surfacing requirement rests on: **the sentence names the engine that
    /// is serving, the engine that is not, and the specific reason.**
    ///
    /// On a build without the fallback this does not compile — there is no `fallback_line`. On a
    /// build where `fallback_line` returned a generic string it fails on the `cudaMalloc`
    /// assertion, which is the clause a user would act on.
    #[test]
    fn the_fallback_line_names_both_engines_and_the_specific_cause() {
        let f = EngineFailure::ExitedBeforeHealthy {
            code: "exit code 1".into(),
            last_log: "\"ggml_backend_cuda_buffer_type_alloc_buffer: cudaMalloc failed: out of \
                       memory\""
                .into(),
        };
        let line = f.fallback_line();
        assert!(line.starts_with(FELL_BACK_MARKER), "the classifier keys on this: {line}");
        assert!(line.contains("Ollama is"), "{line}");
        assert!(line.contains("cudaMalloc failed"), "the specific cause must survive: {line}");
        assert!(line.contains("/provider ollama/llama.cpp"), "the retry must be named: {line}");
    }

    /// **The negative control for the marker.** Without it, a marker that were the empty string —
    /// or one `contains` finds in every remedy — would pass the test above and would classify
    /// every degradation, a stale binary included, as an engine fallback.
    #[test]
    fn the_marker_is_absent_from_remedies_that_are_not_a_fallback() {
        for other in [
            "the running daemon was built from source that has since changed; restart it",
            "no model available — nothing is listening on http://127.0.0.1:11434",
            "dense retrieval is offline; recall is lexical only",
        ] {
            assert!(
                !other.contains(FELL_BACK_MARKER),
                "`{FELL_BACK_MARKER}` must not appear in an unrelated remedy: {other}"
            );
        }
    }

    /// The CPU case must say **GPU**, in those words, and must not read as a generic slowdown.
    ///
    /// Without the offload work this variant does not exist, so the test does not compile. With a
    /// `GpuUnavailable` whose cause said only *"the engine is degraded"* it fails on the first
    /// assertion — which is the point: the requirement is the words, not the variant.
    #[test]
    fn a_cpu_server_says_the_gpu_could_not_be_used_and_not_merely_that_it_is_slow() {
        let f = EngineFailure::GpuUnavailable {
            reading: Offload::Cpu { tok_per_s: 10.2 },
            detail: "a leftover llama-server is holding the card".into(),
        };
        let line = f.fallback_line();
        assert!(line.contains("GPU COULD NOT BE USED"), "{line}");
        assert!(line.contains("CPU"), "{line}");
        assert!(line.contains("10 tok/s"), "the measured rate must be in it: {line}");
        assert!(
            line.contains("slower than Ollama"),
            "a CPU llama-server is worse than the thing it replaced, and the line must say so: \
             {line}"
        );
    }

    /// Every variant produces a distinct, specific cause. A `cause()` returning the same words for
    /// two variants would make the persistent line useless in exactly the situation it exists for.
    #[test]
    fn every_failure_names_something_specific_and_no_two_read_alike() {
        let all = vec![
            EngineFailure::BinaryMissing { looked: "C:\\a, C:\\b".into() },
            EngineFailure::BlobUnresolvable {
                model: "qwen3.5:9b".into(),
                detail: "no manifest at …".into(),
            },
            EngineFailure::PortUnavailable { port: 11437, detail: "something else answered".into() },
            EngineFailure::SpawnFailed {
                binary: PathBuf::from("llama-server.exe"),
                detail: "access denied".into(),
            },
            EngineFailure::ExitedBeforeHealthy {
                code: "exit code 1".into(),
                last_log: "\"cudaMalloc failed\"".into(),
            },
            EngineFailure::NeverBecameHealthy { waited_secs: 90, last_log: "\"…\"".into() },
            EngineFailure::ExitedMidSession { code: "exit code 3".into(), last_log: "\"…\"".into() },
            EngineFailure::GpuUnavailable {
                reading: Offload::Cpu { tok_per_s: 10.2 },
                detail: "the card is full".into(),
            },
            EngineFailure::TemplateHasNoTools { port: 11437, detail: "HTTP 500".into() },
            EngineFailure::ServingSomethingElse {
                port: 11437,
                served: "other.gguf".into(),
                wanted: "wanted.gguf".into(),
            },
        ];
        let causes: Vec<String> = all.iter().map(EngineFailure::cause).collect();
        for (i, a) in causes.iter().enumerate() {
            assert!(!a.trim().is_empty(), "variant {i} has no cause");
            for (j, b) in causes.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "variants {i} and {j} read alike");
                }
            }
        }
    }

    /// `last_log` never returns an empty string. A line reading *"Its last output was: "* looks
    /// like a truncation bug in Marlowe and sends the reader to the wrong place.
    #[test]
    fn an_empty_log_is_stated_rather_than_rendered_as_nothing() {
        let s = SupervisedServer {
            child: None,
            endpoint: crate::llamacpp::default_endpoint(),
            model: "m".into(),
            blob: PathBuf::from("b.gguf"),
            log: Arc::new(Mutex::new(vec!["   ".into(), String::new()])),
            offload: Offload::Unknown,
            startup: Duration::ZERO,
            adopted: true,
        };
        assert_eq!(s.last_log(), "(it wrote nothing to stderr)");
    }

    /// The identity check that `is something listening on 11437` is not.
    ///
    /// **The negative control is the third case**: without it, a `serves_blob` that returned
    /// `true` unconditionally passes the first two, and Marlowe adopts a stranger's server for a
    /// different model — answering from the wrong weights under the right name.
    #[test]
    fn a_server_is_ours_only_when_it_names_our_blob() {
        let blob = std::path::Path::new("C:\\Users\\m\\.ollama\\models\\blobs\\sha256-dec52a44");
        assert!(serves_blob(Some("C:/Users/m/.ollama/models/blobs/sha256-dec52a44"), blob));
        assert!(serves_blob(Some("sha256-dec52a44"), blob));
        assert!(!serves_blob(Some("sha256-0000dead"), blob), "a different blob is not ours");
        assert!(!serves_blob(None, blob), "a server that did not say has not been shown to be ours");
        assert!(!serves_blob(Some(""), blob));
    }
}
