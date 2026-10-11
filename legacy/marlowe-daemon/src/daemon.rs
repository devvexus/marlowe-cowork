//! The daemon. Owns the journal, the engine, and the run table.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use marlowe_contract::TrustClass;
use marlowe_exec::FileSystemTools;
use marlowe_journal::{Journal, Profile};
use crate::clock::SystemClock;
use marlowe_loop::RunControl as _;
use marlowe_loop::{
    ApprovalGate, Budget, CapabilityProfile, ClockSource, Engine, GovernanceConstraint,
    LoopOutcome, MemoryRecorder, OutputContract, Ports, Provenance,
    Run, RunId, SessionId, SessionState, Summarizer, ToolLineState, TurnEvent, TurnSink,
    MEMORY_TOKEN_BUDGET,
};
use marlowe_permission::scope::WorkspaceScope;
use marlowe_permission::{BlastRadius, Tier};
use marlowe_provider::{capability_for, Availability, LocalEndpoint, OllamaDriver, Routing};
use marlowe_tools::builtin_registry;

use crate::protocol::{Event, Request, StatusReport, MAX_TOOL_DETAIL_BYTES};
use crate::DEFAULT_DAEMON_PORT;

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("could not open the profile at {root}: {detail}")]
    Profile { root: String, detail: String },
    #[error("could not listen on 127.0.0.1:{port}: {detail}")]
    Listen { port: u16, detail: String },
    #[error(
        "path scoping refuses to run here: {detail}. The daemon does not start without it — a \
         harness with filesystem tools and no wall is the configuration ADR-002 removed the \
         kernel backstop from"
    )]
    Scope { detail: String },
    #[error("{detail}")]
    Setup { detail: String },
    /// The fifth instance of the `done` defect, refused at startup. See
    /// `marlowe_loop::profile::UnrunnableTools`.
    #[error("{0}")]
    UnrunnableTools(#[from] marlowe_loop::UnrunnableTools),
}

/// Which provider serves this daemon's model calls. **ADR-046.**
///
/// # This enum is the zero-config guard, and it is the guard because ONE function reads it
///
/// K6 — install to first useful output, no configuration — is a kill criterion and it is
/// currently MET on the local path. The way that regresses is not a redesign; it is an
/// environment variable, or a "sensible" fallback, quietly deciding that a hosted provider is in
/// use when nobody asked. `blocks_composed_targets` is this project's precedent for the fix: one
/// function, called at the enforcement site *and* by the test, so the two cannot disagree.
///
/// [`DaemonConfig::model_provider`] is that function. The run path selects a driver from it and
/// `--status` announces it from it, so a test that asserts on it is asserting on the thing that
/// decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelProviderChoice {
    /// ADR-028's default. Loopback, no account, no key, no network.
    Ollama,
    /// ADR-046. **Opt-in only**, and never reachable except by an explicit `--provider openrouter`.
    OpenRouter { model: String },
    /// ADR-060, delivered as its Option E. A local `llama-server` on loopback: **opt-in only**,
    /// never reachable except by an explicit `--provider llamacpp`, and Ollama stays the default.
    ///
    /// The endpoint is a [`LocalEndpoint`], which refuses any non-loopback host by construction —
    /// the property ADR-060 §10 asks to preserve, held by the type rather than by a check somebody
    /// has to remember. The model NAME is not in here: `llama-server` serves whatever it was
    /// launched with, and `config.model` is what this daemon believes that to be.
    ///
    /// **Both settings live in the variant rather than beside it on `DaemonConfig`.** A port field
    /// and a sampling field on the config would be a second source of two facts the choice already
    /// carries, and the two can disagree — which is the shape this file's own header calls the
    /// zero-config guard's failure mode. Everything that already threads a `ModelProviderChoice`
    /// through the harness therefore carries these with no signature change.
    LlamaCpp {
        endpoint: LocalEndpoint,
        sampling: marlowe_provider::llamacpp::SamplingSource,
    },
}

impl ModelProviderChoice {
    /// The one word `--status` prints. ADR-029's rule — announced, never inferred.
    pub fn name(&self) -> &'static str {
        match self {
            ModelProviderChoice::Ollama => "ollama",
            ModelProviderChoice::OpenRouter { .. } => "openrouter",
            // **ONE ENTRY NAMING BOTH HALVES, and it is the decision rather than a label.**
            // ADR-060 accepted as the hybrid: Ollama stores, downloads and lists the models;
            // `llama-server` serves them off the blob Ollama already holds. A silent swap under
            // the existing `ollama` entry was explicitly refused — the user must be able to see
            // which engine is answering.
            //
            // **This string MUST equal the `PROVIDERS` entry**, so it is the same constant.
            // `project.rs`'s picker finds the active provider with `position()` and falls back to
            // `unwrap_or(0)`, so a typo here would render a hybrid daemon as plain `ollama` — a
            // silent wrong answer in the one place a user reads which engine is live.
            ModelProviderChoice::LlamaCpp { .. } => marlowe_view::provider::HYBRID,
        }
    }
}

/// **Two facts, and `or_else` published only one of them.**
///
/// Every status arm read `stale_against_source().or_else(|| availability.remedy())`. With nothing
/// listening on the engine's port AND a daemon built from stale source, `--status` printed the
/// staleness warning and **not the launch command** — so the one instruction that makes a missing
/// server survivable was hidden at exactly the moment it was needed.
///
/// A stale binary and a dead engine are independent, and both are the user's to act on. The engine
/// leads because it is what stops the next turn working; staleness follows because it explains why
/// the engine's behaviour may not match the source in front of them.
fn degraded_line(primary: Option<String>, stale: Option<String>) -> Option<String> {
    match (primary, stale) {
        (Some(p), Some(s)) => Some(format!("{p}

Also: {s}")),
        (Some(p), None) => Some(p),
        (None, s) => s,
    }
}

/// One turn's resolved provider, with everything that turn needs to build a driver.
///
/// Local to the run path: [`ModelProviderChoice`] is the *declaration* and this is the
/// *resolution*, and keeping them apart is what stops a probe result being mistaken for a setting.
enum Selected {
    Ollama(LocalEndpoint, Routing),
    OpenRouter(String),
    /// The endpoint, the configured model name, and where the sampler comes from. The name is
    /// sent as `"model"` and used to resolve the sampler from Ollama's store; it does not route.
    LlamaCpp(LocalEndpoint, String, marlowe_provider::llamacpp::SamplingSource),
}

/// **Where this turn's model gets its answers from — the ONE seam, and it is a parameter of the
/// ONE turn.**
///
/// # The finding this exists to close
///
/// `Daemon::turn` built its `Box<dyn ModelDriver>` internally and neither public door took one, so
/// **nothing in the workspace could drive a real daemon turn with a scripted model.** The measured
/// consequence was that `turn`'s injected-memory push — `state.push(Block::new(InjectedMemory,
/// retrieved.text, retrieved.floor))`, the only production line the whole of layer 3 runs through —
/// had **zero** coverage: laundering its class to `UserAsserted`, and guarding it with `if false
/// &&`, each left the entire `marlowe-daemon` crate green (`runs/m3-mutation/finding1*.txt`).
///
/// The barrier was never assertion strength. Provider selection `return`s early when
/// `Availability::probe` reports no model, so on a machine with no Ollama running nothing in
/// process reached the push at all. [`Self::Supplied`] therefore enters `turn` **before** provider
/// selection and skips it entirely, rather than substituting a driver after it.
///
/// # What this is NOT
///
/// It is not a second turn path. `resume_streaming`'s own doc comment states the rule: *"a separate
/// resume path would be a second place where the profile, the governance tier, the tool host and
/// the trust floor are assembled, and the two would drift. The one that drifted would be the one
/// nobody runs interactively."* The same argument applies with more force here, because a scripted
/// turn exists precisely to measure that assembly. So this is one parameter threaded through the
/// one `turn`, and **everything after the driver is chosen is byte-identical between the two
/// variants**: the same profile, the same registry, the same tool host, the same retrieval, the
/// same push, the same adjudicator.
///
/// # Why an enum and not the alternatives
///
/// * **`Option<&mut dyn ModelDriver>`** was the obvious shape and is rejected on the call site:
///   `turn` already takes `Option<Checkpoint>`, so every configured call would read
///   `self.turn(session, message, None, None, approvals, on_event)` — two adjacent `None`s of
///   different meanings, which is a transposition waiting to happen and reads as *absence* where
///   the fact is *choice*. `DriverSource::Configured` says which one it is.
/// * **A factory (`&mut dyn FnMut(...) -> Box<dyn ModelDriver>`)** would let a caller build a
///   driver per turn, which nothing needs, and it hands the seam a closure that could consult the
///   config — a second place where "which provider serves" is decided. `model_provider()` is the
///   one function that decides that (see the selection block), and a factory would quietly make it
///   two.
/// * **A `ModelDriver` field on `Daemon`** would make the substitution outlive a turn and would
///   have to be `Option`, so the product path would carry a branch on a field that is `None` in
///   every shipped run — a control the product declares and never reads, which is instance #16.
enum DriverSource<'a> {
    /// Select a provider from the config and build the driver, exactly as the product does. Every
    /// shipped path — `ask`, `ask_streaming`, `ask_streaming_with`, `resume_streaming` — passes
    /// this, and the block it reaches is unchanged.
    Configured,
    /// Drive the turn with this driver and **do not select a provider at all**.
    Supplied(&'a mut dyn marlowe_loop::ModelDriver),
}

/// [`DriverSource`] once provider selection has run: the configured case now carries the provider
/// it resolved to.
///
/// **A second type rather than a third variant, because a third variant would be constructible by
/// a caller.** `Configured`-before-selection and `Provider(_)`-after-selection are different
/// states, and folding them into one enum would let an external caller hand `turn` a pre-selected
/// provider — a way into the driver-construction block that never passed the availability probe.
/// Two small types make the impossible state unrepresentable instead of merely unreached; the
/// alternative, an `Option<Selected>` beside a `DriverSource` with an `unreachable!()` for the
/// `(Configured, None)` corner, keeps the invariant in a comment.
enum TurnDriver<'a> {
    Provider(Selected),
    Supplied(&'a mut dyn marlowe_loop::ModelDriver),
}

pub struct DaemonConfig {
    pub profile_root: PathBuf,
    pub workspace: PathBuf,
    pub port: u16,
    pub model: String,
    /// ADR-029: the active rerank provider, **announced** rather than silently chosen. Read from
    /// the field the profile row stamps; this crate does not derive a second one.
    pub rerank_provider: String,
    /// `--dev`: write raw provider frames and the outbound request to stderr.
    pub dev: bool,
    /// Whether the model separates reasoning into the `thinking` channel.
    ///
    /// Declared here so a run states it rather than inheriting whatever the provider defaults to.
    pub thinking: bool,
    /// The context window in tokens. **One number**: it becomes both `num_ctx` on every request
    /// and the assembler's window, so §6's 70% compaction trigger is computed against the window
    /// the provider actually has.
    pub context_tokens: u32,
    /// Which provider answers. **`Ollama` unless something explicitly says otherwise** —
    /// see [`ModelProviderChoice`], and `tests/zero_config_is_unchanged.rs`, which asserts that
    /// no environment variable can move it.
    pub model_provider: ModelProviderChoice,
    /// The cross-encoder directory. `None` makes memory **write-only**, announced by
    /// [`crate::memory::RetrievalState`] and printed by `--status`.
    ///
    /// Not required, deliberately, and this is the one place this session accepts an absent
    /// dependency rather than a load-time error. The eval adapter requires it because a run without
    /// it measures a different system under the same label; the product's failure mode is
    /// different — refusing to start would make a 60 MB model an install-time dependency of being
    /// able to talk at all, which is K6. What must never happen is retrieval silently not running,
    /// and that is what the announcement closes.
    pub reranking: Option<PathBuf>,
    /// Every model `openrouter.ai` serves, fetched **once when the provider is switched** and
    /// cached here. Empty until then.
    ///
    /// **Never fetched on the status path.** `status()` is called on essentially every tick of the
    /// surface, and a network round trip inside a repaint loop is a freeze with a plausible
    /// explanation. See `marlowe_openrouter::catalogue`.
    pub openrouter_models: Vec<String>,
    /// The hosted slug to go back to. Set when leaving OpenRouter, read when returning, so a
    /// round trip through `ollama` does not make the user retype it.
    pub last_openrouter_model: Option<String>,
}

impl DaemonConfig {
    /// **The single definition of which provider is in use.**
    ///
    /// Called by the run path to choose a driver, by `status()` to announce one, and by
    /// `tests/zero_config_is_unchanged.rs` to assert on one. A second reading of the same fact —
    /// an env var checked at the driver site, say — is how "OpenRouter is opt-in" becomes true of
    /// the comment and false of the code.
    pub fn model_provider(&self) -> ModelProviderChoice {
        self.model_provider.clone()
    }

    pub fn new(profile_root: PathBuf, workspace: PathBuf) -> Self {
        Self {
            profile_root,
            workspace,
            port: DEFAULT_DAEMON_PORT,
            model: marlowe_provider::DEFAULT_MODEL.to_string(),
            // Until retrieval is wired (M2 D) there is no profile row to read, and inventing a
            // value here would be the second source ADR-029 forbids. The honest value is that
            // nothing has announced one yet.
            rerank_provider: "not-wired".to_string(),
            // **The default is the local path and nothing can move it but an explicit choice.**
            //
            // **AND IT IS `Ollama` HERE ON PURPOSE, EVEN THOUGH THE PRODUCT DEFAULTS TO THE
            // HYBRID.** `ollama/llama.cpp` is what a user gets; it is resolved in
            // `marlowe::resolve_provider`, at the CLI boundary, and NOT in this struct.
            //
            // The difference is not stylistic. `Daemon::open` starts an engine when this field
            // says `LlamaCpp`, and **twenty `Daemon::open` call sites in the daemon tests alone
            // take this default** — with cargo running tests inside a binary on parallel threads.
            // Putting the hybrid here made every one of them spawn a `llama-server` and load a
            // 6.7 GB model onto a 16 GB card, concurrently. The workspace suite stopped being a
            // four-minute run and started **freezing the machine before it could finish**.
            //
            // So: this struct is the *declaration a library caller inherits*, and it must be inert.
            // The product's opinion lives where the product is assembled. A test that wants the
            // hybrid asks for it by name, which is also the only way to be sure a test that
            // exercises it meant to.
            model_provider: ModelProviderChoice::Ollama,
            dev: false,
            thinking: true,
            context_tokens: marlowe_provider::DEFAULT_CONTEXT_TOKENS,
            reranking: None,
            openrouter_models: Vec::new(),
            last_openrouter_model: None,
        }
    }
}

/// A run the daemon owns, as the client is allowed to see it.
#[derive(Debug, Clone)]
pub struct RunSummary {
    pub id: String,
    pub status: String,
    pub tokens: u64,
    pub depth: u8,
    /// **Which model actually answered, and which upstream served it.** ADR-046 §3.
    ///
    /// `None` on the local path, where the question does not arise: Ollama serves the model whose
    /// name was asked for, on this machine. On the hosted path it is the difference between a
    /// benchmark row somebody can re-run and one nobody can, because OpenRouter may route one
    /// model name to a different upstream between two requests without the name changing.
    pub attribution: Option<String>,
    /// What the run has cost, and how long it has been going. **M3 Session A**: §6.2's window
    /// renders *"elapsed, spend against ceiling"* and there was nowhere to read either from.
    pub spend_micros_usd: u64,
    /// The run's final wall time, set when the turn ends. **`0` while it is still going** — see
    /// `started_ms`, and `ControlPlane::detail`, which is where the two become one number.
    pub elapsed_ms: u64,
    /// When the daemon accepted the work, on the wall clock. **Session F.**
    ///
    /// `elapsed_ms` alone could not drive a window: it is written once, at the end, so a run that
    /// had been going for a minute reported `0` for the whole minute. A window whose elapsed reads
    /// zero while the thing is visibly working is the surface contradicting itself in the one panel
    /// whose job is saying what is happening.
    pub started_ms: u64,
}

impl RunSummary {
    /// A fresh record for an accepted turn. **Recorded before anything can fail**: the daemon took
    /// the work, so the record is the daemon's from that moment.
    pub fn accepted(id: String) -> Self {
        Self {
            id,
            status: "running".into(),
            tokens: 0,
            depth: 0,
            attribution: None,
            spend_micros_usd: 0,
            elapsed_ms: 0,
            // **Through the fence.** §4.5 forbids a system clock on the contract paths and names
            // the legitimate case in the same paragraph — in production the harness supplies the
            // real clock. `SystemClock` is that harness's one clock, and `determinism_guard.rs`
            // fences the file it lives in rather than this one.
            started_ms: ClockSource::now_ms(&mut crate::clock::SystemClock).max(0) as u64,
        }
    }

    /// The wire frame. **One definition**, so the main port and the control port cannot report
    /// the same run differently — which is the `--status` family in miniature.
    pub fn to_frame(&self) -> Event {
        Event::Run {
            id: self.id.clone(),
            status: self.status.clone(),
            tokens: self.tokens,
            depth: self.depth,
            attribution: self.attribution.clone(),
        }
    }
}

/// A tool detail cut to [`MAX_TOOL_DETAIL_BYTES`], **saying so where it was cut**.
///
/// Cut here rather than at the producer: the loop's `text` is what the MODEL received, and a
/// display bound must never shorten the record of that. This is the display's own boundary.
///
/// The notice is the shape `bash` already uses for a killed command — a bracketed sentence naming
/// the bound and what was really produced — because a pane whose output merely stops is
/// indistinguishable from a command that finished.
pub fn bounded_detail(d: String) -> String {
    if d.len() <= MAX_TOOL_DETAIL_BYTES {
        return d;
    }
    let mut cut = MAX_TOOL_DETAIL_BYTES;
    while cut > 0 && !d.is_char_boundary(cut) {
        cut -= 1;
    }
    let n = d.len();
    format!(
        "{}\n[the harness stopped showing this at {MAX_TOOL_DETAIL_BYTES} bytes. The model \
         received {n}.]",
        &d[..cut]
    )
}

/// Turns a loop event into a wire event. Render-only.
///
/// `TurnEvent::Done` returns `None`: the loop's own Done carries no outcome, and the daemon emits
/// a richer one after `engine.run` returns. Under the old collecting sink that was fixed up by
/// `retain`ing it out of the vector afterwards — **which streaming makes impossible**, because a
/// written event cannot be recalled. Filtering at the point of emission is the same correction
/// made where it still works.
fn to_wire(event: TurnEvent) -> Option<Event> {
    Some(match event {
        TurnEvent::TextDelta(t) => Event::Text { delta: t },
        TurnEvent::ReasoningDelta { text, tokens } => Event::Reasoning { delta: text, tokens },
        TurnEvent::SpeechRetracted { tokens } => Event::SpeechRetracted { tokens },
            TurnEvent::ToolLine { id, verb, target, state } => {
                // **The detail died on this line for the whole of M1 and M2**, and both ends of
                // the path had the field. `ResultSummary::render` is metrics only; `Event::Tool`
                // had five fields and none was a detail, so `s.detail` was dropped here and
                // nothing downstream could recover it. A `write` the permission layer refused
                // carried its reason as far as this function and no further.
                let (state, summary, detail) = match state {
                    // A running call has produced nothing yet. `None`, not an empty string: the
                    // absence is the fact.
                    ToolLineState::Running { elapsed_ms } => {
                        ("running".to_string(), format!("{elapsed_ms} ms"), None)
                    }
                    ToolLineState::Ok(s) => ("ok".to_string(), s.render(), s.detail.clone()),
                    ToolLineState::Failed(s) => {
                        ("failed".to_string(), s.render(), s.detail.clone())
                    }
                };
                Event::Tool { id, verb, target, state, summary, detail: detail.map(bounded_detail) }
            }
            TurnEvent::Compacted { turns } => Event::Compacted { turns },
            TurnEvent::Degraded { what } => Event::Degraded {
                what: what.headline().to_string(),
                // The specific remedy is the provider's to state; the loop only knows the class.
                remedy: "see `marlowe --status`".to_string(),
            },
            // **The prompt the GATE sends is the one that carries a decision id.** This one is
            // the loop announcing that it is about to ask, so it is render-only and its id is 0.
            TurnEvent::ApprovalPrompt(br) => Event::Approval {
                decision: 0,
                verb: br.verb,
                scope: br.scope,
                reversible: br.reversible,
                novelty: br.novelty.map(|n| format!("{n:?}")),
            },
        TurnEvent::Done { .. } => return None,
    })
}

/// How many streamed tokens between `Event::Cadence` frames. See the emission site for why this
/// is not 1.
///
/// Eight is roughly 110 ms at this machine's measured 73.6 tok/s and roughly 800 ms at the 10 tok/s
/// a CPU offload produces — which is the right way round: the slower the engine, the more obviously
/// the figure is telling you so, and the fewer frames it spends doing it.
const CADENCE_EVERY: u64 = 8;

/// A sink that hands each event to a callback **as the loop produces it**.
///
/// # This is the structural half of streaming
///
/// The previous sink was `struct Collector { events: Vec<Event> }`, and `Daemon::ask` returned
/// that vector — so the daemon computed every model call and every tool execution before writing
/// a single byte to the socket. Layers 1 and 4 could stream perfectly and the user would still see
/// nothing until the turn was over, because the middle held everything.
///
/// The callback is what lets `serve_one` write-and-flush per event and `ask` collect into a `Vec`
/// for the in-process path, without two sinks that could drift.
struct CallbackSink<F: FnMut(Event)> {
    on_event: F,
}

impl<F: FnMut(Event)> TurnSink for CallbackSink<F> {
    fn emit(&mut self, event: TurnEvent) {
        if let Some(e) = to_wire(event) {
            (self.on_event)(e);
        }
    }
}

/// One wire event as a run window's frame, or `None` if it is not a run's own output.
///
/// # This is where ADR-055's scope is enforced, and the `None`s are the enforcement
///
/// ADR-055 §7 permits a window to stream `TextDelta` and `ReasoningDelta` — **model prose** — plus
/// the harness's own §B6 line. It explicitly does not permit raw tool results, and it does not
/// permit the window to become a second copy of the conversation.
///
/// So `Status`, `Approval`, `Done`, `Run`, `Error` and the control-plane frames return `None`. Each
/// is either about the daemon rather than the run, or is already carried by
/// [`crate::protocol::Event::RunDetail`] — and a frame that arrived twice by two routes would be a
/// window that disagreed with its own identity panel.
///
/// **Exhaustive, with no `_` arm.** The next `Event` variant somebody adds has to decide whether it
/// belongs in a window, at the site where ADR-055's scope is written down.
pub fn to_run_frame(e: &Event) -> Option<crate::protocol::RunFrame> {
    use crate::protocol::RunFrame;
    Some(match e {
        Event::Text { delta } => RunFrame::Text { delta: delta.clone() },
        Event::Reasoning { delta, tokens } => {
            RunFrame::Reasoning { delta: delta.clone(), tokens: *tokens }
        }
        Event::SpeechRetracted { tokens } => RunFrame::SpeechRetracted { tokens: *tokens },
        // **`detail` is dropped, and that is ADR-055 being enforced rather than an omission.**
        // `RunFrame`'s own doc says it: *"There is no `ToolResult` variant and there must not be
        // one. What crosses is model prose and the harness's own §B6 summary line."* A `read`
        // window is a raw tool result.
        //
        // There is a second reason and it is arithmetic. Conversation events stream once and are
        // not retained; run frames live in `control_plane::Frames` up to `MAX_FRAMES`, and every
        // `frames_since` poll CLONES them. At `MAX_TOOL_DETAIL_BYTES` that is 64 MB per watched
        // run, re-cloned per poll.
        //
        // A failure detail was considered as a carve-out and refused for the reason `finish_call`
        // already gives about failures: `bash` sets `failed: code != 0` with a full stdout body,
        // so "a failure detail is harness prose" is an assumption about every executor that will
        // ever exist. If a window should show refusal reasons, that is a separate harness-authored
        // field and a separate decision.
        Event::Tool { id, verb, target, state, summary, detail: _ } => RunFrame::Tool {
            id: *id,
            verb: verb.clone(),
            target: target.clone(),
            state: state.clone(),
            summary: summary.clone(),
        },
        Event::Compacted { turns } => RunFrame::Compacted { turns: *turns },
        // A degraded path is a fact about the run and it is worth seeing in a window — but it is
        // harness speech, and the window's transcript vocabulary has no variant for it that is not
        // model prose. It reaches a window through the daemon's `Degraded` handling on the
        // conversation port instead, and is deliberately not duplicated here.
        // An announcement is about the DAEMON, not about this run, and a run window that
        // repeated the daemon's startup log would be a second copy of a different subject.
        // `Cadence` is about the turn the CONVERSATION is having; a watched run has its own.
        Event::Announce(_)
        | Event::Cadence { .. }
        | Event::Degraded { .. }
        | Event::User { .. }
        | Event::Status(_)
        | Event::Approval { .. }
        | Event::Done { .. }
        | Event::Run { .. }
        | Event::Error { .. }
        | Event::RunDetail { .. }
        | Event::RunOutput { .. } => return None,
    })
}

/// M2's approval gate: **deny by default**.
///
/// A daemon with no interactive surface attached cannot ask, and a gate that auto-approved
/// because nobody was listening would make §8.2's "approvals are enforced by the harness" a
/// statement about a code path that never runs. The interactive gate arrives with the TUI.
struct DenyUnattended;

impl ApprovalGate for DenyUnattended {
    fn await_approval(&mut self, _radius: &BlastRadius) -> bool {
        false
    }
}

/// The interactive gate: **asks the client that opened this connection, and blocks.**
///
/// # Why it round-trips on the existing connection rather than waiting for `Request::Approve`
///
/// The daemon is serial — `serve` accepts one connection, serves it to completion, then accepts
/// the next. A client that answered on a *second* connection would be talking to a listener that
/// is not listening: the turn holding the first connection is exactly what is blocked. So the
/// approval travels on the socket that is already open. The daemon writes `Event::Approval` and
/// then reads one line back.
///
/// **It holds its own clones of the socket**, rather than sharing the event writer. Two mutable
/// borrows of one writer would not compile, and the alternative — routing the prompt through the
/// event callback and the answer through somewhere else — would split one exchange across two
/// mechanisms. Writes are sequential on one thread, so ordering on the wire is preserved.
///
/// **Every failure is a denial.** A hung-up client, a malformed reply, a reply naming a different
/// decision: all return `false`. §8.2 puts enforcement in the harness, and a gate that approved
/// because it could not hear the answer would be enforcement in name only.
struct SocketApprovals {
    writer: TcpStream,
    reader: BufReader<TcpStream>,
    next_decision: u64,
    /// The reason given with the most recent decline, read back by the loop.
    last_reason: Option<String>,
}

impl SocketApprovals {
    fn new(writer: TcpStream, reader: BufReader<TcpStream>) -> Self {
        Self { writer, reader, next_decision: 1, last_reason: None }
    }
}

impl ApprovalGate for SocketApprovals {
    /// A client is on the other end of this socket, so somebody can be asked.
    fn is_interactive(&self) -> bool {
        true
    }

    fn decline_reason(&self) -> Option<String> {
        self.last_reason.clone()
    }

    fn await_approval(&mut self, radius: &BlastRadius) -> bool {
        let decision = self.next_decision;
        self.next_decision += 1;

        // §B9 wants the blast radius stated rather than the command. `scope` carries every
        // declared Target — including the numeric ones, which `ArgValue::render` made visible in
        // M2 C2f; before that a spend ceiling was silently absent from this line.
        //
        // **`novelty` is carried as an Option and is NOT defaulted.** §B9 asks for a novelty
        // reason and a ceiling; the ceiling has no producer yet (the trust ledger is M6). Sending
        // `"routine"` because nothing said otherwise would be a claim about promotion logic
        // nobody has written. Absent renders as absent.
        let prompt = Event::Approval {
            decision,
            verb: radius.verb.clone(),
            scope: radius.scope.clone(),
            reversible: radius.reversible,
            novelty: radius.novelty.as_ref().map(|n| format!("{n:?}")),
        };
        if crate::protocol::write_line(&mut self.writer, &prompt).is_err() {
            return false;
        }

        // Cleared before the ask, so a reason from an earlier decline cannot be reported against
        // this one.
        self.last_reason = None;

        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) | Err(_) => false,
            Ok(_) => match serde_json::from_str::<Request>(line.trim()) {
                // The reply must name the decision it is answering. A client that answered a
                // stale prompt would otherwise approve whatever is pending now.
                Ok(Request::Approve { decision: d, granted, reason }) if d == decision => {
                    if !granted {
                        self.last_reason = reason.filter(|r| !r.trim().is_empty());
                    }
                    granted
                }
                _ => false,
            },
        }
    }
}

/// A summarizer that refuses to invent a summary it did not produce.
struct PassthroughSummarizer;

impl Summarizer for PassthroughSummarizer {
    fn summarize(&mut self, view: &marlowe_loop::ContextView) -> String {
        // Until a cheap model is routed for this (ADR-008), compaction keeps the most recent
        // volatile text rather than fabricating a summary. Stated plainly because a summarizer
        // that silently returned "" would make governance survival untestable for the wrong
        // reason — it would pass while doing nothing.
        view.volatile
            .iter()
            .rev()
            .take(3)
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Everything a conversation carries from one turn to the next.
///
/// # Why this exists
///
/// It did not, and Marlowe could not summarize the conversation he was in. `ask_streaming` built
/// a fresh `SessionState` on **every** request, so the model was handed the identity block,
/// governance, and one user message — turn 1, every time. Asking "what did we just discuss?"
/// produced an honest answer about an empty history, which reads as amnesia and is worse than a
/// crash, because nothing in the system reports a fault.
///
/// The session **id** was already stable (`SessionId::from_name`), which is what made this hard to
/// see: everything downstream was correctly keyed to a conversation that was never stored.
///
/// `Provenance` travels with the state because it is the attribution of *those* blocks. Rebuilding
/// it per turn would leave the assembled history with no record of which parts came from the user,
/// which is what ADR-023's taint computation reads.
struct SessionMemory {
    state: SessionState,
    provenance: Provenance,
}

/// The one place a tool host is built.
///
/// **A single constructor is the fix for a real gap**, not a tidy-up. `verify_every_exposed_tool_is_runnable`
/// is only evidence about the turn if it is handed the same host the turn runs; two construction
/// sites is two hosts that can drift, and the drift is invisible in the direction that matters —
/// a runtime host missing an executor the verified one had would pass startup and fail on the call.
/// The registry the model's tool list is built from: the builtins, plus every tool an installed
/// MCP server contributed. ADR-052.
///
/// **One function, called from all three sites.** `builtin_registry()` used to be called
/// separately for the `Engine` and for each driver, which was already a duplication hazard and
/// becomes a real defect the moment the registry has a per-profile component: an `Engine` that
/// knows a tool and a driver that does not would offer the model nothing and refuse nothing, and
/// the symptom would be a tool the user installed that the model never mentions.
fn tool_registry(
    fleet: &std::sync::Arc<std::sync::Mutex<crate::mcp::McpFleet>>,
) -> Result<marlowe_tools::ToolRegistry, DaemonError> {
    let mut registry = builtin_registry().map_err(|e| DaemonError::Profile {
        root: "<builtins>".into(),
        detail: e.to_string(),
    })?;
    for reg in fleet.lock().expect("the mcp fleet lock was poisoned").registrations() {
        registry.register(reg.clone()).map_err(|e| DaemonError::Profile {
            root: "<mcp>".into(),
            detail: e.to_string(),
        })?;
    }
    Ok(registry)
}

type DaemonToolHost = crate::mcp::McpTools<
    crate::skills::SkillTools<crate::recall::RecallTools<FileSystemTools<WorkspaceScope>>>,
>;

fn build_tool_host(
    workspace: &std::path::Path,
    // The model's context window, forwarded to `read` so "this file is large" is measured against
    // the window the run actually has rather than a constant.
    context_tokens: u32,
    beliefs: std::sync::Arc<std::sync::Mutex<marlowe_memory::BeliefStore>>,
    skills: std::sync::Arc<std::sync::Mutex<marlowe_tools::skill::SkillRegistry>>,
    fleet: std::sync::Arc<std::sync::Mutex<crate::mcp::McpFleet>>,
) -> Result<DaemonToolHost, DaemonError> {
    let scope = WorkspaceScope::new().map_err(|e| DaemonError::Scope { detail: e.to_string() })?;
    // **The wrapper order is not free.** Each layer serves its own tool and delegates the rest,
    // and each MUST forward `execute_batch` or the innermost host's concurrent fetch stops
    // running — `RecallTools::execute_batch` documents that trap one layer in, and adding a second
    // wrapper is exactly the event it warns about. A link that forwards to a link that does not is
    // as broken as one that does not forward, so the assertion is on the whole chain:
    // `tests/composition_root.rs::a_batch_reaches_the_innermost_host_through_both_wrappers`.
    //
    // **That name is on one line deliberately.** This comment named the test before the test
    // existed, AND it wrapped the name across a line break — so a grep for the name found this
    // comment and nothing else, and a claim about a test survived having no test behind it. Both
    // mutations (`skilltools_batch`, `mcptools_batch`) now fail it.
    Ok(crate::mcp::McpTools::new(
        crate::skills::SkillTools::new(
            crate::recall::RecallTools::new(
                // **The same window the driver sends as `num_ctx` and the assembler sizes its
                // view from.** `read` uses it to decide whether a file is large enough to warn
                // about; a fixed threshold would warn a 200k-context model about a file that is a
                // rounding error to it.
                FileSystemTools::new(scope, workspace.to_path_buf())
                    .with_context_tokens(context_tokens),
                beliefs,
            ),
            skills,
        ),
        fleet,
    ))
}

/// Load the profile's installed skills, and say what refused.
///
/// **Scanned once, at startup, and that is a decision rather than an omission.** Rescanning per
/// turn would let a newly dropped `SKILL.md` work without a restart, which is nicer — but a
/// malformed skill's refusal would then have nowhere to go: produced on the turn path, where
/// there is nothing to print it to, once per turn, forever. One scan in one place where the
/// refusals can be seen is worth the restart. ADR-051 §6.
fn load_skills(
    profile_root: &std::path::Path,
) -> (marlowe_tools::skill::SkillRegistry, Vec<marlowe_tools::skill::SkillError>) {
    // **The timestamp comes from the fence, not from the filesystem.** `skill::scan` used to
    // read each file's mtime, which `determinism_guard` refused on the first workspace run
    // after it was written -- correctly: a `UserReviewed { at }` reaches a manifest, and a
    // stray clock read makes every decay-dependent result irreproducible. See `crate::clock`.
    let reviewed_at = marlowe_loop::ClockSource::now_ms(&mut crate::clock::SystemClock);
    marlowe_tools::skill::scan(&profile_root.join("skills"), reviewed_at)
}

pub struct Daemon {
    config: DaemonConfig,
    /// One log, two writers inside a turn: the loop's recorder and the memory host.
    journal: std::sync::Arc<std::sync::Mutex<Journal>>,
    /// M2 Session D. `memory: None` used to be passed to every turn — which was concealing that
    /// there was no single-claim write path to wire, not merely that it was unwired.
    memory: crate::memory::DaemonMemory,
    /// The run table and the durable control plane, shared with the control listener.
    ///
    /// **The run table moved in here rather than staying a plain field**, and that is the change
    /// that makes `/steer` mean anything: the turn in flight and the control connection reach the
    /// same object, so a steer written by one is seen by the other at the next iteration boundary.
    /// A copy on each side would be two answers to one question — the shape `--status` produced.
    plane: crate::control_plane::Shared,
    /// Keyed by the client's session name — the same key `SessionId::from_name` derives from.
    sessions: BTreeMap<String, SessionMemory>,
    shutdown: Arc<AtomicBool>,
    /// The installed skills, scanned once at startup. ADR-051; see [`load_skills`].
    skills: std::sync::Arc<std::sync::Mutex<marlowe_tools::skill::SkillRegistry>>,
    /// The connected MCP servers and the tools they contributed. ADR-052.
    mcp: std::sync::Arc<std::sync::Mutex<crate::mcp::McpFleet>>,
    /// Servers that failed to connect, and tools whose description changed since install.
    ///
    /// **Both are the user's to act on**, so both are kept rather than logged and dropped: a
    /// server that did not connect and a description that changed under an approved name are the
    /// two ways an installed tool stops being what the user agreed to.
    mcp_notices: Vec<String>,
    /// What refused to load during that scan, kept so the startup surface can say so.
    ///
    /// **Held rather than logged and dropped.** A skill the user installed and cannot find is the
    /// failure this avoids, and "it is in the scrollback somewhere" is not an answer.
    skill_refusals: Vec<String>,
    /// The socket token. See [`crate::auth`] — loopback is per-machine, not per-user.
    ///
    /// Held on the daemon rather than re-read per connection: a token file replaced under a running
    /// daemon must not change who it will serve, and re-reading would make that possible for anyone
    /// who could write the profile root.
    token: String,
    /// **ADR-060: which engine is actually serving, and the latched reason when it is not the one
    /// the user picked.** See [`crate::engine::HybridEngine`].
    ///
    /// It is state on the daemon rather than on `DaemonConfig` because the config is the
    /// *declaration* — what was asked for — and this is the *resolution*. Collapsing them would
    /// force a choice between two lies: a picker that silently flips to `ollama`, changing the
    /// user's own setting with no record of who did it, or a screen claiming llama.cpp is serving
    /// when Ollama is.
    engine: crate::engine::HybridEngine,

    /// **Set by `set_provider`, consumed by [`Self::start_pending_engine`].** `Some` means a
    /// hybrid switch has been ACCEPTED and its engine has not been started yet.
    ///
    /// It exists because starting the engine takes seconds — a spawn, a health wait, an offload
    /// measurement — and `set_provider` is called on the thread that answers the control port.
    /// Doing it inline froze the surface from the moment the slash command was sent until the
    /// engine either came up or gave up. The switch is now acknowledged first and the engine
    /// started second, so the user sees the provider change immediately and the engine's verdict
    /// when it arrives.
    ///
    /// The `String` is the sampler disclosure, resolved during the switch. Carried rather than
    /// recomputed because resolving it reads Ollama's store, and doing that twice would put a
    /// second child process on the path this change exists to shorten.
    pending_engine: Option<String>,

    /// When this daemon came up, from the one fenced clock. §B7 lists *daemon uptime* among what
    /// the Status tab holds; `StatusReport::uptime_ms` is the difference, and it is a duration
    /// rather than this stamp precisely so no clock reading crosses the boundary.
    started_ms: i64,

    /// Turns this engine has answered. **Zero means the next turn is the first one.**
    ///
    /// It is what `Event::Cadence`'s `warm` reports, and it is deliberately a count of turns rather
    /// than a claim about weights: the harness cannot see whether the model was paged in from disk,
    /// and it can see exactly which turn this is. Starting an engine and switching model both reset
    /// it, because both replace the thing being measured.
    turns_on_engine: u32,
}

impl Daemon {

    /// **Start the engine for a hybrid switch that has already been acknowledged.** No-op unless
    /// [`Self::pending_engine`] is set.
    ///
    /// Split out of `set_provider` deliberately: see the field's own note. The caller emits a
    /// `Status` between the two, which is what unfreezes the surface.
    pub fn start_pending_engine(&mut self) {
        let Some(note) = self.pending_engine.take() else { return };
        let ModelProviderChoice::LlamaCpp { endpoint, .. } = self.config.model_provider() else {
            return;
        };
        self.engine = crate::engine::HybridEngine::start(
            &self.config.model,
            &endpoint,
            self.config.context_tokens,
        );
        // ADR-029: announced, never inferred. Both halves of the hybrid are named, because the
        // point of one entry with two names is that the user can see both.
        // **A new engine is a new denominator.** `warm` reports whether this engine has answered
        // before, so replacing it must reset the count -- otherwise the first turn on the new
        // engine is labelled warm and whatever the load cost reads as the engine being slow.
        self.turns_on_engine = 0;
        crate::announce::info(format!(
            "model provider {} · Ollama stores and lists · {} · {note}",
            marlowe_view::provider::HYBRID,
            self.engine.disclosure(),
        ));
        if self.engine.fallback_line().is_none() {
            crate::announce::info(
                "llama.cpp renders the GGUF's own chat template and parses its own \
                 tool-call dialect. Ollama's renderer and parser are NOT in this path, so the \
                 tool-call reliability recorded for this model does not describe it.",
            );
        }
    }

    /// How many history blocks this conversation is carrying. **Test surface for the session
    /// store** — the amnesia bug was invisible from outside because the session *id* was stable
    /// while the state behind it was not.
    pub fn session_turn_count(&self, session: &str) -> usize {
        self.sessions.get(session).map_or(0, |m| m.state.history_len())
    }

    /// Append a user turn to a session's history without calling a model.
    ///
    /// Exists so the store can be driven in a test that must not depend on Ollama being up.
    pub fn remember_user_turn(&mut self, session: &str, message: &str) {
        let session_id = SessionId::from_name(session);
        let memory = self.sessions.remove(session).unwrap_or_else(|| SessionMemory {
            state: SessionState::new(session_id, identity_block()),
            provenance: Provenance::new(),
        });
        let SessionMemory { mut state, mut provenance } = memory;
        provenance.attribute_user_message(message);
        state.push(marlowe_loop::Block::new(
            marlowe_loop::SourceKind::History,
            message,
            TrustClass::UserAsserted,
        ));
        self.sessions.insert(session.to_string(), SessionMemory { state, provenance });
    }

    pub fn open(config: DaemonConfig) -> Result<Self, DaemonError> {
        // Path scoping must be constructible before anything else. A daemon that started on an
        // unverified platform and refused paths later would present as broken tools.
        WorkspaceScope::new().map_err(|e| DaemonError::Scope { detail: e.to_string() })?;

        let profile = if config.profile_root.join("profile.json").exists() {
            Profile::open(&config.profile_root)
        } else {
            Profile::init(&config.profile_root)
        }
        .map_err(|e| DaemonError::Profile {
            root: config.profile_root.display().to_string(),
            detail: e.to_string(),
        })?;

        let journal = Journal::open(&profile).map_err(|e| DaemonError::Profile {
            root: config.profile_root.display().to_string(),
            detail: e.to_string(),
        })?;

        // **Shared, because one turn has two writers.** The loop records through it and the memory
        // host signs through it, and a second `Journal` would be a second append-only log — the
        // one thing this project's core abstraction says there is exactly one of. See
        // `marlowe_loop::record::SharedJournalRecorder`.
        let journal = std::sync::Arc::new(std::sync::Mutex::new(journal));

        // **Computed BEFORE the embedder loads, and from a reading rather than an assumption.**
        //
        // `LlamaServerLoaded` claims the weights are already out of `memory.free`. Ask what that
        // would report if the server were NOT up: zero reserve, with a confident reason -- the
        // exact family this fix exists to close. So it is only reached when `/health` answers 200,
        // which `llama-server` gives only once the model is IN, taken moments before the embedder
        // resolves. Anything else is `NotOnThisCard`, which under-reserves; the two failures are
        // not symmetric, and under-reserving surfaces as a llama-server that will not allocate --
        // loud, and actionable -- where over-reserving surfaces as an embedder silently on CPU.
        // ── THE ENGINE STARTS HERE, BEFORE THE EMBEDDER LOADS. ────────────────────────────
        //
        // **DO NOT MOVE THIS LATER TO MAKE STARTUP FEEL FASTER. The ordering IS the fix.**
        //
        // `llama-server` takes ~9.5 GB when it offloads a 9B. Starting it first means the embedder
        // resolves against a `memory.free` that is **already net of the language model**, and the
        // tier-1 reserve then correctly adds nothing on top. That is the double-count closed by
        // **sequencing** rather than by arithmetic — and the distinction matters, because the
        // arithmetic alternative (reserve the model's size, then subtract it again when a
        // llama-server is up) is a compensation, and a compensation drifts the moment either side
        // changes. This does not: whatever the engine took, it took before anyone measured.
        //
        // The cost is ~1.6 s of daemon startup on a warm page cache. It is spent where a person
        // expects to wait — at start — and it buys the state every later reading depends on.
        // Deferring it to the first turn would put the same 1.6 s in front of the first answer AND
        // make the embedder resolve against a card the language model had not claimed yet.
        //
        // It is also where the fallback fires. `HybridEngine::start` never fails: it either serves
        // or latches a reason, so a daemon whose `llama-server` will not run still opens, still
        // answers, and says which engine is doing it.
        let engine = match config.model_provider() {
            ModelProviderChoice::LlamaCpp { endpoint, .. } => crate::engine::HybridEngine::start(
                &config.model,
                &endpoint,
                config.context_tokens,
            ),
            _ => crate::engine::HybridEngine::NotSelected,
        };
        // **A function, not seven lines inline, so a test can read the deciding code.** See
        // `crate::engine::tier1_runtime_for` for the four answers and for which one was wrong.
        let tier1_runtime = crate::engine::tier1_runtime_for(&config.model_provider(), &engine);

        // **Beliefs are rebuilt from the log here**, which is what makes memory durable across a
        // restart without any new persistence machinery. A derivation failure stops the daemon
        // rather than starting it with an empty store: a silently forgotten store is
        // indistinguishable from a first run, and would report itself healthy.
        let memory = crate::memory::DaemonMemory::open(
            std::sync::Arc::clone(&journal),
            profile.manifest().derivation_version,
            config.reranking.as_deref(),
            // **The tier-1 model this daemon routes to, so the rerank yields to it.** ADR-045: the
            // language model has no CPU fallback and the reranker does, so the reranker is the one
            // that gives way. `config.model` rather than the compile-time default because
            // `switch_model` can change it.
            &config.model,
            // **WHICH PROCESS holds tier 1, not only which model.** ADR-060 §3, and it closes a
            // defect that was already shipped: this line passed a name unconditionally, including
            // on the OpenRouter path where nothing on this machine runs the model, so the embedder
            // reserved ~5.8 GB against a card with nothing on it and resolved to CPU with a
            // coherent, false reason.
            //
            // Derived from `model_provider()` -- the ONE function that decides which provider is
            // in use -- so this cannot disagree with the run path about who is serving.
            tier1_runtime,
        )
        .map_err(|e| DaemonError::Profile {
            root: config.profile_root.display().to_string(),
            detail: format!("the belief store could not be derived from the journal: {e}"),
        })?;
        // **Read from the resolution, never derived here.** ADR-029 forbids a second producer of
        // this fact and `DaemonMemory::rerank_provider_label` is the first. Assigned once, at
        // startup, immediately after the load that decided it.
        let mut config = config;
        config.rerank_provider = memory.rerank_provider_label();

        // **Refuse to start rather than offer a tool that cannot run.** A daemon that starts and
        // then fails every `recall` call presents as a broken model; this names the tool instead.
        //
        // **It verifies the host it will actually use.** Until M2 Session D this checked a bare
        // `FileSystemTools` while the turn ran a different object — and when `recall` gained its
        // executor the guard refused a daemon that could in fact run it. It failed loudly, which
        // was luck: the same gap in the other direction — a runtime host with FEWER executors than
        // the verified one — would pass here and fail on the turn, which is precisely the `done`
        // defect that cost a run 155 seconds. `build_tool_host` is now the only way to construct
        // one, so the two cannot differ.
        // **The skills are loaded BEFORE the guard below**, because `use` is now in the exposed
        // set and the guard reads the host it will actually use. An empty registry still satisfies
        // it — `SkillTools` executes `use` whether or not any skill is installed, and answers
        // "nothing is installed" rather than failing — which is the right split: having no skills
        // is a state, and being unable to run `use` is a fault.
        let (skills, skill_errors) = load_skills(&config.profile_root);
        let skill_refusals: Vec<String> = skill_errors.iter().map(|e| e.to_string()).collect();
        let skills = std::sync::Arc::new(std::sync::Mutex::new(skills));

        // ── ADR-052: the MCP servers the user installed ──────────────────────────────────
        //
        // **A malformed `mcp.json` refuses to start.** Starting with the servers a broken parse
        // happened to reach would give the user half their tools and no statement that anything
        // went wrong -- the shape this file already refuses for the belief store.
        let specs = crate::mcp::read_config(&config.profile_root)
            .map_err(|detail| DaemonError::Profile {
                root: config.profile_root.display().to_string(),
                detail,
            })?;
        let mut pins = crate::mcp::read_pins(&config.profile_root);
        let (fleet, mcp_errors) = crate::mcp::McpFleet::connect(&specs, &mut pins);
        crate::mcp::write_pins(&config.profile_root, &pins);
        let mcp_notices: Vec<String> = mcp_errors
            .iter()
            .map(|e| e.to_string())
            .chain(fleet.reconsent().iter().cloned())
            .collect();

        // **The profile is WIDENED by what the servers contributed, and the budget can refuse.**
        // Eleven builtins of thirteen leaves room for two MCP tools (ADR-058); a third is
        // `ExposureError::TooMany`, which names the budget and the remedy rather than dropping the
        // overflow silently. A server whose third tool vanished would look like a broken server.
        let profile = CapabilityProfile::interactive_with(fleet.tool_ids()).map_err(|e| {
            DaemonError::Profile {
                root: config.profile_root.display().to_string(),
                detail: format!(
                    "the installed MCP servers do not fit the exposed-tool budget: {e}"
                ),
            }
        })?;
        let fleet = std::sync::Arc::new(std::sync::Mutex::new(fleet));

        marlowe_loop::verify_every_exposed_tool_is_runnable(
            profile.exposed_tools(),
            &build_tool_host(
                &config.workspace,
                config.context_tokens,
                memory.beliefs(),
                std::sync::Arc::clone(&skills),
                std::sync::Arc::clone(&fleet),
            )?,
        )?;

        // **Minted before the listener binds, and a failure here refuses to start.** A daemon that
        // could not establish a token would otherwise have to choose between serving everyone and
        // serving no one, and the first of those is the vulnerability this closes.
        let token = crate::auth::ensure_token(&config.profile_root).map_err(|e| {
            DaemonError::Profile {
                root: config.profile_root.display().to_string(),
                detail: format!("the socket token could not be established: {e}"),
            }
        })?;

        // **Seeded from the journal, or a run that survived a restart is unfindable.** The run
        // table is in memory; after a restart it is empty, so `/runs` would list nothing and
        // `--resume` would need an id nobody could produce. See `seed_from_journal` for why they
        // are seeded as `interrupted` rather than `running`.
        let plane = crate::control_plane::ControlPlane::new(marlowe_loop::DurableControl::new(
            // The journal is the substrate: a checkpoint is an `EventKind::Checkpointed` event in
            // the one append-only log, and `JournalCheckpoints` is the read.
            marlowe_loop::JournalCheckpoints::new(std::sync::Arc::clone(&journal)),
        ));
        let resumable = plane.lock().expect("fresh").seed_from_journal();
        if resumable > 0 {
            // **A warning, not a fact.** Work was interrupted and is sitting there unfinished,
            // which is the definition of something wanting attention -- §B2's amber.
            crate::announce::warn(format!(
                "{resumable} interrupted run(s) can be resumed — `marlowe --runs` lists them, \
                 `marlowe --resume <id>` continues one"
            ));
        }

        Ok(Self {
            config,
            plane,
            journal,
            memory,
            sessions: BTreeMap::new(),
            shutdown: Arc::new(AtomicBool::new(false)),
            skills,
            skill_refusals,
            mcp: fleet,
            mcp_notices,
            token,
            engine,
            pending_engine: None,
            // §4.5's legitimate case, through the one fence. See `crate::clock`.
            started_ms: marlowe_loop::ClockSource::now_ms(&mut crate::clock::SystemClock),
            turns_on_engine: 0,
        })
    }

    /// How many skills loaded. Printed at startup, beside the memory state.
    pub fn skills_installed(&self) -> usize {
        self.skills.lock().expect("the skill registry lock was poisoned").len()
    }

    /// What refused to load, in the words of the refusal. Kept rather than logged and dropped: a
    /// skill the user installed and cannot find is the failure this avoids, and "it is in the
    /// scrollback somewhere" is not an answer.
    pub fn skill_refusals(&self) -> &[String] {
        &self.skill_refusals
    }

    /// How many tools the installed MCP servers contributed. ADR-052.
    pub fn mcp_tools(&self) -> usize {
        self.mcp.lock().expect("the mcp fleet lock was poisoned").registrations().len()
    }

    /// How many servers answered.
    pub fn mcp_servers(&self) -> usize {
        self.mcp.lock().expect("the mcp fleet lock was poisoned").servers()
    }

    /// Servers that failed to connect, and tools whose description changed since install.
    pub fn mcp_notices(&self) -> &[String] {
        &self.mcp_notices
    }

    /// What the retrieval half of memory is doing. Printed at startup; see
    /// [`crate::memory::RetrievalState`].
    pub fn memory_state(&self) -> &crate::memory::RetrievalState {
        self.memory.state()
    }

    pub fn shutdown_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shutdown)
    }

    pub fn live_runs(&self) -> usize {
        self.plane.lock().expect("the control plane lock was poisoned").live_runs()
    }

    /// How long this daemon has been up. §B7 lists *daemon uptime*; §B5's `idle` band carries it.
    ///
    /// **Saturating, so a clock that steps backwards reads zero rather than four billion.**
    /// `SystemClock` is wall time, not monotonic — an NTP correction mid-session is not
    /// hypothetical, and `0 ms` is a visibly wrong number a reader dismisses, where
    /// `4294967295 ms` is a plausible-looking one they believe.
    fn uptime_ms(&self) -> u64 {
        marlowe_loop::ClockSource::now_ms(&mut crate::clock::SystemClock)
            .saturating_sub(self.started_ms)
            .max(0) as u64
    }

    /// What §B5's band and first-run onboarding need, without touching a model.
    ///
    /// # An exhaustive `match`, and that is the change ADR-060 had to make first
    ///
    /// This was `if let ModelProviderChoice::OpenRouter { .. } { ... return }` followed by the
    /// Ollama tail. Adding a third provider compiles clean against that shape and **falls through
    /// to the Ollama branch**: it would probe `127.0.0.1:11434`, offer Ollama's `/api/tags` as the
    /// model picker, and hand back `capability_for(&config.model)` — the 12/12 measured on
    /// 2026-08-08 **through Ollama's own renderer and parser**, for a runtime that uses neither.
    /// One runtime's measured reliability disclosed under another's name, with nothing failing.
    ///
    /// A `match` makes a fourth provider a compile error instead. Same for
    /// [`Self::set_model`], `marlowe::tui::spawn_args` and `marlowe::agent::serve`, which had the
    /// identical shape.
    pub fn status(&self) -> StatusReport {
        match self.config.model_provider() {
            ModelProviderChoice::OpenRouter { model } => self.status_openrouter(model),
            ModelProviderChoice::LlamaCpp { endpoint, .. } => self.status_hybrid(endpoint),
            ModelProviderChoice::Ollama => self.status_ollama(),
        }
    }

    /// **Answers without touching the network.** A probe here would put a round trip in front of
    /// every `--status`, which the surface hits on essentially every tick, and the failures worth
    /// catching early — no key, no model named — need no network to see.
    fn status_openrouter(&self, model: String) -> StatusReport {
        {
            let availability = marlowe_openrouter::Availability::check(
                &model,
                &marlowe_openrouter::ApiKey::from_environment(),
            );
            return StatusReport {
                version: env!("CARGO_PKG_VERSION").to_string(),
                workspace: self.config.workspace.display().to_string(),
                model: model.clone(),
                // **NOT MEASURED, always.** `capability_for` would hand back `qwen3.5:9b`'s
                // 12/12 from 2026-08-08 for any name it does not recognise -- no: it returns
                // `unmeasured`, and that is the correct answer here for the same reason. Nothing
                // about any OpenRouter model has been measured on this machine.
                model_disclosure: marlowe_provider::ModelCapability::unmeasured(&model)
                    .disclosure(),
                degraded: degraded_line(
                    (!availability.is_ready()).then(|| availability.remedy()),
                    crate::staleness::stale_against_source(),
                ),
                rerank_provider: self.config.rerank_provider.clone(),
                model_provider: self.config.model_provider().name().to_string(),
                live_runs: self.live_runs(),
                announcements: crate::announce::retained(),
                uptime_ms: self.uptime_ms(),
                // **The catalogue when we have it, the configured slug when we do not.**
                //
                // This paragraph used to say the opposite -- *"OpenRouter's catalogue is hundreds
                // of models behind a network call; presenting the one configured slug as though it
                // were a list would be a control that offers one choice"* -- and it was right about
                // the problem and wrong about the only way out. The network call is real and must
                // not happen here, on a path the surface hits every tick. It happens **once, on
                // the switch**, and this reads the cache. ADR-049 §7.
                //
                // The fallback is the configured slug rather than an empty list, for the reason
                // the Ollama branch below keeps the configured model selectable: a picker that
                // omits the value it is currently reporting is internally inconsistent.
                models: if self.config.openrouter_models.is_empty() {
                    vec![model]
                } else {
                    let mut m = self.config.openrouter_models.clone();
                    if !m.iter().any(|x| *x == model) {
                        m.push(model);
                        m.sort();
                    }
                    m
                },
            };
        }
    }

    /// **The hybrid: Ollama stores and lists, `llama-server` serves — and this is where the user
    /// finds out which of those is actually true right now.** ADR-060.
    ///
    /// # Three things this arm does that the third-provider version did not
    ///
    /// * **The model list comes from Ollama**, not from `vec![config.model]`. That is the decision:
    ///   Ollama is the inventory. Under the old shape `llama-server` served one model chosen at
    ///   launch, so a one-entry picker was the truth; now `/model` restarts the engine, so every
    ///   model Ollama has pulled is selectable and the picker says so.
    /// * **The fallback line leads the degraded field and persists.** Not a flash — §B5 renders
    ///   `degraded` in amber on every frame, so a user who looks ten minutes later still reads
    ///   which engine is serving and why it is not the one they picked.
    /// * **The offload reading is CARRIED, never re-measured here.** Taking it costs a real
    ///   generation, and this function is on the path the surface hits every tick. The reading is a
    ///   fact about the server process we are still holding — a weaker claim than a fresh
    ///   measurement, and the type says which one it is.
    fn status_hybrid(&self, endpoint: LocalEndpoint) -> StatusReport {
        // **Ollama answers the inventory question in both states**, because Ollama is the store
        // whether or not llama.cpp is the engine. This is the same call the default provider makes
        // and it costs the same.
        let mut models: Vec<String> = match Routing::uniform(&self.config.model) {
            Ok(r) => match Availability::probe(&LocalEndpoint::default_ollama(), &r) {
                Availability::Ready { models } => models,
                Availability::ModelMissing { available, .. } => available,
                _ => Vec::new(),
            },
            Err(_) => Vec::new(),
        };
        models.retain(|m| !marlowe_provider::is_cloud_tag(m));
        if !models.iter().any(|m| *m == self.config.model) {
            models.push(self.config.model.clone());
        }
        models.sort();

        // **The engine's own state first, and it does not touch the network.** A fallen-back
        // engine has a latched sentence; a serving one is re-probed with its CARRIED offload
        // reading, which catches a server that died without the daemon having taken a turn since.
        let engine_trouble = match self.engine.fallback_line() {
            Some(line) => Some(line.to_string()),
            None => match self.config.model_provider() {
                ModelProviderChoice::LlamaCpp { .. } => {
                    let a = marlowe_provider::llamacpp::Availability::probe(
                        &endpoint,
                        &self.config.model,
                        self.config.context_tokens,
                        marlowe_provider::OffloadPolicy::Carried(
                            self.engine.offload().unwrap_or(marlowe_provider::Offload::Unknown),
                        ),
                    );
                    (!a.is_ready()).then(|| a.remedy())
                }
                _ => None,
            },
        };

        StatusReport {
            version: env!("CARGO_PKG_VERSION").to_string(),
            workspace: self.config.workspace.display().to_string(),
            model: self.config.model.clone(),
            // **Names the engine that is serving, in the line where the model is named.** The
            // live CPU defect's status line read *"serving … template reports tool support"* —
            // every clause true, and nothing in it could have revealed the wrong processor. This
            // one carries the engine and the offload reading.
            //
            // **And the measurement-transfer caveat is applied only while it is TRUE.**
            // `disclosure_for` says the recorded tool-call figure was measured through Ollama's
            // own renderer and parser and so does not describe llama.cpp. That is right when
            // llama.cpp is serving. **When the engine has fallen back, Ollama IS the renderer and
            // the parser**, so the figure describes this run exactly — and printing "not measured
            // for this runtime" there would be the caveat applied to the wrong system, which is
            // the same error it exists to prevent, mirrored.
            model_disclosure: match self.engine.fallback_line() {
                Some(_) => format!(
                    "{} · {}",
                    capability_for(&self.config.model).disclosure(),
                    self.engine.disclosure(),
                ),
                None => format!(
                    "{} · {}",
                    marlowe_provider::llamacpp::disclosure_for(&self.config.model),
                    self.engine.disclosure(),
                ),
            },
            degraded: degraded_line(engine_trouble, crate::staleness::stale_against_source()),
            rerank_provider: self.config.rerank_provider.clone(),
            model_provider: self.config.model_provider().name().to_string(),
            live_runs: self.live_runs(),
            models,
            announcements: crate::announce::retained(),
            uptime_ms: self.uptime_ms(),
        }
    }

    fn status_ollama(&self) -> StatusReport {
        let endpoint = LocalEndpoint::default_ollama();
        let routing = Routing::uniform(&self.config.model);
        // **One probe, two answers.** The availability check already enumerates what the endpoint
        // holds, so asking again for the picker would be a second source of the same fact — the
        // shape ADR-029 forbids for `rerank_provider` and the shape this project has logged a dozen
        // instances of.
        let mut models: Vec<String> = Vec::new();
        let degraded = match &routing {
            Err(e) => Some(e.to_string()),
            Ok(r) => {
                let a = Availability::probe(&endpoint, r);
                match &a {
                    Availability::Ready { models: m } => models = m.clone(),
                    Availability::ModelMissing { available, .. } => models = available.clone(),
                    _ => {}
                }
                (!a.is_ready()).then(|| a.remedy())
            }
        };
        // Cloud tags are dropped rather than shown: `Routing::uniform` refuses them, so offering
        // one in a picker builds a control whose only outcome is a refusal. `marlowe --models`
        // lists them *and* marks them refused, which is the right place for that — a list a person
        // reads, not a control a person operates.
        models.retain(|m| !marlowe_provider::is_cloud_tag(m));
        // The configured model is always selectable, even with the endpoint down. A picker that
        // omitted the value it is currently reporting would be internally inconsistent.
        if !models.iter().any(|m| *m == self.config.model) {
            models.push(self.config.model.clone());
        }
        models.sort();
        // **THE ENGINE FALLBACK LEADS, AND THIS ARM IS REACHED PRECISELY WHEN IT MATTERS.**
        // A hybrid whose `llama-server` could not start is *served by Ollama*, so `status()`
        // dispatches here -- and without this line the degraded field carried Ollama's own
        // availability remedy and **never named the engine at all**. The user picked
        // `ollama/llama.cpp`, the picker still shows it, and this sentence was the only thing that
        // could say the engine half is not happening. It was built, wired at eight other sites,
        // and missing from the one arm the fallback actually routes through.
        //
        // Ordered by what the user must act on first: the engine, then whatever Ollama has to say
        // about itself, then staleness. `degraded_line` composes with a blank line and `Also:`, so
        // none of the three is lost when more than one is present.
        let degraded = degraded_line(self.engine.fallback_line().map(str::to_string), degraded);
        // **Announced, loudly, and ahead of everything else.** A daemon serving stale code
        // produces symptoms that look like bugs in whatever was just changed, and the reflex is to
        // debug the change. Invariant 4's rule applies: degrade visibly, and name the remedy.
        let degraded = degraded_line(degraded, crate::staleness::stale_against_source());
        StatusReport {
            version: env!("CARGO_PKG_VERSION").to_string(),
            workspace: self.config.workspace.display().to_string(),
            model: self.config.model.clone(),
            // **Keyed to the model this daemon actually routes to.** Handing back the default's
            // measured reliability for a user-chosen model would report one model's number under
            // another's name -- see `capability_for`.
            model_disclosure: capability_for(&self.config.model).disclosure(),
            degraded,
            rerank_provider: self.config.rerank_provider.clone(),
            model_provider: self.config.model_provider().name().to_string(),
            live_runs: self.live_runs(),
            models,
            announcements: crate::announce::retained(),
            uptime_ms: self.uptime_ms(),
        }
    }

    /// Switch the model this daemon routes to. **Refused by name if the endpoint does not have it.**
    ///
    /// A silent accept would leave the picker showing a model that every subsequent turn fails
    /// against, and the failure would present as a broken model rather than as a bad choice.
    pub fn set_model(&mut self, model: &str) -> Result<(), String> {
        // **An exhaustive match, for the reason `status` is one.** As an `if let` on OpenRouter,
        // a llamacpp daemon fell through to `Availability::probe(&LocalEndpoint::default_ollama())`
        // and accepted or refused models on the authority of what OLLAMA has pulled -- which for
        // this provider is a question about the wrong process.
        match self.config.model_provider() {
            ModelProviderChoice::LlamaCpp { endpoint, .. } => {
                if model == self.config.model {
                    return Ok(());
                }
                // ── The engine restarts. ~1.59 s warm, and it is SYNCHRONOUS. ────────────
                //
                // # Why not a loading state, and why not a refusal
                //
                // Under Ollama `/model` is a validated config write and the runner stays hot. Here
                // the model is the argv `llama-server` was started with, so switching means
                // stopping a process and starting another — 1.59 s warm, longer cold.
                //
                // **A refusal was the old behaviour and it belongs to the old design.** It made
                // sense when Marlowe did not own the server: there was genuinely nothing a config
                // write could change. Now Marlowe owns it, so refusing would be declining to do
                // something it can do, and would leave `/model` meaning two different things
                // depending on a provider setting.
                //
                // **An asynchronous switch with a loading state was the other candidate and it is
                // the one that can lie.** `Request::SetModel` is answered with a fresh `Status`;
                // if this returned immediately, the picker would show the new model while the old
                // server was still the thing answering — an interval, however short, in which the
                // screen names a model that is not loaded. That is the exact failure this
                // function's header forbids, and a spinner does not fix it, it decorates it.
                //
                // Doing it synchronously means there is **no window at all**: the switch is not
                // acknowledged until a server answering on our port is serving the new blob and
                // has been measured on the GPU. The user waits ~1.6 s for a thing that takes
                // ~1.6 s. The elapsed time is announced so the wait is accounted for rather than
                // mysterious.
                let previous = self.config.model.clone();
                self.engine.stop();
                let engine = crate::engine::HybridEngine::start(
                    model,
                    &endpoint,
                    self.config.context_tokens,
                );
                let fallback = engine.fallback_line().map(str::to_string);
                match fallback {
                    None => {
                        self.engine = engine;
                        self.config.model = model.to_string();
                        self.turns_on_engine = 0;
                        crate::announce::info(format!(
                            "model {previous} -> {model} · {}",
                            self.engine.disclosure()
                        ));
                        return Ok(());
                    }
                    // **The switch still happens, and the engine falls back.** Ollama has the
                    // model — it is Ollama's store the name came from — so the user gets the model
                    // they asked for, served by the other half of the hybrid, with the reason
                    // latched in the band. Refusing the model because the *engine* would not start
                    // would be reporting an engine problem as a model problem.
                    Some(line) => {
                        self.engine = engine;
                        self.config.model = model.to_string();
                        self.turns_on_engine = 0;
                        // The model changed AND the engine could not serve it. The second half is
                        // the part that wants attention, so the whole line carries its level.
                        crate::announce::warn(format!("model {previous} -> {model}; {line}"));
                        return Ok(());
                    }
                }
            }
            ModelProviderChoice::Ollama | ModelProviderChoice::OpenRouter { .. } => {}
        }
        if let ModelProviderChoice::OpenRouter { model: current } = self.config.model_provider() {
            // **This used to refuse outright**, and the refusal was right while the picker could
            // only ever hold the machine's Ollama inventory: accepting a name would have put a
            // hosted daemon on a local slug and the 404 would have read like a provider fault.
            //
            // With `/provider` the picker holds openrouter.ai's own catalogue, so the name is a
            // hosted slug and setting it is the whole point. What survives from the old refusal is
            // its reason: a name that is **not** in the catalogue is still refused by name.
            if model == current {
                return Ok(());
            }
            let known = &self.config.openrouter_models;
            if !known.is_empty() && !known.iter().any(|m| m == model) {
                return Err(format!(
                    "`{model}` is not a model openrouter.ai lists. Pick one from `/model`, or see \
                     https://openrouter.ai/models"
                ));
            }
            self.config.model_provider = ModelProviderChoice::OpenRouter { model: model.to_string() };
            return Ok(());
        }
        if model == self.config.model {
            return Ok(());
        }
        let endpoint = LocalEndpoint::default_ollama();
        let routing = Routing::uniform(model).map_err(|e| e.to_string())?;
        match Availability::probe(&endpoint, &routing) {
            Availability::Ready { .. } => {
                self.config.model = model.to_string();
                Ok(())
            }
            other => Err(other.remedy()),
        }
    }

    /// Switch the provider this daemon routes to. **ADR-049 §7.**
    ///
    /// ADR-046 made the provider a launch-time choice, and it stayed one everywhere: `--tui`
    /// accepted `--provider` and threw it away, the Windows Terminal profile ignored it, and the
    /// only way to change your mind was to restart. This is the session-level answer.
    ///
    /// # Three things it refuses, each by name
    ///
    /// **An unknown provider**, against `project::PROVIDERS` -- the same list the picker is built
    /// from, so an option a user can see is an option this accepts.
    ///
    /// **OpenRouter with no key.** `Availability::check` is the existing reader of that, and it
    /// already names the remedy. Switching first and failing on the next turn would present a
    /// missing credential as a broken model.
    ///
    /// **OpenRouter with nothing to route to.** A slug is remembered from launch or from a
    /// previous switch; failing that, the catalogue's fetch supplies one; failing both, the switch
    /// is refused rather than leaving a hosted daemon with no model.
    ///
    /// # The catalogue is fetched HERE and nowhere else
    ///
    /// One round trip, at the moment a person asked for it. A failure does not fail the switch --
    /// the picker falls back to the configured slug, which is a usable daemon with a thin list
    /// rather than an unusable one with an excuse.
    /// The config, mutably. **For tests that need to place the daemon in a state a live switch
    /// would reach through a network round trip** — a hosted provider with a catalogue already
    /// fetched. Nothing in the product mutates the config through this; the product path is
    /// [`Self::set_provider`] and [`Self::set_model`], which validate.
    pub fn config_mut(&mut self) -> &mut DaemonConfig {
        &mut self.config
    }

    /// Where this daemon expects a local `llama-server`, and where its sampling comes from.
    /// **One definition**, so the probe, the turn path and the VRAM reserve cannot end up asking
    /// about different ports.
    ///
    /// An already-active `LlamaCpp` choice wins; otherwise the documented defaults, which is the
    /// case `/provider llamacpp` takes on a daemon launched as something else. A non-default port
    /// reached that way needs a relaunch with `--llamacpp-port`, and the remedy names the port it
    /// looked at either way.
    fn llamacpp_settings(&self) -> (LocalEndpoint, marlowe_provider::llamacpp::SamplingSource) {
        match self.config.model_provider() {
            ModelProviderChoice::LlamaCpp { endpoint, sampling } => (endpoint, sampling),
            _ => (
                marlowe_provider::llamacpp::default_endpoint(),
                marlowe_provider::llamacpp::SamplingSource::OllamaParams,
            ),
        }
    }

    pub fn set_provider(&mut self, provider: &str) -> Result<(), String> {
        // **`llamacpp` is a SPELLING of `ollama/llama.cpp`, not a second provider.** A person
        // typing `/provider llamacpp` at a prompt means the hybrid; refusing them over a slash
        // would be pedantry, and silently doing something else would be the thing this whole
        // change exists to prevent. Normalised here, once, so everything downstream — the match
        // below, the config, `name()`, the picker — sees exactly one string.
        //
        // **An alias is not a default.** Both spellings are things the user typed; neither is
        // reachable by omission, which is the property `--reranking off` and `--llamacpp-sampling`
        // are written to hold.
        let provider = if provider == "llamacpp" {
            marlowe_view::provider::HYBRID
        } else {
            provider
        };
        if !crate::project::PROVIDERS.contains(&provider) {
            return Err(format!(
                "`{provider}` is not a provider this build has. Options: {}",
                crate::project::PROVIDERS.join(", ")
            ));
        }
        // **Selecting the provider already in use is a no-op — EXCEPT for the hybrid, where it is
        // the retry, and this exception is not a special case so much as a bug report.**
        //
        // `EngineFailure::fallback_line` ends with *"`/provider ollama/llama.cpp` retries the
        // engine"*. That sentence is the one actionable clause in the persistent band, and the
        // early return above made it **false**: the provider was already `ollama/llama.cpp` — the
        // user never left it, only the engine fell back — so the gesture the product told them to
        // perform did nothing at all, silently, and the band went on saying it would work.
        //
        // Found by `a_hybrid_switch_whose_engine_cannot_start_falls_back_to_ollama_and_says_exactly_why`,
        // which asserts on the words the user reads. A test asserting `set_provider(...).is_ok()`
        // would have passed on this build: the call DID succeed, it just did not do anything.
        //
        // For `ollama` and `openrouter` the no-op is still right — re-selecting them costs a
        // catalogue fetch and changes nothing.
        if provider == self.config.model_provider().name() && provider != marlowe_view::provider::HYBRID
        {
            return Ok(());
        }
        match provider {
            "ollama" => {
                // **The slug is not thrown away.** Switching back to openrouter should not make
                // the user retype it, and `last_openrouter_model` is the field that would be, so
                // it is kept on the config rather than in the choice that is about to be replaced.
                if let ModelProviderChoice::OpenRouter { model } = self.config.model_provider() {
                    self.config.last_openrouter_model = Some(model);
                }
                // **Stopping the engine is not tidiness, it is the card.** A `llama-server` we
                // started holds ~9.5 GB; leaving it running after the user switched away means
                // the next thing that wants the GPU -- Ollama loading this very model, or the
                // embedder -- finds it full, and neither of them fails loudly. Ollama evicts its
                // own model; the embedder drops to CPU with a coherent reason.
                self.engine.stop();
                self.config.model_provider = ModelProviderChoice::Ollama;
                Ok(())
            }
            "openrouter" => {
                let key = marlowe_openrouter::ApiKey::from_environment();
                // The catalogue first: it is also how a daemon with no remembered slug gets one.
                match marlowe_openrouter::catalogue::fetch_models() {
                    Ok(models) => self.config.openrouter_models = models,
                    // Not fatal. A thin picker beats a refused switch, and the user asked for
                    // this provider rather than for a list.
                    Err(_) => {}
                }
                let Some(model) = self
                    .config
                    .last_openrouter_model
                    .clone()
                    .or_else(|| self.config.openrouter_models.first().cloned())
                else {
                    return Err(
                        "no OpenRouter model is set and its catalogue could not be read. Restart \
                         with `--openrouter-model <slug>`, or see https://openrouter.ai/models"
                            .to_string(),
                    );
                };
                // **Checked before the switch, not on the next turn.** A missing credential that
                // surfaces as a failed turn reads as a broken model.
                let availability = marlowe_openrouter::Availability::check(&model, &key);
                if !availability.is_ready() {
                    return Err(availability.remedy());
                }
                self.config.model_provider = ModelProviderChoice::OpenRouter { model };
                Ok(())
            }
            // **`marlowe_view::provider::HYBRID`, not a literal.** The picker offers this string,
            // `ModelProviderChoice::name()` returns it, and this arm accepts it — three readers,
            // one constant, so a name a user can see is a name this function takes.
            marlowe_view::provider::HYBRID => {
                // The hosted slug survives a round trip through any other provider, for the
                // reason the `ollama` arm keeps it: switching back should not make the user
                // retype it.
                if let ModelProviderChoice::OpenRouter { model } = self.config.model_provider() {
                    self.config.last_openrouter_model = Some(model);
                }
                let (endpoint, sampling) = self.llamacpp_settings();

                // **The compiled defaults are not the check; the configured ports are.**
                // `LLAMACPP_DEFAULT_PORT` used to BE `DEFAULT_DAEMON_PORT`, and every unit test
                // passed throughout because no single process knows both numbers -- it took a live
                // `--status`, which reported "something is listening but it is not llama-server"
                // about Marlowe's own daemon. Moving one constant fixes today; this fixes the
                // class, because `--daemon-port` and `--llamacpp-port` can both be set by hand.
                if endpoint.port() == self.config.port {
                    return Err(format!(
                        "port {} is this daemon's own control port, so a llama-server cannot be \
                         there. Start marlowe with `--llamacpp-port <N>`",
                        endpoint.port()
                    ));
                }

                // **The sampler is resolved BEFORE the engine starts, and a failure refuses the
                // switch.** Ollama applies the model's `.params` layer on every request it serves
                // and `llama-server` pointed at the raw blob does not, so a switch that could not
                // read that layer would run the same weights at a different temperature with
                // nothing reporting the change. `--llamacpp-sampling server` is how a user says
                // they meant llama.cpp's own defaults; it is never reached by omission.
                //
                // **This is a REFUSAL, not a fallback**, and the asymmetry is deliberate. The
                // fallback exists for things that break under us — a moved store layout, a full
                // card. A sampler we cannot read is a thing we do not know, and continuing on a
                // silently different temperature is not "still working".
                let plan =
                    marlowe_provider::llamacpp::resolve_sampling(&self.config.model, sampling)
                        .map_err(|e| e.remedy())?;

                // ── The engine ────────────────────────────────────────────────────────────
                //
                // **Re-issuing this command CLEARS the latch**, and that is the whole retry
                // story. `HybridEngine::FellBack` never retries on its own — see its header — so
                // the user asking again is the one event that starts a new attempt. Dropping the
                // old engine first stops any server we own, because the commonest reason a start
                // fails on this card is that a `llama-server` is already holding it.
                self.engine.stop();

                // **The switch is ACCEPTED either way, and that is the decision.** *"llama fails
                // fall back to ollama but surface to user why."* Refusing here would be the old
                // third-provider behaviour: the user picks the hybrid, is told no, and gets
                // nothing. Instead they get Ollama, working, with the reason on screen for the
                // session.
                self.config.model_provider =
                    ModelProviderChoice::LlamaCpp { endpoint, sampling };

                // **The engine is NOT started here, and that is the fix for the freeze.**
                // Spawning it, waiting for health and measuring the offload takes seconds, and
                // this function runs on the thread that answers the control port -- so doing it
                // inline froze the surface from the instant the slash command was sent. The
                // switch is acknowledged now; `start_pending_engine` runs after the caller has
                // emitted a `Status`.
                self.pending_engine = Some(plan.disclosure());
                Ok(())
            }
            // Unreachable while `PROVIDERS` and this match agree, and a wrong answer here is a
            // silent no-op, so it is stated rather than left to a catch-all.
            other => Err(format!("`{other}` is listed as a provider and has no implementation")),
        }
    }

    /// One turn, start to finish. The daemon owns the run for its whole life.
    /// One turn, collecting every event. The in-process path (`marlowe --ask` with no daemon)
    /// and the tests use this; `serve_one` uses [`Self::ask_streaming`].
    pub fn ask(&mut self, session: &str, message: &str) -> Vec<Event> {
        let mut out = Vec::new();
        self.ask_streaming(session, message, |e| out.push(e));
        out
    }

    /// One turn, emitting each event **as it is produced**.
    pub fn ask_streaming(
        &mut self,
        session: &str,
        message: &str,
        mut on_event: impl FnMut(Event),
    ) {
        self.ask_streaming_with(session, message, &mut DenyUnattended, on_event)
    }

    /// As [`Self::ask_streaming`], with the approval gate supplied by the caller.
    ///
    /// **`serve_one` passes a gate that round-trips on the live connection.** The daemon is
    /// serial: it serves one request at a time, so a `Request::Approve` arriving on a *second*
    /// connection could not be read while the turn holding the first is still running. The
    /// approval therefore travels on the connection that is already open — the daemon writes
    /// `Event::Approval` and blocks reading one line back.
    pub fn ask_streaming_with(
        &mut self,
        session: &str,
        message: &str,
        approvals: &mut dyn ApprovalGate,
        on_event: impl FnMut(Event),
    ) {
        self.turn(session, message, None, DriverSource::Configured, approvals, on_event)
    }

    /// As [`Self::ask_streaming_with`], with the **model driver** supplied by the caller.
    ///
    /// **This is the seam, and it exists so that a real daemon turn can be measured.** See
    /// [`DriverSource`] for what it is not: it is one parameter on the one `turn`, not a second
    /// assembly path. Provider selection does not run — that is the point, because it `return`s
    /// early on a machine with no model, which is what put `turn`'s injected-memory push beyond
    /// the reach of every test in the workspace.
    ///
    /// Everything else is the shipped turn: the profile is `interactive_with` the MCP fleet's
    /// tools, the tool host is `build_tool_host`, retrieval runs against the real `DaemonMemory`,
    /// the push carries `retrieved.floor`, and the adjudicator is the real one. A caller who wants
    /// the product's own behaviour calls [`Self::ask_streaming_with`] and gets exactly what it
    /// always did.
    pub fn ask_streaming_with_driver(
        &mut self,
        session: &str,
        message: &str,
        driver: &mut dyn marlowe_loop::ModelDriver,
        approvals: &mut dyn ApprovalGate,
        on_event: impl FnMut(Event),
    ) {
        self.turn(session, message, None, DriverSource::Supplied(driver), approvals, on_event)
    }

    /// Continue a run from its last durable checkpoint. **The other end of `RunControl::resume`.**
    ///
    /// It is the same turn path, entered with a run instead of a message. That matters more than
    /// it reads: a separate resume path would be a second place where the profile, the governance
    /// tier, the tool host and the trust floor are assembled, and the two would drift. The one
    /// that drifted would be the one nobody runs interactively.
    pub fn resume_streaming(
        &mut self,
        run: RunId,
        approvals: &mut dyn ApprovalGate,
        mut on_event: impl FnMut(Event),
    ) {
        let staged = {
            let mut plane = self.plane.lock().expect("the control plane lock was poisoned");
            match plane.control.resume(run) {
                Ok(()) => plane.control.take_resumed(),
                Err(e) => {
                    // **Refused by name.** Every variant of `ResumeError` says something different
                    // and actionable: no checkpoint, a version this build will not read, a run the
                    // orphan policy cancelled, a run that already finished.
                    on_event(Event::Error { detail: e.to_string() });
                    return;
                }
            }
        };
        let Some(cp) = staged else {
            on_event(Event::Error {
                detail: format!("run {run} staged no checkpoint despite resuming cleanly"),
            });
            return;
        };
        // The session name is the client's key; the checkpoint carries the id it derives from.
        // There is no reverse map, so the resumed run keeps its own session and the store is
        // keyed by the id -- which is what the loop reads anyway.
        let session = cp.session.to_string();
        self.turn(&session, "", Some(cp), DriverSource::Configured, approvals, on_event)
    }

    /// The one turn. **Every public door arrives here**, and the only thing that varies between
    /// them is the three parameters: a message or a checkpoint, and where the driver comes from.
    fn turn(
        &mut self,
        session: &str,
        message: &str,
        resumed: Option<marlowe_loop::Checkpoint>,
        driver_source: DriverSource<'_>,
        approvals: &mut dyn ApprovalGate,
        mut on_event: impl FnMut(Event),
    ) {
        // The run is recorded FIRST, before anything can fail. The daemon accepted the work, so
        // the record is the daemon's from that moment — a turn that degraded is still a turn
        // that happened, and a client asking `runs` after one must not be told nothing occurred.
        // Recording it only on success made a degraded turn indistinguishable from no turn.
        //
        // **A resumed run keeps its id**, or `/runs` would show the work restarting as something
        // new and the checkpoint chain would fork.
        let run_id = resumed.as_ref().map_or_else(RunId::new, |c| c.run);
        self.plane
            .lock()
            .expect("the control plane lock was poisoned")
            .runs
            .insert(run_id.to_string(), RunSummary::accepted(run_id.to_string()));
        // **A closure over the shared plane, taking the lock per call.** It used to take
        // `&mut self.runs`; the table now lives behind the plane's lock so a `/runs` on the
        // control port sees the same row this turn is updating. The lock is held for one field
        // assignment and never across a model call.
        let plane = std::sync::Arc::clone(&self.plane);
        let mark = |plane: &crate::control_plane::Shared, status: &str, tokens: u64| {
            if let Some(s) = plane
                .lock()
                .expect("the control plane lock was poisoned")
                .runs
                .get_mut(&run_id.to_string())
            {
                s.status = status.to_string();
                s.tokens = tokens;
            }
        };

        // ── which provider, and is it ready ────────────────────────────────────────────
        //
        // **Both arms degrade with a remedy and neither crashes** — invariant 4. The remedies are
        // different because the failures are: `ollama serve` fixes one and an API key fixes the
        // other, and a message a user cannot act on is a crash with better manners.
        //
        // **Neither arm falls back to the other.** A hosted run that quietly became a local one
        // would report a frontier model's name over a 9B's answers, which for a benchmark is
        // worse than not running at all.
        //
        // ── AND THE WHOLE BLOCK IS SKIPPED WHEN THE CALLER SUPPLIED A DRIVER ──────────────
        //
        // **Skipped, not overridden after the fact, and the difference is the entire seam.** Each
        // arm below `return`s on an unready provider — no `ollama serve`, no API key, no
        // `llama-server` — so substituting a driver *after* selection would leave a scripted turn
        // dependent on a live model being installed, which is exactly the condition that put
        // `turn`'s injected-memory push out of reach of every test in the workspace. Entering here
        // is what makes the seam mean anything.
        //
        // The `Configured` arm is byte-for-byte what it was; `tests/provider_switching.rs` and
        // `tests/llamacpp_is_opt_in.rs` assert its selection and its degradation events, and both
        // are untouched by this change.
        let selected: TurnDriver<'_> = match driver_source {
            DriverSource::Supplied(d) => TurnDriver::Supplied(d),
            DriverSource::Configured => TurnDriver::Provider(match self.config.model_provider() {
            ModelProviderChoice::Ollama => {
                let endpoint = LocalEndpoint::default_ollama();
                let routing = match Routing::uniform(&self.config.model) {
                    Ok(r) => r,
                    Err(e) => {
                        mark(&plane, "failed", 0);
                        on_event(Event::Error { detail: e.to_string() });
                        return;
                    }
                };
                let availability = Availability::probe(&endpoint, &routing);
                if !availability.is_ready() {
                    mark(&plane, "degraded", 0);
                    // Invariant 4: a declared, actionable state — not a crash and not a silent stub.
                    on_event(Event::Degraded {
                        what: "no model available".into(),
                        remedy: availability.remedy(),
                    });
                    on_event(Event::Done {
                        outcome: "degraded".into(),
                        detail: availability.remedy(),
                        spend_micros_usd: 0,
                        elapsed_ms: 0,
                    });
                    return;
                }
                Selected::Ollama(endpoint, routing)
            }
            ModelProviderChoice::OpenRouter { model } => {
                // **No network here.** A startup probe would put a round trip in front of every
                // turn and would answer a question the first call answers anyway; what it CAN
                // answer offline — no key, no model named — it answers offline.
                let availability = marlowe_openrouter::Availability::check(
                    &model,
                    &marlowe_openrouter::ApiKey::from_environment(),
                );
                if !availability.is_ready() {
                    mark(&plane, "degraded", 0);
                    on_event(Event::Degraded {
                        what: "no model available".into(),
                        remedy: availability.remedy(),
                    });
                    on_event(Event::Done {
                        outcome: "degraded".into(),
                        detail: availability.remedy(),
                        spend_micros_usd: 0,
                        elapsed_ms: 0,
                    });
                    return;
                }
                Selected::OpenRouter(model)
            }
            ModelProviderChoice::LlamaCpp { endpoint, sampling } => {
                // **The mid-session case, and the only one that cannot be caught at startup.**
                // One non-blocking `try_wait` per turn. A server that died since the last turn
                // latches its reason here, so the very next thing the user sees names the exit
                // code and the server's own last log line rather than a connection failure.
                // **A pending engine is started here too, so a caller that forgot cannot leave
                // the hybrid serving nothing.** `set_provider` acknowledges the switch and defers
                // the start; the control-plane handler runs it immediately afterwards. This is the
                // backstop for every other path -- a default that makes a mismatch unobservable is
                // the shape CLAUDE.md has four bugs from, and "the engine never started because
                // nobody called the second function" is exactly that shape.
                self.start_pending_engine();
                if let Some(line) = self.engine.refresh() {
                    on_event(Event::Degraded {
                        what: "the engine changed".into(),
                        remedy: line,
                    });
                }
                match self.engine.fallback_line() {
                    // ── FALLEN BACK: Ollama serves this turn. ─────────────────────────
                    //
                    // **The turn runs.** That is the decision — *"llama fails fall back to ollama
                    // but surface to user why"* — and it is the opposite of what this arm used to
                    // do, which was emit `Degraded` and `Done{degraded}` and answer nothing at
                    // all. The user asked a question; they get an answer.
                    //
                    // The `Degraded` event is emitted on **every** such turn rather than once.
                    // §B5's band reads `StatusReport::degraded`, which holds it permanently, but a
                    // transcript scrolled back through weeks later has only the events, and a turn
                    // that does not carry the reason is a turn whose engine is unrecoverable from
                    // the record.
                    Some(line) => {
                        on_event(Event::Degraded {
                            what: "llama.cpp is not serving".into(),
                            remedy: line.to_string(),
                        });
                        let ollama = LocalEndpoint::default_ollama();
                        let routing = match Routing::uniform(&self.config.model) {
                            Ok(r) => r,
                            Err(e) => {
                                mark(&plane, "degraded", 0);
                                on_event(Event::Degraded {
                                    what: "no model available".into(),
                                    remedy: e.to_string(),
                                });
                                on_event(Event::Done {
                                    outcome: "degraded".into(),
                                    detail: e.to_string(),
                                    spend_micros_usd: 0,
                                    elapsed_ms: 0,
                                });
                                return;
                            }
                        };
                        let availability = Availability::probe(&ollama, &routing);
                        if !availability.is_ready() {
                            // Both halves are down. The remedy names both, because a user reading
                            // only the second would think Ollama was the thing they chose.
                            let detail = format!(
                                "{}\n\nAnd the engine had already fallen back: {line}",
                                availability.remedy()
                            );
                            mark(&plane, "degraded", 0);
                            on_event(Event::Degraded {
                                what: "no model available".into(),
                                remedy: detail.clone(),
                            });
                            on_event(Event::Done {
                                outcome: "degraded".into(),
                                detail,
                                spend_micros_usd: 0,
                                elapsed_ms: 0,
                            });
                            return;
                        }
                        Selected::Ollama(ollama, routing)
                    }
                    // ── SERVING: llama.cpp answers. ───────────────────────────────────
                    //
                    // The probe carries the offload reading taken at start rather than measuring
                    // again: a generation on every turn's first millisecond would be a model call
                    // in front of every model call.
                    None => {
                        let availability = marlowe_provider::llamacpp::Availability::probe(
                            &endpoint,
                            &self.config.model,
                            self.config.context_tokens,
                            marlowe_provider::OffloadPolicy::Carried(
                                self.engine
                                    .offload()
                                    .unwrap_or(marlowe_provider::Offload::Unknown),
                            ),
                        );
                        if !availability.is_ready() {
                            // Healthy a moment ago and not now, and the child has not exited —
                            // the server is wedged rather than dead. Latch it and let the NEXT
                            // turn take the Ollama path above; answering this one on a server
                            // that just failed its own health check would be guessing.
                            let failure = marlowe_provider::EngineFailure::PortUnavailable {
                                port: endpoint.port(),
                                detail: availability.remedy(),
                            };
                            self.engine = crate::engine::HybridEngine::fell_back(&failure);
                            let line = failure.fallback_line();
                            mark(&plane, "degraded", 0);
                            on_event(Event::Degraded {
                                what: "llama.cpp is not serving".into(),
                                remedy: line.clone(),
                            });
                            on_event(Event::Done {
                                outcome: "degraded".into(),
                                detail: line,
                                spend_micros_usd: 0,
                                elapsed_ms: 0,
                            });
                            return;
                        }
                        Selected::LlamaCpp(endpoint, self.config.model.clone(), sampling)
                    }
                }
            }
            }),
        };

        let registry = match tool_registry(&self.mcp) {
            Ok(r) => r,
            Err(e) => {
                mark(&plane, "failed", 0);
                on_event(Event::Error { detail: e.to_string() });
                return;
            }
        };
        let scope = match WorkspaceScope::new() {
            Ok(s) => s,
            Err(e) => {
                mark(&plane, "failed", 0);
                on_event(Event::Error { detail: e.to_string() });
                return;
            }
        };
        // **Derived, not restated.** The assembler's window is the same number the driver sends
        // as `num_ctx`. Before C2e these were 32_000 and (unset -> 2048): the assembler packed
        // 32k of context into a 2k window and §6's trigger fired against a window that did not
        // exist. `tests/context_window.rs` fails if they ever diverge again.
        let window = self.config.context_tokens;
        let reserve = (window / 16).max(512);
        let mut engine = Engine::new(
            registry,
            scope,
            window,
            reserve,
            self.config.workspace.clone(),
            Tier::Act,
        );

        // ── ONE PROVIDER PER TURN, chosen by the ONE function that decides ────────────
        //
        // `self.config.model_provider()` is read here and by `status()`, and nowhere else. That
        // is what makes "OpenRouter is opt-in" a property rather than a comment: there is no
        // second reading of the same fact for an environment variable to sneak into.
        let attribution_cell = std::sync::Arc::new(std::sync::Mutex::new(
            marlowe_openrouter::RunAttribution::default(),
        ));
        // **The owner, so the built driver outlives the borrow handed to `Ports`.** `Option::insert`
        // returns a `&mut` into the slot it just filled, which is what lets both arms below produce
        // the same `&mut dyn ModelDriver` with no `unwrap` and no second `Box` on the supplied path.
        // It is dropped explicitly further down — see the `drop` beside the attribution read, which
        // is why this is a named local and not a temporary.
        let mut configured_driver: Option<Box<dyn marlowe_loop::ModelDriver>> = None;
        let driver: &mut dyn marlowe_loop::ModelDriver = match selected {
            // **The seam's whole width.** From here to the end of the turn the two cases are the
            // same code: the profile, the tool host, retrieval, the injected-memory push and the
            // adjudicator are assembled once, below, and neither variant can reach a copy of them.
            TurnDriver::Supplied(d) => d,
            TurnDriver::Provider(selected) => configured_driver.insert(match selected {
            Selected::Ollama(endpoint, routing) => {
            let mut driver = OllamaDriver::new(
                endpoint,
                routing,
                tool_registry(&self.mcp).expect("the registry loaded a moment ago"),
            )
            .with_capability(capability_for(&self.config.model))
            .with_context_tokens(self.config.context_tokens)
            .with_thinking(self.config.thinking);

            // **`--dev`: the provider's own wire, before interpretation.**
            //
            // A slow turn and a hung one look identical from outside, and this is the only view that
            // separates *the model is emitting slowly* from *nothing is arriving*. It writes to the
            // daemon's stderr rather than the transcript: it is a diagnostic, and §B1's rule that
            // instrumentation lives under `--dev` applies to the provider seam as much as to memory.
            if self.config.dev {
                // The outbound request, once per model call. This is the reading that settles whether
                // the persona reached the model — the constructed-body test cannot.
                driver = driver.with_request_dump(Box::new(|body| {
                    let system: Vec<&str> = body
                        .get("messages")
                        .and_then(|m| m.as_array())
                        .map(|ms| {
                            ms.iter()
                                .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("system"))
                                .filter_map(|m| m.get("content").and_then(|c| c.as_str()))
                                .collect()
                        })
                        .unwrap_or_default();
                    eprintln!("[dev] ===== OUTBOUND REQUEST =====");
                    eprintln!(
                        "[dev] model={} num_ctx={} num_predict={}",
                        body.get("model").and_then(|m| m.as_str()).unwrap_or("?"),
                        body.pointer("/options/num_ctx")
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "UNSET (Ollama defaults to 2048)".into()),
                        body.pointer("/options/num_predict")
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "unset".into()),
                    );
                    eprintln!("[dev] system messages: {}", system.len());
                    for (i, sys) in system.iter().enumerate() {
                        eprintln!("[dev] --- system[{i}] ({} chars) ---", sys.len());
                        for line in sys.lines() {
                            eprintln!("[dev] | {line}");
                        }
                    }

                    // **Every message, with its role.** The dump printed only the system tier, so the
                    // shape of the conversation — the thing that decides whether the model can see its
                    // own previous tool calls — was the one part of the request `--dev` could not
                    // show. An instrument that omits the interesting half is how a wrong answer looks
                    // authoritative.
                    if let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) {
                        eprintln!("[dev] --- conversation ({} messages) ---", msgs.len());
                        for (i, m) in msgs.iter().enumerate() {
                            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("?");
                            let content =
                                m.get("content").and_then(|c| c.as_str()).unwrap_or("");
                            let calls = m
                                .get("tool_calls")
                                .and_then(|t| t.as_array())
                                .map(|a| a.len())
                                .unwrap_or(0);
                            let name = m.get("tool_name").and_then(|n| n.as_str()).unwrap_or("");
                            eprintln!(
                                "[dev] [{i:>3}] {role:<9} {:>5} chars  tool_calls={calls}  tool_name={name:?}  {:?}",
                                content.len(),
                                content.chars().take(70).collect::<String>()
                            );
                        }
                    }
                    eprintln!(
                        "[dev] tools offered: {}",
                        body.get("tools")
                            .and_then(|t| t.as_array())
                            .map(|a| a
                                .iter()
                                .filter_map(|t| t.pointer("/function/name").and_then(|n| n.as_str()))
                                .collect::<Vec<_>>()
                                .join(", "))
                            .unwrap_or_else(|| "NONE".into())
                    );
                    // **The literal bytes.** Every summary above is a rendering of this; when the two
                    // disagree the summary is wrong, and only this settles it.
                    if std::env::var("MARLOWE_DUMP_BODY").is_ok() {
                        eprintln!("[dev] ===== RAW BODY =====");
                        eprintln!("{}", serde_json::to_string_pretty(body).unwrap_or_default());
                    }
                    eprintln!("[dev] ===== END REQUEST =====");
                }));

                let mut n: u64 = 0;
                driver = driver.with_raw_frames(Box::new(move |frame| {
                    n += 1;
                    let think = frame
                        .get("message")
                        .and_then(|m| m.get("thinking"))
                        .and_then(|c| c.as_str())
                        .unwrap_or("");
                    let text = frame
                        .get("message")
                        .and_then(|m| m.get("content"))
                        .and_then(|c| c.as_str())
                        .unwrap_or("");
                    if !think.is_empty() {
                        eprintln!("[dev] frame {n:>4}  THINK {:>4} bytes", think.len());
                    }
                    let done = frame.get("done").and_then(|d| d.as_bool()).unwrap_or(false);
                    eprintln!(
                        "[dev] frame {n:>4}  {:>5} bytes  done={done}  {:?}",
                        text.len(),
                        text.chars().take(60).collect::<String>()
                    );
                    // ── OLLAMA'S OWN DECOMPOSITION OF THE REQUEST IT JUST SERVED ─────────────
                    //
                    // **These fields arrive on every final frame and NOTHING in this workspace
                    // read them.** `usage` takes `prompt_eval_count`, `eval_count` and
                    // `total_duration` and drops the other three on the floor -- so the one
                    // question a TTFT investigation has to answer, *how much of the wait is the
                    // scheduler and how much is prompt evaluation*, had no instrument at all and
                    // was argued from the outside three times.
                    //
                    // `load_duration` is request-receipt to `sched.GetRunner` returning. On a
                    // model that never left VRAM it is **pure scheduler overhead** and it is
                    // charged **per request**, which the loop makes one of per iteration.
                    //
                    // **`prompt_ms` is the work reading, NEVER `prompt_eval_count`.** Ollama
                    // reports the count for the WHOLE prompt whether or not it evaluated it; on a
                    // cache hit almost none of it was evaluated and the count is unchanged. A
                    // previous session read the count as work and reached a wrong conclusion. The
                    // duration is the only one of the pair that moves when the cache hits, which
                    // is exactly why both are printed side by side here rather than either alone.
                    if done {
                        let ms = |k: &str| {
                            frame.get(k).and_then(|v| v.as_u64()).map(|ns| ns as f64 / 1e6)
                        };
                        let n_of = |k: &str| frame.get(k).and_then(|v| v.as_u64());
                        eprintln!(
                            "[dev] ollama-timing load_ms={:?} prompt_ms={:?} prompt_n={:?} \
                             eval_ms={:?} eval_n={:?} total_ms={:?}",
                            ms("load_duration").map(|v| (v * 10.0).round() / 10.0),
                            ms("prompt_eval_duration").map(|v| (v * 10.0).round() / 10.0),
                            n_of("prompt_eval_count"),
                            ms("eval_duration").map(|v| (v * 10.0).round() / 10.0),
                            n_of("eval_count"),
                            ms("total_duration").map(|v| (v * 10.0).round() / 10.0),
                        );
                    }
                }));
            }
                Box::new(driver)
            }
            Selected::OpenRouter(model) => {
                // The key was already proven present by the availability check above; this is the
                // read that consumes it. A second failure here would mean the environment changed
                // between two statements, which is worth reporting rather than unwrapping.
                let key = match marlowe_openrouter::ApiKey::from_environment() {
                    Ok(k) => k,
                    Err(e) => {
                        mark(&plane, "failed", 0);
                        on_event(Event::Error { detail: e.to_string() });
                        return;
                    }
                };
                let sink_cell = std::sync::Arc::clone(&attribution_cell);
                let dev = self.config.dev;
                let mut driver = marlowe_openrouter::OpenRouterDriver::new(
                    Box::new(marlowe_openrouter::TlsTransport::new()),
                    key,
                    &model,
                    tool_registry(&self.mcp).expect("the registry loaded a moment ago"),
                )
                .with_context_tokens(self.config.context_tokens)
                // **The attribution sink is wired unconditionally, NOT behind `--dev`.**
                //
                // Which upstream served a call is not a diagnostic; it is the difference between
                // a benchmark row somebody can re-run and one nobody can. A run record that
                // carried it only when a debugging flag happened to be on would be exactly the
                // shape ADR-046 §3 exists to prevent.
                .with_attribution_sink(Box::new(move |call| {
                    if dev {
                        eprintln!("[dev] openrouter: {}", call.disclosure());
                    }
                    sink_cell.lock().expect("attribution").push(call.clone());
                }));

                if self.config.dev {
                    driver = driver.with_request_dump(Box::new(|body| {
                        // **The bytes the RUNNING process sent.** Same instrument as the Ollama
                        // adapter's, and it exists for the same reason: a test on a constructed
                        // body cannot see a stale deployment. The API key is NOT here and cannot
                        // be — it travels in a header, and `tests/key_containment.rs` asserts
                        // that against this very sink.
                        eprintln!("[dev] ===== OUTBOUND REQUEST (openrouter) =====");
                        eprintln!(
                            "[dev] model={} max_tokens={} temperature={}",
                            body.get("model").and_then(|m| m.as_str()).unwrap_or("?"),
                            body.get("max_tokens").map(|v| v.to_string()).unwrap_or_default(),
                            body.get("temperature").map(|v| v.to_string()).unwrap_or_default(),
                        );
                        if let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) {
                            eprintln!("[dev] --- conversation ({} messages) ---", msgs.len());
                            for (i, m) in msgs.iter().enumerate() {
                                let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("?");
                                let content =
                                    m.get("content").and_then(|c| c.as_str()).unwrap_or("");
                                let calls = m
                                    .get("tool_calls")
                                    .and_then(|t| t.as_array())
                                    .map(|a| a.len())
                                    .unwrap_or(0);
                                eprintln!(
                                    "[dev] [{i:>3}] {role:<9} {:>5} chars  tool_calls={calls}  {:?}",
                                    content.len(),
                                    content.chars().take(70).collect::<String>()
                                );
                            }
                        }
                        if std::env::var("MARLOWE_DUMP_BODY").is_ok() {
                            eprintln!("[dev] ===== RAW BODY =====");
                            eprintln!("{}", serde_json::to_string_pretty(body).unwrap_or_default());
                        }
                        eprintln!("[dev] ===== END REQUEST =====");
                    }));
                    let mut n: u64 = 0;
                    driver = driver.with_raw_frames(Box::new(move |frame| {
                        n += 1;
                        let text = frame
                            .pointer("/choices/0/delta/content")
                            .and_then(|c| c.as_str())
                            .unwrap_or("");
                        let think = frame
                            .pointer("/choices/0/delta/reasoning")
                            .and_then(|c| c.as_str())
                            .unwrap_or("");
                        if !think.is_empty() {
                            eprintln!("[dev] frame {n:>4}  THINK {:>4} bytes", think.len());
                        }
                        eprintln!(
                            "[dev] frame {n:>4}  {:>5} bytes  {:?}",
                            text.len(),
                            text.chars().take(60).collect::<String>()
                        );
                    }));
                }
                Box::new(driver)
            }
            Selected::LlamaCpp(endpoint, model, sampling) => {
                // **The load-time refusals live in `build`**, so this call site cannot skip one.
                // A store that will not resolve is a named `ResolveError` with a remedy, not a
                // silent substitution of llama.cpp's own sampler.
                let built = marlowe_provider::LlamaCppDriver::build(
                    endpoint,
                    &model,
                    tool_registry(&self.mcp).expect("the registry loaded a moment ago"),
                    sampling,
                );
                let mut driver = match built {
                    Ok(d) => d
                        .with_context_tokens(self.config.context_tokens)
                        .with_thinking(self.config.thinking),
                    Err(e) => {
                        mark(&plane, "degraded", 0);
                        on_event(Event::Degraded {
                            what: "no model available".into(),
                            remedy: e.remedy(),
                        });
                        on_event(Event::Done {
                            outcome: "degraded".into(),
                            detail: e.remedy(),
                            spend_micros_usd: 0,
                            elapsed_ms: 0,
                        });
                        return;
                    }
                };

                if self.config.dev {
                    // **Its OWN dump, not the Ollama one.** That dump reads `/options/num_ctx` and
                    // prints `UNSET (Ollama defaults to 2048)` when it is missing -- and on a
                    // llamacpp body it is CORRECTLY missing, because the window is a launch flag
                    // here. Reusing it would print a confident falsehood on every single request,
                    // which is worse than printing nothing. Its raw-frame half reads
                    // `frame["message"]["thinking"]`, which is Ollama's NDJSON shape, not SSE.
                    let sampling = driver.sampling().disclosure();
                    driver = driver.with_request_dump(Box::new(move |body| {
                        eprintln!("[dev] ===== OUTBOUND REQUEST (llamacpp) =====");
                        eprintln!(
                            "[dev] model={} max_tokens={} enable_thinking={}",
                            body.get("model").and_then(|m| m.as_str()).unwrap_or("?"),
                            body.get("max_tokens").map(|v| v.to_string()).unwrap_or_default(),
                            body.pointer("/chat_template_kwargs/enable_thinking")
                                .map(|v| v.to_string())
                                .unwrap_or_else(|| "unset".into()),
                        );
                        // The window is NOT in this body by design; saying so is what stops a
                        // reader hunting for it. `Availability::ContextTooSmall` is what checks it.
                        eprintln!(
                            "[dev] context window: a LAUNCH flag (-c), not a request field; \
                             checked against /props at switch and at turn start"
                        );
                        eprintln!("[dev] {sampling}");
                        if let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) {
                            eprintln!("[dev] --- conversation ({} messages) ---", msgs.len());
                            for (i, m) in msgs.iter().enumerate() {
                                let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("?");
                                let content =
                                    m.get("content").and_then(|c| c.as_str()).unwrap_or("");
                                let calls = m
                                    .get("tool_calls")
                                    .and_then(|t| t.as_array())
                                    .map(|a| a.len())
                                    .unwrap_or(0);
                                eprintln!(
                                    "[dev] [{i:>3}] {role:<9} {:>5} chars  tool_calls={calls}  {:?}",
                                    content.len(),
                                    content.chars().take(70).collect::<String>()
                                );
                            }
                        }
                        eprintln!(
                            "[dev] tools offered: {}",
                            body.get("tools")
                                .and_then(|t| t.as_array())
                                .map(|a| a
                                    .iter()
                                    .filter_map(|t| {
                                        t.pointer("/function/name").and_then(|n| n.as_str())
                                    })
                                    .collect::<Vec<_>>()
                                    .join(", "))
                                .unwrap_or_else(|| "NONE".into())
                        );
                        if std::env::var("MARLOWE_DUMP_BODY").is_ok() {
                            eprintln!("[dev] ===== RAW BODY =====");
                            eprintln!("{}", serde_json::to_string_pretty(body).unwrap_or_default());
                        }
                        eprintln!("[dev] ===== END REQUEST =====");
                    }));
                    let mut n: u64 = 0;
                    driver = driver.with_raw_frames(Box::new(move |frame| {
                        n += 1;
                        // `reasoning_content` on the DELTA -- 168/168 of this server's first
                        // deltas. Reading `message.thinking` here would print zero bytes of
                        // reasoning on every frame of a reasoning model.
                        let think = frame
                            .pointer("/choices/0/delta/reasoning_content")
                            .and_then(|c| c.as_str())
                            .unwrap_or("");
                        let text = frame
                            .pointer("/choices/0/delta/content")
                            .and_then(|c| c.as_str())
                            .unwrap_or("");
                        if !think.is_empty() {
                            eprintln!("[dev] frame {n:>4}  THINK {:>4} bytes", think.len());
                        }
                        eprintln!(
                            "[dev] frame {n:>4}  {:>5} bytes  {:?}",
                            text.len(),
                            text.chars().take(60).collect::<String>()
                        );
                    }));
                }
                Box::new(driver)
            }
            })
            .as_mut(),
        };
        let tool_scope = match WorkspaceScope::new() {
            Ok(s) => s,
            Err(e) => {
                mark(&plane, "failed", 0);
                on_event(Event::Error { detail: e.to_string() });
                return;
            }
        };
        // **`recall` gets its executor here.** STATE.md predicted the consequence: `consolidation()`
        // exposes `recall`, so `verify_every_exposed_tool_is_runnable` would refuse the moment
        // memory was wired — *"the guard firing then is the guard working"*. It fires against a
        // host that can now answer, which is the resolution rather than an exemption.
        let _ = tool_scope;
        let mut tools = match build_tool_host(
            &self.config.workspace,
            self.config.context_tokens,
            self.memory.beliefs(),
            std::sync::Arc::clone(&self.skills),
            std::sync::Arc::clone(&self.mcp),
        ) {
            Ok(h) => h,
            Err(e) => {
                mark(&plane, "failed", 0);
                on_event(Event::Error { detail: e.to_string() });
                return;
            }
        };
        let mut summarizer = PassthroughSummarizer;
        // **Every frame goes to both places, and neither is derived from the other.** The
        // conversation's client gets what it always got; the plane gets a copy a window polls for.
        // Deriving one from the other would mean a window showing a *different* run of the same
        // turn, which is the "two definitions" shape applied to a stream.
        //
        // `to_run_frame` returns `None` for everything that is not a run's own output — ADR-055
        // §7: what streams is model prose and the harness's §B6 line, and a raw tool result reaches
        // a window only after `condense_batch`, exactly as it reaches the main pane.
        let plane_for_sink = std::sync::Arc::clone(&self.plane);
        let run_key = run_id.to_string();
        let key_for_sink = run_key.clone();
        let mut downstream = move |e: Event| {
            if let Some(frame) = to_run_frame(&e) {
                if let Ok(mut p) = plane_for_sink.lock() {
                    p.push(&key_for_sink, frame);
                }
            }
            on_event(e);
        };

        // ── §B5's FIGURES, MEASURED WHERE THE TOKENS ACTUALLY ARRIVE ─────────────────────────
        //
        // **No model call is added to get these.** Every token the engine streams already passes
        // through `CallbackSink::emit` on its way to the socket, so the measurement is a counter
        // and two clock reads on a path that was already running. The alternative — a separate
        // probe turn — would report the latency of a request nobody made.
        //
        // **TTFT is the first token of ANY kind**, `thinking` included. `qwen3.5:9b` emitted 2,615
        // of 2,862 frames with an empty `content` field; timing to the first *answer* token would
        // have reported the length of its deliberation as latency. What a person perceives as "it
        // started" is the first token that exists.
        //
        // **The rate is emitted with it and never without it.** `marlowe_view::Cadence` is what
        // enforces that at the render, and the reason is this project's own measurement: a CPU
        // `llama-server` beat Ollama on TTFT — 218 ms against 426 — while being five times worse
        // per turn, because TTFT is prompt eval and prompt eval is what a CPU does acceptably.
        //
        // The clock is the §4.5 fence, the same one `started_ms` and the memory path read.
        let turn_started_ms = marlowe_loop::ClockSource::now_ms(&mut crate::clock::SystemClock);
        let warm = self.turns_on_engine > 0;
        let mut first_token_ms: Option<i64> = None;
        let mut tokens: u64 = 0;
        let mut announced = crate::announce::issued();
        let mut on_event = move |e: Event| {
            // ── COUNTED BEFORE FORWARDING, SO A FIGURE EMITTED ALONGSIDE A TOKEN INCLUDES IT ──
            //
            // **This comment used to read "one delta is one token on both local engines" and that
            // was false.** `STATE.md` had already recorded that nothing measured it; measured on
            // 2026-08-31 against `llama-server /tokenize` on the blob Ollama was serving, the
            // thinking channel runs **3–10% low** because Ollama's own parser merges a
            // whitespace-leading token into the next frame. Deterministic across repetitions, and
            // identical on `/v1/chat/completions`. The numbers are in
            // `OllamaDriver::call_streaming_split`'s header.
            //
            // **What is counted here is deltas, and that is now stated rather than assumed.** The
            // cadence band reports a RATE, and a rate is a ratio of two quantities measured over
            // the same interval: the denominator is this process's clock and the numerator is what
            // this process saw arrive. A count corrected at the end of a call — which is what the
            // thinking line uses, and what `Event::Reasoning::tokens` carries — cannot feed a
            // figure that has to be right *while* the call is running.
            //
            // So the two numbers are deliberately different quantities, and neither is a proxy for
            // the other: this one is throughput as observed, `Entry::Reasoning::tokens` is spend as
            // the engine counted it.
            // **An empty reasoning delta is not an arrival.** It is the end-of-call settlement —
            // tokens the engine counted and never streamed — and counting it here would put a
            // token in the rate's numerator that took no time to arrive. That is the same error
            // `Cadence::tok_per_s` was carrying a fortnight ago in the denominator.
            let is_token = match &e {
                Event::Text { .. } => true,
                Event::Reasoning { delta, .. } => !delta.is_empty(),
                _ => false,
            };
            if is_token {
                tokens += 1;
            }
            let ending = matches!(e, Event::Done { .. });
            downstream(e);

            if is_token && first_token_ms.is_none() {
                first_token_ms =
                    Some(marlowe_loop::ClockSource::now_ms(&mut crate::clock::SystemClock));
            }
            // Every `CADENCE_EVERY` tokens, and once more as the turn closes. Not per token: at
            // 73 tok/s that would be 73 extra frames a second carrying a number that had barely
            // moved, and a meter redrawn faster than a person can read is decoration (§B12).
            let due = (is_token && tokens % CADENCE_EVERY == 0) || ending;
            if due {
                if let Some(first) = first_token_ms {
                    let now =
                        marlowe_loop::ClockSource::now_ms(&mut crate::clock::SystemClock);
                    downstream(Event::Cadence {
                        ttft_ms: first.saturating_sub(turn_started_ms).max(0) as u64,
                        tokens,
                        since_first_ms: now.saturating_sub(first).max(0) as u64,
                        warm,
                    });
                }
            }

            // **The daemon's own announcements, flushed onto the live stream.** Anything said
            // during a turn — an engine that died and fell back, a model switch, the openrouter
            // disclosure — reaches the pane while the turn is still going, rather than waiting for
            // the next `Status`. The relaxed load is the whole cost on the common path where
            // nothing has been said.
            if crate::announce::issued() != announced {
                let (mark, fresh) = crate::announce::since(announced);
                announced = mark;
                for a in fresh {
                    downstream(Event::Announce(a));
                }
            }
        };
        let mut sink = CallbackSink { on_event: &mut on_event };
        // **The shared control plane, not `NoControl`.** This is what makes `/steer` reach a run
        // that is already going: the control listener writes into the same `DurableControl` this
        // reads, and the loop asks it at every iteration boundary.
        let mut control = crate::control_plane::SharedControl(std::sync::Arc::clone(&self.plane));
        let mut clock = SystemClock;

        let session_id =
            resumed.as_ref().map_or_else(|| SessionId::from_name(session), |c| c.session);
        let mut run = Run::root(
            run_id,
            session_id,
            // **The same widened profile the startup guard verified.** Building
            // `interactive()` here instead would expose the eleven builtins while the host
            // executes thirteen -- the model would never see the MCP tools, and nothing would
            // report it.
            CapabilityProfile::interactive_with(
                self.mcp.lock().expect("the mcp fleet lock was poisoned").tool_ids(),
            )
            .expect("the MCP tools fitted the budget at startup"),
            Budget::interactive(),
            OutputContract::answer(),
        );
        // **Resumed, not rebuilt.** Governance is asserted once when the conversation starts;
        // `assert_governance` appends, so re-asserting per turn would grow the stable tier by two
        // blocks a turn until compaction had nothing left to trim.
        let workspace_rule = format!(
            "The workspace is {}. Every path you name is relative to it.",
            self.config.workspace.display()
        );
        let memory = self.sessions.remove(session).unwrap_or_else(|| {
            let mut state = SessionState::new(session_id, identity_block());
            // §6: governance lives in the stable tier and is re-asserted structurally. The
            // workspace scope is a user-visible constraint, so it is stated to the model as one.
            state.assert_governance(GovernanceConstraint::asserted(&workspace_rule));
            // **This told the model to call a tool that does not exist.** `done` was removed from
            // the vocabulary when completion became the *absence* of an action — a small model
            // cannot be trusted to emit a terminator reliably, so the loop ends on prose with no
            // tool call. The prompt was not updated with it, so every turn instructed the model to
            // finish with `done`, and the model tried. Found in `--dev`'s outbound dump on a real
            // run; no test could see it, because nothing asserts that the prompt names only tools
            // that exist.
            state.assert_governance(GovernanceConstraint::asserted(governance_prompt()));
            // ── WHAT IS ACTUALLY IN THE WORKSPACE ────────────────────────────────────────
            //
            // **`SourceKind::ProjectFiles` existed, had 15% of the window budgeted to it, and
            // NOTHING EVER PUT ANYTHING IN IT.** A grep for the variant outside `context.rs`
            // returned nothing at all: the tier was declared, budgeted, trimmable, reported --
            // and permanently empty.
            //
            // The cost was measured live, 2026-08-27. Asked to write `session-handoff.md`, the
            // model tried `scratchpad/session-handoff.md` (refused), then `bash dir` (declined --
            // no approval surface), then `scratchpad/wsC/session-handoff.md` (refused), then the
            // right path. **Three wasted calls and 190 seconds to discover a directory listing.**
            // It had been told the workspace's absolute path and nothing about its contents, so
            // it invented plausible prefixes out of the path string itself.
            //
            // A tool would answer this too, and a tool is the wrong shape: knowing where you are
            // is not an action, it is context, and making the model spend a call and a round trip
            // on it is the same mistake as making it guess. §6's context tier is exactly the
            // place for "things that describe the world rather than speak in it" -- the tier's own
            // words -- so the listing goes there and the model simply knows.
            if let Some(map) = workspace_map(&self.config.workspace) {
                state.push(marlowe_loop::Block::new(
                    marlowe_loop::SourceKind::ProjectFiles,
                    map,
                    // The harness read the filesystem; no model composed this.
                    marlowe_contract::TrustClass::AgentObserved,
                ));
            }
            SessionMemory { state, provenance: Provenance::new() }
        });
        let SessionMemory { mut state, mut provenance } = memory;

        // **A resume replaces the run, the window and the provenance wholesale**, and adds no user
        // message, retrieves no memories and surfaces no skills — there is no new message to do
        // any of that against. Doing it anyway would push a turn's worth of retrieval into a
        // window that was mid-thought, which is not the state the run was in when it stopped.
        //
        // `resumed_steps` and `resumed_retries` are the two loop counters whose purpose is to
        // bound a run that will not stop. Resetting them would make `MAX_STEPS` and audit finding
        // E8's cap fire per RESTART rather than per run.
        let (resumed_steps, resumed_retries) = match resumed {
            Some(cp) => {
                let r = cp.restore();
                run = r.run;
                state = r.state;
                provenance = r.provenance;
                (r.steps, r.contract_retries)
            }
            None => {
                provenance.attribute_user_message(message);
                state.push(marlowe_loop::Block::new(
                    marlowe_loop::SourceKind::History,
                    message,
                    TrustClass::UserAsserted,
                ));
                (0, 0)
            }
        };
        let is_resume = resumed_steps > 0;

        // ── §4.2 retrieval, before the model speaks ────────────────────────────────────
        //
        // **M2 Session D, and the half that makes memory a product feature rather than a
        // measurement.** `select_for_injection` had exactly one caller — the eval adapter — so
        // conformance, `repro` and the poisoning suite all exercised injection while the daemon,
        // the thing a person actually talks to, contained no retrieval at all. A property measured
        // on one path and claimed for another is this project's most-logged mistake, and D2 shipped
        // it before D2b caught it.
        //
        // **§B1 is binding: none of this is visible in the interface.** No banner, no citation, no
        // tool line. The user experiences memory through Marlowe knowing things. The diagnostic
        // goes to the daemon's stderr under `--dev` only.
        //
        // The block carries the memories' own trust floor, never a class chosen here. Injecting an
        // untrusted memory at a higher class is precisely the laundering §3.3 exists to close, and
        // `ContextView::trust_floor` is `min` over all blocks including this one — so a recalled
        // web-derived belief correctly drops the run's floor and blocks composed targets.
        let now_for_memory = clock.now_ms();
        // **PHASE TIMING FOR THE PRE-REQUEST PATH, and it exists because nobody could name where
        // ~198 ms goes.**
        //
        // Measured product-level, warm, Ollama: a ~500 ms time-to-first-token decomposes as
        // **198.5 ms OURS** before a byte is written, 226.5 ms of Ollama's own per-request
        // scheduler cost, 224.3 ms of prompt evaluation. The first term is the only one we control
        // and it had never been broken down — the guesses so far were the cross-encoder (which is
        // **not loaded at all**, so it cannot be spending anything) and skills ranking (which is
        // `lexical::score_texts` over one installed skill, so it cannot either).
        //
        // `clock.now_ms()` is the fence — the same one the turn already reads for memory — so this
        // adds no clock access outside it and `the_only_real_clock_read_is_the_latency_fence`
        // stays satisfied. Behind `--dev`, like every other instrument.
        let retrieved = if is_resume {
            crate::memory::Retrieved::nothing_was_asked()
        } else {
            self.memory.retrieve(session, message, now_for_memory, MEMORY_TOKEN_BUDGET)
        };
        if self.config.dev {
            eprintln!(
                "[dev] phase retrieve {} ms · store {}",
                clock.now_ms().saturating_sub(now_for_memory),
                self.memory.state().headline(),
            );
            eprintln!(
                "[dev] memory: {} · injected {} · margin {:?} · abstained {:?}",
                self.memory.state().headline(),
                retrieved.count,
                retrieved.margin,
                retrieved.abstention.map(|a| a.as_str()),
            );
        }
        if !retrieved.is_empty() {
            state.push(marlowe_loop::Block::new(
                marlowe_loop::SourceKind::InjectedMemory,
                retrieved.text.clone(),
                retrieved.floor,
            ));
        }

        // ── The skills bootstrap, here for the same reason retrieval is ───────────────
        //
        // **`SourceKind::Skills` had zero producers before this line.** ADR-051 shipped
        // progressive disclosure's second half — the body loads on `use` — and left the first half
        // to chance: nothing ever told the model a skills library existed, so discovery could only
        // fire if it guessed. It did not. See `skills::surface`.
        //
        // Deliberately alongside `retrieve` rather than at registration: what is relevant depends
        // on what was just asked, and the ranking is against `message` for that reason.
        //
        // **§B1 applies exactly as it does to memory** — none of this is visible in the interface.
        // The user experiences it as Marlowe knowing he has a skill for this.
        //
        // `UserAsserted` for the module header's reason: installing a skill is the user directing
        // Marlowe to follow it, and the agent cannot install one. This carries descriptions only,
        // which is strictly less than a load already puts in at the same class.
        if let Some(text) = {
            let registry = self.skills.lock().expect("the skill registry lock was poisoned");
            (!is_resume).then(|| crate::skills::surface(&registry, message)).flatten()
        } {
            if self.config.dev {
                eprintln!("[dev] skills: surfaced {} B", text.len());
            }
            // **REPLACE, DO NOT APPEND, AND THIS IS THE PREFIX-CACHE FIX.**
            //
            // `context_blocks` is append-only and this runs EVERY TURN, so the same sentence --
            // `1 skill(s) installed in this profile. Search them with `use`.` -- accumulated one
            // copy per turn. Measured on a real profile: the system message grew **+64 chars every
            // turn**, 17,478 -> 17,962 over six turns.
            //
            // A server reuses its prompt cache only for a **byte-identical prefix**, so a system
            // message that grows by a line is a system message that is never cached: Ollama
            // re-evaluated all ~9,900 tokens on every turn, `prompt_ms` sat at 197-239 ms and never
            // fell, and time-to-first-token stayed near 500 ms with `injected 0` memories and 1-4 ms
            // of harness time. It was never retrieval, the cross-encoder, or the memory budget --
            // each of those was measured and cleared. It was this line, duplicated.
            //
            // Replacing keeps the tier's content identical between turns when the surfaced skills
            // are the same, which is the common case, so the prefix goes byte-stable and the cache
            // holds from the second turn on.
            state.context_blocks.retain(|b| b.source != marlowe_loop::SourceKind::Skills);
            state.push(marlowe_loop::Block::new(
                marlowe_loop::SourceKind::Skills,
                text,
                TrustClass::UserAsserted,
            ));
        }

        // **The per-query half rides the TAIL, for the same reason injected memory does.**
        //
        // `skills::surface` above is now constant for a session — it says how many skills exist and
        // nothing else — so it sits in the cached prefix for free. The matches for THIS message are
        // query-dependent, and a query-dependent block inside the leading system message is a
        // prefix that changes: measured at **±164 chars** between consecutive turns, costing a full
        // re-evaluation of ~9,900 tokens each time it appeared or vanished.
        //
        // Pushed straight onto `volatile` rather than through `push`, because `SourceKind::Skills`
        // maps to `Tier::Context` and `push` would route it back into the system message — which is
        // the thing being fixed.
        if !is_resume {
            let hits = {
                let registry = self.skills.lock().expect("the skill registry lock was poisoned");
                crate::skills::hits_for(&registry, message)
            };
            // **Replace, do not accumulate -- the same mistake one tier down.** The context-tier
            // version of this bug appended a line per turn; moving it to `volatile` without a
            // retain simply moved the accumulation, and the trailing block grew 164 -> 330 -> 496
            // -> 662 chars over four turns. Measured, not assumed.
            state.volatile.retain(|b| b.source != marlowe_loop::SourceKind::Skills);
            if let Some(hits) = hits {
                state.volatile.push(marlowe_loop::Block::new(
                    marlowe_loop::SourceKind::Skills,
                    hits,
                    TrustClass::UserAsserted,
                ));
            }
        }
        if self.config.dev {
            // Everything from the turn's clock read to here: retrieval, skills, and the pushes
            // between them. Subtract the `retrieve` line above and what is left is the rest.
            eprintln!(
                "[dev] phase pre-request {} ms",
                clock.now_ms().saturating_sub(now_for_memory)
            );
        }

        let trace = run.trace_id;
        // **Wrapped so a spawned child reaches `/runs` while it is alive.** ADR-057 / `roster.rs`:
        // `ask_streaming_with` inserts the row for the turn the daemon accepted, and until this
        // session that was the only writer of the run table. `Engine::spawn` creates children and
        // knows nothing about a control plane, so a child existed in the journal and in no
        // listing — invisible for the whole of M2 because no model call could produce a spawn.
        let mut recorder = crate::roster::RosterRecorder::new(
            marlowe_loop::record::SharedJournalRecorder::new(
                std::sync::Arc::clone(&self.journal),
                trace,
            ),
            std::sync::Arc::clone(&self.plane),
        );
        let outcome = {
            let mut ports = Ports {
                escalations: None,
                driver,
                summarizer: &mut summarizer,
                tools: &mut tools,
                memory: Some(&mut self.memory),
                approvals,
                sink: &mut sink,
                control: &mut control,
                clock: &mut clock,
                recorder: &mut recorder,
            };
            engine.continue_from(
                &mut run,
                &mut state,
                &mut provenance,
                &mut ports,
                resumed_steps,
                resumed_retries,
            )
        };
        drop(sink);

        // **The turn's history goes back into the conversation.** `engine.run` has pushed the
        // model's prose and every tool result onto `state`; dropping it here is precisely the bug
        // this store exists for.
        self.sessions.insert(session.to_string(), SessionMemory { state, provenance });

        let (status, detail) = match &outcome {
            // **Empty, and that is the fix for a double-send.**
            //
            // M2 C2e made the reply the result, so `CondensedResult` now holds the SAME text the
            // model already streamed as `TextDelta`s. Rendering it here put it on screen twice:
            // once as the answer, then again as `answer: Hello.` — observed live.
            //
            // The turn's prose reaches the user through the stream. `Done` carries the OUTCOME,
            // not a second copy of the content; a client that wants the structured result asks
            // for the run, which is the only place it belongs.
            LoopOutcome::Completed(_) => ("completed", String::new()),
            LoopOutcome::Paused { reason } => ("paused", format!("{reason:?}")),
            LoopOutcome::Escalated { question } => ("escalated", question.clone()),
            // **Structurally unreachable, and it carries no detail for the same reason.**
            // This match is the CONVERSATIONAL run's outcome, and
            // `marlowe_loop::escalation_route` answers `NotRaisable(Root)` for a
            // `Secretary`-level profile -- so `Engine` never returns this variant here.
            // The arm exists because the match is exhaustive by design and a `_` would
            // let the next variant land silently; the empty detail is M3-DESIGN section
            // 3.2 held at the one place a body could otherwise reach the pane Marlowe is
            // speaking in.
            LoopOutcome::Raised(_) => ("raised", String::new()),
            LoopOutcome::Cancelled => ("cancelled", String::new()),
            LoopOutcome::Failed { error } => ("failed", error.clone()),
        };
        mark(&plane, status, run.spent.tokens);
        // **Spend and elapsed, because §6.2's window renders both and neither had a home.** Read
        // off the run rather than recomputed: `run.spent` is what the budget checks against, so a
        // second arithmetic here would be a second answer to "what has this cost".
        if let Some(s) = plane
            .lock()
            .expect("the control plane lock was poisoned")
            .runs
            .get_mut(&run_id.to_string())
        {
            s.spend_micros_usd = run.spent.micros_usd;
            s.elapsed_ms = run.spent.wall_ms;
            s.depth = run.budget.depth;
        }

        // ── ADR-046 §3: WHICH MODEL ANSWERED, AND WHICH UPSTREAM SERVED IT ──────────────
        //
        // Dropped into the run record, where a benchmark harness reading `runs` finds it. Empty
        // on the local path — Ollama serves the model whose name was asked for, on this machine,
        // so there is nothing a second name could disagree with.
        //
        // **The line is printed as well as recorded**, and not behind `--dev`: a benchmark's
        // stderr is where this is actually read, and a fact that decides whether a number is
        // reproducible does not belong behind a debugging flag.
        // **The Box, not the borrow.** `driver` was moved into `Ports` and dropped with it; what
        // still holds the OpenRouter driver's clone of `attribution_cell` is the box in
        // `configured_driver`, and dropping it here is what makes the `try_unwrap` below take the
        // fast path. `None` on the supplied path, where there is no box and no attribution.
        drop(configured_driver);
        let attribution = std::sync::Arc::try_unwrap(attribution_cell)
            .map(|m| m.into_inner().expect("attribution"))
            .unwrap_or_else(|arc| arc.lock().expect("attribution").clone());
        if !attribution.calls.is_empty() {
            let line = attribution.disclosure();
            crate::announce::info(format!("openrouter · {line}"));
            if let Some(s) = plane
                .lock()
                .expect("the control plane lock was poisoned")
                .runs
                .get_mut(&run_id.to_string())
            {
                s.attribution = Some(line);
            }
        }

        // The loop's own Done was filtered at emission (`to_wire`), so this is the only one.
        on_event(Event::Done {
            outcome: status.into(),
            detail,
            spend_micros_usd: run.spent.micros_usd,
            elapsed_ms: run.spent.wall_ms,
        });
    }

    pub fn runs(&self) -> Vec<Event> {
        // **One definition of a run frame** (`RunSummary::to_frame`), shared with the control
        // port. Two renderings of one row is how the main port and the control port would start
        // reporting the same run differently.
        self.plane.lock().expect("the control plane lock was poisoned").run_frames()
    }

    /// Answer a request, emitting each event **as it is produced**.
    ///
    /// `Ask` streams; everything else is a single frame and has nothing to stream.
    fn handle(
        &mut self,
        request: Request,
        approvals: &mut dyn ApprovalGate,
        mut on_event: impl FnMut(Event),
    ) {
        match request {
            Request::Status => on_event(Event::Status(self.status())),
            Request::Ask { session, message } => {
                self.ask_streaming_with(&session, &message, approvals, on_event)
            }
            // The gate is not wired to a surface yet; refusing is the honest answer rather than
            // recording an approval nobody gave.
            Request::Replay { session } => {
                // **The turn is reconstructed, not summarised.** A first version emitted only the
                // prose and a bare tool verb, so a reopened window lost every thinking block and
                // every tool line's target and result — the conversation came back and the work
                // behind it did not.
                //
                // Order falls out of the block order and does not need buffering: each assistant
                // turn is pushed BEFORE the result it produced, so reasoning precedes its tool
                // line exactly as it did live.
                if let Some(mem) = self.sessions.get(&session) {
                    // The call an upcoming tool result belongs to, so its line can name a target.
                    let mut pending: Option<(String, String)> = None;
                    for block in mem.state.volatile.iter() {
                        let wire = block.wire.as_ref();
                        match block.source {
                            marlowe_loop::SourceKind::History => {
                                if let Some(t) = wire.and_then(|w| w.thinking.as_ref()) {
                                    // **`0`, and the head line renders that as no number at all.**
                                    // A replayed turn has the reasoning text; what it cost was
                                    // never recorded, because `WireTurn` carries what the endpoint
                                    // documents and a token count is not part of that shape.
                                    // Inventing one from the string is the estimate this whole
                                    // change exists to remove, so the field renders as missing —
                                    // the rule `Event::Approval`'s `novelty` already follows.
                                    on_event(Event::Reasoning { delta: t.clone(), tokens: 0 });
                                }
                                if let Some(c) =
                                    wire.and_then(|w| w.tool_calls.first())
                                {
                                    // The first string argument is what §B6 shows as the target,
                                    // which is the same thing `blast_radius` reads.
                                    let target = c
                                        .arguments
                                        .as_object()
                                        .and_then(|o| {
                                            o.values().find_map(|v| v.as_str().map(String::from))
                                        })
                                        .unwrap_or_default();
                                    pending = Some((c.name.clone(), target));
                                }
                                if !block.text.is_empty() {
                                    if block.trust == TrustClass::AgentInferred {
                                        on_event(Event::Text { delta: block.text.clone() });
                                    } else {
                                        on_event(Event::User { text: block.text.clone() });
                                    }
                                }
                            }
                            marlowe_loop::SourceKind::ToolResults => {
                                let (verb, target) = pending.take().unwrap_or_else(|| {
                                    (
                                        wire.and_then(|w| w.tool_name.clone())
                                            .unwrap_or_else(|| "tool".into()),
                                        String::new(),
                                    )
                                });
                                let failed = wire.is_some_and(|w| w.tool_failed);
                                on_event(Event::Tool {
                                    id: 0,
                                    verb,
                                    target,
                                    state: if failed { "failed".into() } else { "ok".into() },
                                    summary: wire
                                        .and_then(|w| w.tool_summary.clone())
                                        .unwrap_or_default(),
                                    // **`None`, and there is nothing to put here.** A replay is
                                    // rebuilt from `WireTurn` (`marlowe-loop/src/context.rs`),
                                    // which carries `tool_summary` and no `tool_detail` — the
                                    // context window keeps the §B6 line, not the bytes. Nor
                                    // should it: the journal is deliberately not a copy of every
                                    // file the agent has opened (see `Engine::finish_call`).
                                    //
                                    // A replayed tool line therefore has no expansion, which is a
                                    // true statement about what was retained rather than a gap to
                                    // fill with the summary.
                                    detail: None,
                                });
                            }
                            _ => {}
                        }
                    }
                }
            }
            Request::SetModel { model } => {
                // Answered with a fresh `Status`, never with a bare acknowledgement. The client
                // then re-projects the daemon's own report — including the new disclosure — rather
                // than patching its view with what it hoped had happened.
                match self.set_model(&model) {
                    Ok(()) => on_event(Event::Status(self.status())),
                    Err(detail) => on_event(Event::Error { detail }),
                }
            }
            Request::SetProvider { provider } => {
                // Answered with a fresh `Status` for the same reason `SetModel` is: the model list
                // is a consequence of this, and the client re-projects the daemon's report rather
                // than guessing what the new list holds.
                match self.set_provider(&provider) {
                    Ok(()) => {
                        // **Two statuses, and the first one is the point.** The switch is
                        // acknowledged before the engine is started, so the surface repaints with
                        // the new provider immediately instead of sitting frozen for the seconds a
                        // spawn, a health wait and an offload measurement take. The second carries
                        // the engine's verdict -- serving, or fallen back with the reason.
                        on_event(Event::Status(self.status()));
                        self.start_pending_engine();
                        on_event(Event::Status(self.status()));
                    }
                    Err(detail) => on_event(Event::Error { detail }),
                }
            }
            // **Delegated to the one implementation**, which the control listener also calls.
            // Answering them here too is not redundancy: a single-terminal user with an idle
            // daemon reaches the main port, and refusing there would be a control that works only
            // when a second terminal is open.
            Request::Runs | Request::Watch { .. } | Request::Steer { .. } | Request::Cancel { .. } => {
                crate::control_plane::answer(&std::sync::Arc::clone(&self.plane), request, &mut on_event)
            }
            // **Needs the engine, so it is the main port's**, and it drives rather than staging.
            // **Resolved the same way every other run-addressed request is.** A `--resume
            // daring-storm` that failed while `--watch daring-storm` worked would be two answers
            // to the question of what a run is called.
            Request::Resume { run } => {
                let resolved = self
                    .plane
                    .lock()
                    .expect("the control plane lock was poisoned")
                    .resolve(&run);
                match resolved {
                    Ok(id) => self.resume_streaming(id, approvals, on_event),
                    Err(detail) => on_event(Event::Error { detail }),
                }
            }
            Request::Shutdown => {
                // **Refused while a run is live.** That is exactly what invariant 6 protects: the
                // work outlives the window. An idle daemon protects nothing and is only in the
                // way.
                let live = self.live_runs();
                if live > 0 {
                    on_event(Event::Error {
                        detail: format!(
                            "{live} run(s) still in flight; the daemon keeps them (invariant 6). \
                             Stop it again once they finish."
                        ),
                    });
                } else {
                    self.shutdown.store(true, Ordering::Relaxed);
                    on_event(Event::Done {
                        outcome: "shutdown".into(),
                        detail: String::new(),
                        spend_micros_usd: 0,
                        elapsed_ms: 0,
                    });
                }
            }
            // **An approval arriving on its OWN connection cannot be honoured, and the reason is
            // the daemon's serialism rather than a missing feature.** The turn that raised the
            // prompt is holding the only connection being served; a second one is not read until
            // that turn finishes, by which time the decision it answers has already been denied.
            // The live path answers on the connection the prompt arrived on — see
            // `SocketApprovals`. This arm stays so the wire shape is total, and it says why.
            Request::Approve { .. } => on_event(Event::Error {
                detail: "an approval must be answered on the connection that asked for it. This daemon serves one connection at a time, so a decision sent on a second connection is read only after the turn it answers has already been denied. Concurrency is M3."
                    .into(),
            }),
        }
    }

    /// Serve until shut down. One connection at a time — a second client is a M3 concern and
    /// pretending to handle it now would be a concurrency story nobody tested.
    pub fn serve(mut self) -> Result<(), DaemonError> {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, self.config.port));
        let listener = TcpListener::bind(addr).map_err(|e| DaemonError::Listen {
            port: self.config.port,
            detail: e.to_string(),
        })?;
        // **The control plane comes up beside the main port**, so `/steer`, `/watch` and `/runs`
        // are answerable while a turn is holding this one. A bind failure degrades visibly and
        // names the remedy (invariant 4) rather than refusing to start the daemon: a secondary
        // port being busy must not cost the user their assistant.
        match crate::control_plane::spawn(
            std::sync::Arc::clone(&self.plane),
            &self.config.profile_root.clone(),
            self.token.clone(),
            Arc::clone(&self.shutdown),
        ) {
            Ok(port) => crate::announce::info(format!("control plane on 127.0.0.1:{port}")),
            Err(detail) => crate::announce::warn(format!("DEGRADED · {detail}")),
        }
        let shutdown = Arc::clone(&self.shutdown);

        let state = Mutex::new(&mut self);

        for incoming in listener.incoming() {
            // LOOP-EXEMPT: an accept loop, not an agent loop. HP10's check is scoped to
            // `marlowe-loop`; this is stated so a reader does not go looking.
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            // **THE CONNECTION IS SHUT DOWN EXPLICITLY, AND THIS IS THE FREEZE.**
            //
            // Windows creates sockets from `accept` as INHERITABLE, and Rust's `Command::spawn` passes
            // `bInheritHandles = TRUE`. So every child started while a connection is open receives a
            // duplicate of that connection's handle. Dropping our own copy is then NOT enough: the peer
            // sees no EOF, because the child is holding the other reference for as long as it lives.
            //
            // `/provider ollama/llama.cpp` starts a `llama-server` that is MEANT to outlive the request.
            // So the socket stayed open forever. Measured against the real control port: both status
            // frames arrived, at 2,050 ms and 6,489 ms, and then **no EOF after 180 seconds**, while a
            // second connection was answered in 27 ms. The daemon was never stuck. Only the caller was,
            // on a handle it could not see and did not own.
            //
            // This is why shortening `HEALTH_DEADLINE` and deferring the engine start did not help:
            // both made the daemon faster at a job it was already completing, and neither can close a
            // handle held by another process.
            //
            // `shutdown` acts on the CONNECTION rather than on a handle's reference count, so it sends
            // FIN and the peer reads EOF no matter how many duplicates exist. It is ordinary safe Rust —
            // this crate denies `unsafe`, and `SetHandleInformation` would have needed an exemption for a
            // weaker guarantee, since it only protects children spawned AFTER the flag is cleared.
            let Ok(stream) = incoming else { continue };
            let hangup = stream.try_clone().ok();
            let mut guard = state.lock().expect("the daemon is single-threaded");
            let _ = guard.serve_one(stream);
            drop(guard);
            // FIN, unconditionally. See the note above the accept.
            if let Some(h) = hangup {
                let _ = h.shutdown(std::net::Shutdown::Both);
            }
            // **Checked after serving, not only before accepting.** `incoming()` blocks, so a
            // shutdown request set the flag and then the loop sat waiting for a connection that
            // would never come — the daemon answered "shutdown" and kept listening. Verified by
            // connecting again afterwards rather than by trusting the reply.
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
        }
        Ok(())
    }

    fn serve_one(&mut self, stream: TcpStream) -> std::io::Result<()> {
        let mut writer = stream.try_clone()?;
        let mut reader = BufReader::new(stream);

        // ---- Authentication, before anything is parsed as a request ----------------------------
        //
        // **The CONNECTION is authenticated, not the request.** `Request` is an internally-tagged
        // enum, so a token field would have to go on every variant and every variant's construction
        // site — and the one variant somebody forgot would be an unauthenticated request that
        // deserialized fine. One preamble line cannot be forgotten per-variant.
        //
        // **Bounded, because the daemon is serial.** `serve` holds one connection at a time, so a
        // peer that connects and says nothing would hang every other client. That is true of the
        // request read as well and always has been; it is closed here because this is the read a
        // port scanner reaches first. The timeout is cleared before the turn, where a two-minute
        // model call is working rather than dead.
        reader
            .get_ref()
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .ok();
        let mut preamble = String::new();
        let offered = match reader.read_line(&mut preamble) {
            Ok(0) => return Ok(()), // Connected and hung up. A probe, not a client.
            Ok(_) => preamble.trim().to_string(),
            // A peer that connects and sends nothing gets nothing. Writing the refusal would tell
            // a scanner a daemon is here; closing tells it only that something accepted.
            Err(_) => return Ok(()),
        };
        if !crate::auth::matches(&self.token, &offered) {
            crate::protocol::write_line(
                &mut writer,
                &Event::Error { detail: crate::auth::refusal() },
            )?;
            // **Drain before closing, or the refusal can be destroyed by the close itself.**
            //
            // The client sends its preamble and its request together. On the refusal path the
            // request line is never read, so it is still sitting in the receive buffer when this
            // function returns and the socket is dropped — and closing a socket with unread inbound
            // data makes Windows send an **RST** rather than a FIN. An RST discards whatever is in
            // flight, including the refusal that was just written, so the client sees an empty
            // stream and cannot tell "refused" from "the daemon hung up".
            //
            // Found by `an_empty_a_truncated_and_a_one_character_wrong_token_are_all_refused`
            // failing with `got []` — intermittently, because it is a race between the client's
            // read and this close. The refusal is the only thing that tells a user their client is
            // pointed at another profile, so losing it turns a diagnosable state into a mystery.
            let mut sink = [0u8; 4096];
            let mut drained = 0usize;
            reader
                .get_ref()
                .set_read_timeout(Some(std::time::Duration::from_millis(200)))
                .ok();
            while drained < 64 * 1024 {
                // LOOP-EXEMPT: draining a socket before close, not a driving loop.
                match reader.read(&mut sink) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => drained += n,
                }
            }
            return Ok(());
        }
        reader.get_ref().set_read_timeout(None).ok();
        // ----------------------------------------------------------------------------------------

        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let request: Request = match serde_json::from_str(line.trim()) {
            Ok(r) => r,
            Err(e) => {
                crate::protocol::write_line(
                    &mut writer,
                    &Event::Error { detail: format!("malformed request: {e}") },
                )?;
                return Ok(());
            }
        };
        // **Write and flush per event.** `write_line` already flushes — §4.0.2's rule that a
        // response sitting in a buffer is indistinguishable from a hang, which is exactly what the
        // whole turn used to be.
        //
        // A write failure means the client hung up. It cannot be propagated out of the callback,
        // so it is captured and returned after the turn: aborting mid-turn would leave the run
        // half-recorded, and the daemon owns the run whether or not anyone is listening
        // (invariant 6).
        // **The gate holds its own clones.** See `SocketApprovals`: the event writer is borrowed
        // by the callback below, and one exchange split across two mechanisms would be worse than
        // a second file descriptor.
        //
        // **The ORIGINAL reader is moved in, not a second one over the same socket.** A fresh
        // `BufReader` would start with an empty buffer while this one may already hold bytes the
        // client sent after the request line — those bytes would be stranded in a reader nobody
        // reads again. It is not reachable today, because the client cannot answer a prompt it has
        // not been sent yet, but a second buffer over one socket is a bug waiting for a client
        // that pipelines.
        let mut approvals = match writer.try_clone() {
            Ok(w) => Some(SocketApprovals::new(w, reader)),
            // A socket that cannot be cloned cannot carry an approval, and the honest gate for
            // that is the one that denies.
            Err(_) => None,
        };
        let mut deny = DenyUnattended;
        let gate: &mut dyn ApprovalGate = match approvals.as_mut() {
            Some(a) => a,
            None => &mut deny,
        };

        let mut write_err: Option<std::io::Error> = None;
        self.handle(request, gate, |event| {
            if write_err.is_some() {
                return;
            }
            if let Err(e) = crate::protocol::write_line(&mut writer, &event) {
                write_err = Some(e);
            }
        });
        match write_err {
            Some(e) => Err(e),
            None => writer.flush(),
        }
    }
}

/// **The persona, from the artifact.** Addendum C §C6: *"Versioned as an artifact, `persona/vN.md`
/// … **not a string in the code** and not a user setting."*
///
/// Until M2 C2d this was a 40-word `const` whose doc comment claimed it "carries the persona
/// (Addendum C)". It carried no part of Addendum C, and nothing tested the claim. `include_str!`
/// makes the artifact the single source and a change to it a reviewable diff.
///
/// **The check that matters is emission, not loading.** See
/// `marlowe-provider/tests/persona_emission.rs`.
///
/// **v2 as of M2 C2f.** It is 8.7× the size of v1 — ~3,544 tokens against ~409 by the assembler's
/// three-chars-per-token estimator, which is **11.5% of the 30,720-token effective window**,
/// permanently, in every request. That is a real cost and it is deliberate; it is recorded here
/// because a stable-tier artifact that grows silently is a compaction trigger that fires earlier
/// than anyone expects, with nothing naming the cause. The stable tier is not trimmable
/// (`SourceKind::trimmable`), so it will never be truncated to fit — it will push everything else
/// out first.
const PERSONA: &str = include_str!("../../../persona/v2.md");

/// The run's identity, which §C6 places in the stable tier *alongside* the persona rather than as
/// part of it. Kept separate so the artifact stays deployment-independent: a persona that named
/// the workspace would not be the same artifact across two runs.
/// **State the capability; do NOT ask the model to optimise against it.**
///
/// The first version of this said *"prefer notation that survives"* and then listed which commands
/// render and which do not. A user watching a live reply reported the model spending a visible
/// share of its thinking checking whether its formulas would survive -- reasoning about the
/// renderer instead of about the question.
///
/// That was a straightforward prompt-design error. The degradation is **safe by construction**:
/// notation the renderer cannot typeset is shown as the source the model wrote, which is legible
/// and honest. There is no penalty to trade against, so asking for a preference invented a
/// cost-benefit calculation that has no cost on either side -- and the token list gave it a
/// checklist to run per formula.
///
/// It now says what happens and explicitly says there is nothing to work around.
const IDENTITY_FACTS: &str =
    "You are running as a terminal-native agent harness. Say what you did and what you did not, \
     and never claim a result you did not produce. \
     The terminal renders your replies as Markdown -- headings, lists, tables, fenced code, \
     emphasis -- and typesets LaTeX written as $inline$ or $$display$$. Write maths the way you \
     normally would: anything the renderer cannot typeset is shown as your own source, which is \
     legible, so there is nothing to avoid and nothing to work around.";

/// What reaches the stable tier: persona first, then the run's facts.
fn identity_block() -> String {
    format!("{PERSONA}\n{IDENTITY_FACTS}")
}

/// A recorder for a daemon with no journal on disk — used by tests, never by `serve`.
pub fn memory_recorder() -> MemoryRecorder {
    MemoryRecorder::default()
}

/// A bounded picture of the workspace, for the context tier.
///
/// # What it includes, and why each bound is there
///
/// Breadth-first from the root so the top level is always complete before anything deeper is
/// spent on: a model that knows the top level can `grep` or `read` its way down, whereas one that
/// got an exhaustive listing of the first directory alphabetically knows almost nothing.
///
/// **`MAP_MAX_ENTRIES` is a cap on the LISTING, not on the workspace.** A truncated map says so in
/// words, because a listing that silently stops is worse than none: the model would conclude a
/// file is absent when it was merely past the cap. `SourceBudgets` gives `ProjectFiles` 15% of the
/// window and the assembler may trim this block; the cap is what stops it being the thing that
/// forces a trim.
///
/// Skips the directories whose contents are never what the model wants and would consume the whole
/// budget: version control, build output, dependency trees.
pub fn workspace_map(root: &std::path::Path) -> Option<String> {
    /// Enough to see a real project's shape; small enough not to dominate the context tier.
    const MAP_MAX_ENTRIES: usize = 200;
    /// Two levels: the root, and one inside each directory. Deeper is `grep`'s job.
    const MAP_MAX_DEPTH: usize = 2;
    // **The list lives in `marlowe_exec`, because the walk `glob` and `grep` use needs the same
    // one.** This private copy was correct and the executors' walk had none at all, which is how
    // `grep(".")` on this checkout filled all 2,000 of its file slots with build artifacts and
    // never reached `crates/`. One definition, two readers — a second copy is how the map and the
    // search come to disagree about what a project is.
    use marlowe_exec::WALK_SKIP as SKIP;

    let mut out = Vec::new();
    let mut queue = std::collections::VecDeque::new();
    queue.push_back((root.to_path_buf(), 0usize));
    let mut truncated = false;

    while let Some((dir, depth)) = queue.pop_front() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        // **Sorted, because this reaches a model and `read_dir` order is a filesystem detail.**
        // Two runs on the same workspace must describe it the same way, or the prompt prefix
        // changes for no reason and the provider's cache is thrown away every turn.
        let mut names: Vec<_> = entries.flatten().collect();
        names.sort_by_key(|e| e.file_name());
        for e in names {
            if out.len() >= MAP_MAX_ENTRIES {
                truncated = true;
                break;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') && name != ".claude" {
                continue;
            }
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let rel = e.path().strip_prefix(root).ok()?.to_string_lossy().replace('\\', "/");
            if is_dir {
                out.push(format!("{rel}/"));
                if depth + 1 < MAP_MAX_DEPTH && !SKIP.contains(&name.as_str()) {
                    queue.push_back((e.path(), depth + 1));
                }
            } else {
                out.push(rel);
            }
        }
        if truncated {
            break;
        }
    }

    if out.is_empty() {
        return None;
    }
    let mut text = format!(
        "<workspace>\nThese paths exist in the workspace, relative to its root. Paths you use in \
         `read`, `write`, `edit` and `grep` are relative to that root -- do NOT prefix them with \
         the root's own name.\n\n{}",
        out.join("\n")
    );
    if truncated {
        text.push_str(&format!(
            "\n\n[listing stopped at {MAP_MAX_ENTRIES} entries and at depth {MAP_MAX_DEPTH}; \
             there are more files than this. Use `glob` to see what is not listed, or `grep` \n             to search inside it.]"
        ));
    }
    text.push_str("\n</workspace>");
    Some(text)
}

/// How the loop's termination rule is stated to the model.
///
/// **Public so it can be asserted on.** It said *"Answer with `done` when the task is complete"*
/// for as long as `done` had not existed, and nothing could see it — the loop was right, the
/// provider was right, and a prompt is just a string until something reads it. It is a function
/// rather than a literal so `the_system_prompt_names_no_tool_that_does_not_exist` has a subject.
pub fn governance_prompt() -> &'static str {
    "Use a tool when the user asks for something a tool can do. You may call tools while reasoning. When the task is complete, reply to the user in prose and call no tool — that is      what ends the turn."
}

#[cfg(test)]
mod approval_gate_tests {
    use super::*;
    use marlowe_permission::BlastRadius;
    use std::io::Write as _;

    /// **The surface renders Markdown and LaTeX, and the model is told so.**
    ///
    /// Without this the model has no way to know: it cannot see the terminal, and a reply written
    /// as flat prose renders identically whether or not the renderer exists. The statement lives in
    /// `IDENTITY_FACTS` rather than in `persona/vN.md` because it is a DEPLOYMENT fact -- the same
    /// persona artifact runs behind a surface that renders and one that does not -- and because the
    /// persona is a section 13 boundary a rendering note has no business editing.
    ///
    /// **This asserts the CONTENT; the CHANNEL is proven elsewhere.**
    /// `marlowe-provider/tests/persona_emission.rs` asserts the stable tier reaches the outbound
    /// request body as a `system` message, and `identity_block` is what goes into it. What is new
    /// here is text on a path already known to carry, not a new claim about carrying.
    #[test]
    fn the_model_is_told_the_surface_renders_markdown_and_latex() {
        let block = identity_block();
        assert!(block.contains("Markdown"), "{block}");
        assert!(block.contains("$inline$") && block.contains("$$display$$"), "{block}");
        // The honesty half matters more than the capability half: a model that believes every
        // expression typesets will write matrices that arrive as raw LaTeX.
        // The graceful-degradation half, and the wording matters as much as the presence.
        //
        // This asserted "raw source" against a sentence that ALSO said "prefer notation that
        // survives" -- and that phrase, plus a list of which commands render, had the model
        // spending a visible share of its thinking checking whether its formulas would survive.
        // Reasoning about the renderer instead of the question, reported from a live reply.
        //
        // The degradation is safe by construction, so there is no trade-off to state. It now says
        // there is nothing to work around, and the assertion is on THAT rather than on the fact
        // that some notation is unrepresentable.
        assert!(
            block.contains("nothing to avoid"),
            "the model must be told it need not optimise against the renderer: {block}"
        );
        // An addition to the stable tier, not a replacement of it.
        assert!(block.len() > PERSONA.len(), "the persona was lost: {}", block.len());
    }

    fn radius() -> BlastRadius {
        BlastRadius {
            verb: "web".into(),
            scope: "https://example.com/".into(),
            reversible: true,
            novelty: None,
        }
    }

    /// A connected pair, so the gate is exercised over a real socket rather than a mock. The
    /// client half is returned for the test to answer on.
    fn pair() -> (SocketApprovals, TcpStream) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
        let addr = listener.local_addr().expect("addr");
        let client = TcpStream::connect(addr).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        let reader = BufReader::new(server.try_clone().expect("clone"));
        (SocketApprovals::new(server, reader), client)
    }

    fn answer(client: &mut TcpStream, line: &str) {
        client.write_all(line.as_bytes()).expect("write");
        client.write_all(b"\n").expect("newline");
        client.flush().expect("flush");
    }

    #[test]
    fn a_granted_decision_is_approved_and_the_prompt_names_the_blast_radius() {
        let (mut gate, mut client) = pair();
        answer(&mut client, r#"{"op":"approve","decision":1,"granted":true}"#);
        assert!(gate.await_approval(&radius()));

        // §B9: the prompt states the blast radius, not the command. Read it back off the wire —
        // asserting on a struct built in the test would prove nothing about what was sent.
        let mut sent = String::new();
        BufReader::new(client).read_line(&mut sent).expect("the prompt was written");
        assert!(sent.contains("example.com"), "the scope must reach the client: {sent}");
        assert!(sent.contains("\"decision\":1"), "the prompt must carry its id: {sent}");
    }

    #[test]
    fn a_declined_decision_is_denied() {
        let (mut gate, mut client) = pair();
        answer(&mut client, r#"{"op":"approve","decision":1,"granted":false}"#);
        assert!(!gate.await_approval(&radius()));
    }

    /// **Every failure is a denial**, and each of these is a separate way to fail. A gate that
    /// approved because it could not hear the answer would make §8.2's "the harness enforces"
    /// a statement about a code path that does not run.
    #[test]
    fn silence_a_malformed_reply_and_a_stale_decision_id_are_all_denials() {
        // Hung up without answering.
        let (mut gate, client) = pair();
        drop(client);
        assert!(!gate.await_approval(&radius()), "a client that hung up has not approved");

        // Answered with something that is not a decision.
        let (mut gate, mut client) = pair();
        answer(&mut client, "not json at all");
        assert!(!gate.await_approval(&radius()), "a malformed reply is not an approval");

        // Answered a DIFFERENT decision — the stale-prompt case. Without the id check this
        // would approve whatever happens to be pending now.
        let (mut gate, mut client) = pair();
        answer(&mut client, r#"{"op":"approve","decision":99,"granted":true}"#);
        assert!(
            !gate.await_approval(&radius()),
            "a reply naming another decision must not approve this one"
        );
    }

    /// Ids increment, so two prompts in one turn cannot be confused for each other.
    #[test]
    fn each_prompt_in_a_turn_gets_its_own_decision_id() {
        let (mut gate, mut client) = pair();
        answer(&mut client, r#"{"op":"approve","decision":1,"granted":true}"#);
        assert!(gate.await_approval(&radius()));
        // The second prompt is decision 2; answering it with 1 again must fail.
        answer(&mut client, r#"{"op":"approve","decision":1,"granted":true}"#);
        assert!(
            !gate.await_approval(&radius()),
            "re-sending the previous decision must not approve the next one"
        );
    }
}
