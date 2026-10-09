//! **Which engine is serving, and — when it is not the one the user picked — why.**
//!
//! ADR-060, accepted as the hybrid. `marlowe_provider::hybrid` knows how to start a
//! `llama-server` and how to describe every way that fails; this holds the resulting state for the
//! life of the daemon and answers the one question the surface asks on every frame.
//!
//! # The state is here rather than on `DaemonConfig`, and that is the point
//!
//! `DaemonConfig::model_provider` is the **declaration** — what the user asked for. This is the
//! **resolution** — what is actually answering. Keeping them apart is what lets the picker go on
//! showing `ollama/llama.cpp` (which is still true: it is what they chose) while the band says the
//! engine half of it is not happening. Collapsing them would force a choice between two lies:
//! either the picker silently flips to `ollama`, so the user's own setting changes under them with
//! no record of who did it, or the screen claims llama.cpp is serving when Ollama is.
//!
//! # The fallback line persists, and persistence is a requirement rather than a nicety
//!
//! *"llama fails fall back to ollama but surface to user why"*. A notice that flashes once
//! satisfies the letter and misses the point: the user who scrolls back ten minutes later, sees a
//! reply that took 800 ms, and wants to know which engine produced it needs the answer to still be
//! on the screen. So the line lives in `StatusReport::degraded`, which §B5 renders in amber in the
//! status band on **every** frame, for as long as the fallback holds.

use marlowe_provider::hybrid::{self, EngineFailure, SupervisedServer};
use marlowe_provider::llamacpp::Offload;
use marlowe_provider::LocalEndpoint;

/// Which engine is serving this session.
pub enum HybridEngine {
    /// The `ollama/llama.cpp` provider is not selected. Nothing to supervise, nothing to say.
    NotSelected,
    /// A `llama-server` is serving, and it was measured to be on the GPU.
    Serving(Box<SupervisedServer>),
    /// **`llama-server` could not serve; Ollama is answering.** Latched for the session.
    ///
    /// # Why it latches instead of retrying every turn
    ///
    /// Retrying looks more helpful and is worse. It makes the band flicker between two engines; it
    /// spends a spawn and a health wait on every turn that fails; and it produces a session in
    /// which *"which engine served that reply"* has a different answer for every reply and is
    /// recorded nowhere. On this machine the commonest cause — the card is full — does not clear
    /// on its own, so most of those retries would fail identically and slowly.
    ///
    /// `set_provider` re-issued clears it. That is the user asking again, which is a different
    /// event from the daemon guessing.
    FellBack {
        /// The whole sentence, from [`EngineFailure::fallback_line`]. Stored rendered rather than
        /// as the failure, because it is read on every `--status` and re-rendering a string on a
        /// path the surface hits every tick is work for nothing.
        line: String,
    },
}

impl HybridEngine {
    /// Start a `llama-server` for `model`, or latch the reason it could not.
    ///
    /// **Never returns an error.** Every failure is a fallback, because that is the decision:
    /// Marlowe keeps working when Ollama's undocumented store layout changes under it. The caller
    /// gets an engine either way and reads [`Self::fallback_line`] to find out which.
    pub fn start(model: &str, endpoint: &LocalEndpoint, context_tokens: u32) -> Self {
        match hybrid::start(model, endpoint, context_tokens) {
            Ok(server) => {
                // ADR-029: announced, never inferred. Two things change under this engine that a
                // user cannot otherwise see — which process ran the forward pass, and on which
                // processor — and both are printable here.
                crate::announce::info(format!(
                    "engine llama.cpp · {} · started in {} ms · {}",
                    server.endpoint(),
                    server.startup.as_millis(),
                    server.offload().disclosure(),
                ));
                HybridEngine::Serving(Box::new(server))
            }
            Err(failure) => Self::fell_back(&failure),
        }
    }

    /// Latch a failure. The one place a fallback line is produced, so the announcement on stderr
    /// and the sentence in the status band cannot say different things.
    pub fn fell_back(failure: &EngineFailure) -> Self {
        let line = failure.fallback_line();
        // **Amber, and this is the one announcement whose level is not a judgement call.** The
        // engine the user picked is not the engine that is serving; §B2 gives amber to exactly
        // "not the way it was asked for". The band already latches this through
        // `StatusReport::degraded`; the pane keeps it with the rest of the log so the sequence is
        // legible — what started, what failed, what took over.
        crate::announce::warn(line.clone());
        HybridEngine::FellBack { line }
    }

    /// **Called at the start of every turn.** Catches the one failure that cannot be caught at
    /// startup: a server that was serving and then stopped.
    ///
    /// Returns the newly-latched line when this call is what discovered the death, so the turn can
    /// emit it once as an event as well as leaving it in the band. `None` on every other turn,
    /// which is the overwhelming majority — the cost is one non-blocking `try_wait`.
    pub fn refresh(&mut self) -> Option<String> {
        let HybridEngine::Serving(server) = self else { return None };
        let failure = server.exited()?;
        *self = Self::fell_back(&failure);
        match self {
            HybridEngine::FellBack { line } => Some(line.clone()),
            _ => unreachable!("fell_back constructs FellBack"),
        }
    }

    /// The endpoint a turn should send to, when llama.cpp is the thing serving.
    pub fn serving_endpoint(&self) -> Option<&LocalEndpoint> {
        match self {
            HybridEngine::Serving(s) => Some(s.endpoint()),
            _ => None,
        }
    }

    /// The persistent sentence, or `None` while the chosen engine is serving.
    ///
    /// **This is the negative control's subject.** A build in which this returned `Some` always
    /// would put a fallback notice on every healthy session, and every assertion about the
    /// fallback's wording would still pass. The control is
    /// `llamacpp_is_opt_in.rs::the_fallback_sentence_is_absent_when_no_engine_was_asked_to_start`,
    /// which checks that a plain-Ollama daemon and an OpenRouter daemon carry no fallback sentence
    /// at all — the half a test that only checks the message would miss.
    pub fn fallback_line(&self) -> Option<&str> {
        match self {
            HybridEngine::FellBack { line } => Some(line),
            _ => None,
        }
    }

    /// One clause for the status line, naming the engine and the processor it is on.
    pub fn disclosure(&self) -> String {
        match self {
            // **Read only from `status_hybrid`**, i.e. on a daemon whose provider IS the hybrid.
            // So the honest sentence is not "Ollama serves and Ollama runs" — that describes the
            // plain `ollama` provider, which never reaches here — but that no engine has been
            // started for this one. Reachable when the provider was set on the config directly
            // rather than through `Daemon::open` or `set_provider`.
            HybridEngine::NotSelected => {
                "no llama-server has been started for this daemon; Ollama is answering".to_string()
            }
            HybridEngine::Serving(s) => format!(
                "llama.cpp on {} off Ollama's blob, {}",
                s.endpoint(),
                s.offload().disclosure()
            ),
            HybridEngine::FellBack { .. } => "Ollama is serving (llama.cpp fell back)".to_string(),
        }
    }

    /// The offload reading, when there is a server to have one.
    pub fn offload(&self) -> Option<Offload> {
        match self {
            HybridEngine::Serving(s) => Some(s.offload()),
            _ => None,
        }
    }

    /// Stop whatever is running. Called before a model switch and on provider change.
    pub fn stop(&mut self) {
        *self = HybridEngine::NotSelected;
    }
}

/// **Which process holds tier 1 — the VRAM reserve's authority, derived from the engine that is
/// actually serving.**
///
/// # This is a function so the test can read the deciding code rather than a copy of it
///
/// It was seven lines inline in `Daemon::open`. A test could then only assert on a
/// reconstruction, which is the `inline_threshold_bytes == 0` shape: green on a build where the
/// deciding code says something else.
///
/// # The four answers, and the one that used to be wrong
///
/// | provider | engine | runtime | why |
/// |---|---|---|---|
/// | `ollama` | — | `Ollama` | Ollama runs it; ask `ollama ps` |
/// | `openrouter` | — | `NotOnThisCard` | tier 1 is not on this card at all |
/// | `ollama/llama.cpp` | serving | `LlamaServerLoaded` | its ~9.5 GB is **already out of `memory.free`** |
/// | `ollama/llama.cpp` | **fell back** | **`Ollama`** | **Ollama is running tier 1 now** |
///
/// **The last row is the fix.** The inline version asked `Availability::probe` and answered
/// `NotOnThisCard` whenever it was not `Ready` — which is exactly the fallback case, where Ollama
/// has loaded the model and *does* hold the card. That under-reserves the full weight of tier 1
/// and lets the embedder take memory belonging to the language model; Ollama then evicts its own
/// model rather than failing, so the symptom appears in a log this process cannot see.
///
/// Row 3 is the double-count the brief names, and it is a **positive** reading rather than an
/// assumption: `HybridEngine::Serving` exists only after `/health` returned 200 *and* the offload
/// reading said GPU, both taken by `hybrid::start` moments before. Ask what this would report if
/// the server were not loaded — `Serving` would not exist, and the answer would be one of the
/// other three rows.
pub fn tier1_runtime_for(
    provider: &crate::daemon::ModelProviderChoice,
    engine: &HybridEngine,
) -> marlowe_memory::cue::dense::vram::Tier1Runtime {
    use crate::daemon::ModelProviderChoice as P;
    use marlowe_memory::cue::dense::vram::Tier1Runtime as T;
    match provider {
        P::Ollama => T::Ollama,
        P::OpenRouter { .. } => T::NotOnThisCard,
        P::LlamaCpp { .. } => match engine {
            HybridEngine::Serving(_) => T::LlamaServerLoaded,
            // Fell back, or not yet started. Either way Ollama is the thing that will answer the
            // next turn, and its weights are the ones that must be left room for.
            HybridEngine::FellBack { .. } | HybridEngine::NotSelected => T::Ollama,
        },
    }
}
