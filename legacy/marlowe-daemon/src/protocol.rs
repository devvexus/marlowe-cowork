//! What crosses the client/daemon boundary. ARCHITECTURE §6.
//!
//! # The boundary is the point, not the transport
//!
//! §6 splits the binary into a thin client and a daemon **because of invariant 6**: if the client
//! owned the run, closing the terminal would end it. So the shape of this protocol is constrained
//! by one rule — **the client may hold nothing the daemon does not have.** A request names a
//! session and a message; a response is a stream of render-only events. There is no request that
//! hands the client a `Run`, a `Checkpoint`, or a `ContextView`, and there must not be one.
//!
//! That is also why the client can paint before this connection resolves (§B13's 150 ms first
//! frame): there is nothing in the first frame that comes from here.
//!
//! # NDJSON over loopback
//!
//! Same framing as the §4.0 eval transport, for the same reason it gives: the wire log is
//! readable with ordinary tools. One JSON value per line, `\n`-terminated, flushed per frame.

use serde::{Deserialize, Serialize};

/// Client → daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// What is the daemon, and is it healthy? Answered without touching a model.
    Status,
    /// Start a turn. The daemon owns the run; the client gets events.
    Ask { session: String, message: String },
    /// List runs the daemon owns. **Read-only** — the client cannot mutate a run through it.
    ///
    /// Answered on **either** port. The control port answers it while a turn is in flight, which
    /// is the case `/runs` is actually for; the main port keeps answering it so nothing that
    /// already asked there has to move.
    Runs,
    /// One run, in the detail §6.2 asks a window to render. **Control port.**
    ///
    /// `/watch` opens a window rather than streaming into the conversation pane (§6.6) — filling
    /// the main pane with agent output halts the conversation *visually*, which is what this
    /// milestone exists to stop. The window is Session F's; this is the state it renders, and
    /// there is only one of it.
    /// `since` is the highest output-frame sequence the client already holds, so a window polls
    /// incrementally instead of re-reading a whole run every 120 ms. **`0` asks for everything**,
    /// which is what a window that has just opened wants and what every non-window caller sends.
    ///
    /// **Added by Session F, on A's variant rather than beside it.** A second `WatchOutput` request
    /// would be two questions about one run answered from one lock, and the answers could disagree
    /// about which frames belong to the detail they arrived with.
    /// **`#[serde(default)]`, so a client built before this field still parses the frame** — the
    /// same courtesy `attribution` gets above, and the reason is the same: this field was added to
    /// an existing request, and a wire that refused the old shape would break every caller that had
    /// no reason to change. `0` asks for everything, which is what those callers mean.
    Watch { run: String, #[serde(default)] since: u64 },
    /// Guidance for a running run. **Control port**, and that is the whole point.
    ///
    /// M3-DESIGN §10.1 requires steering *"from outside"* — another terminal, no TUI, a script.
    /// On the serial main port a steer is not *read* until the turn it was meant to change has
    /// ended, so it is served by `crate::control_plane`, which shares only the run table and the
    /// `DurableControl` with the turn in flight.
    Steer { run: String, text: String },
    /// Ask a run to stop at its next iteration boundary. **Control port.**
    ///
    /// Never mid-tool-call: a cancel that interrupted a call would leave the call's effect
    /// unrecorded, and the journal is the thing that has to stay true.
    Cancel { run: String },
    /// Continue a run from its last completed checkpoint. **Main port** — it needs the engine.
    ///
    /// The control plane could *stage* a resume and deliberately does not: staging without
    /// driving would report success for a run that never took another step.
    Resume { run: String },
    /// Approve or decline a pending decision (§B9). The harness enforces; the model never sees
    /// this path.
    /// **`reason` is only meaningful on a decline.** "No, because ..." is a different
    /// instruction to the model than "no": it usually says what an acceptable call would be.
    Approve { decision: u64, granted: bool, reason: Option<String> },
    /// Re-send a session's turns so a reconnecting client can rebuild its view.
    ///
    /// **The screen and the model disagreed without this.** The daemon owns the session; a client
    /// that reconnects built its view from `Status` alone, which fills the control strip and the
    /// band and nothing else. So a live conversation rendered as a blank transcript, and Marlowe
    /// answered from forty turns of context the user could not see.
    ///
    /// It replays **render-only events** — the same frames a live turn produces — so §2.14 still
    /// holds: the client is re-projecting what the daemon owns, not taking custody of it.
    Replay { session: String },
    /// Change the model the daemon routes to.
    ///
    /// **A request, not an assignment.** The daemon verifies the endpoint actually has it and
    /// refuses by name otherwise — §2.14 again: the client proposes, the daemon decides, and the
    /// picker re-reads the daemon's answer rather than assuming its own optimism was accepted.
    ///
    /// The conversation is unaffected: the session store is keyed by name, not by model, so a
    /// switch mid-conversation continues the same thread with a different model. The **disclosure**
    /// changes with it, which is the point — `capability_for` reports NOT MEASURED for anything but
    /// the one model that has been.
    SetModel { model: String },
    /// ADR-049 §7. `ollama` or `openrouter`.
    ///
    /// **Separate from `SetModel` because the model list is a consequence of it**, not a peer:
    /// switching provider replaces the set of models that can be chosen at all, so a client that
    /// sent both would be racing its own picker against a list that had not arrived yet. The
    /// daemon answers with a fresh `Status`, and the new list is in it.
    SetProvider { provider: String },
    /// Stop the daemon.
    ///
    /// **Invariant 6 says a run survives the client that started it, not that the daemon is
    /// immortal.** Without this there is no way to stop one at all: closing the TUI leaves it
    /// listening, and the next launch reconnects to it — which is how a fixed build gets tested
    /// against a stale binary. It refuses while a run is live, so the invariant still holds where
    /// it means something.
    Shutdown,

    // ── the control plane a run window speaks to (`M3-DESIGN.md` §6) ────────────────────────
    //
}

/// One frame of a run's output. `M3-DESIGN.md` §6.2, ADR-055.
///
/// # Why this is not `Event` reused
///
/// [`Event::Text`] and friends are **this conversation's** stream — the thing the main pane draws.
/// A run's output is a different subject with a different governing decision (ADR-055 permits it;
/// nothing permits raw tool results), and conflating them would mean a change to one silently
/// changing the other. They look alike because they describe the same kinds of thing, not because
/// they are the same channel.
///
/// **There is no `ToolResult` variant and there must not be one.** What crosses is model prose and
/// the harness's own §B6 summary line. A fetched page reaches a window only after
/// `condense_batch`, exactly as it reaches the main pane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum RunFrame {
    Text { delta: String },
    /// `tokens` is what this chunk cost, from the engine — see [`Event::Reasoning`].
    Reasoning { delta: String, tokens: u64 },
    /// The speech streamed so far this turn was reasoning after all. The window moves it, so no
    /// text that belonged inside a think block is left in the response colour. `tokens` is what
    /// that speech cost, moving with it.
    SpeechRetracted { tokens: u64 },
    Tool { id: u64, verb: String, target: String, state: String, summary: String },
    Compacted { turns: u32 },
}

/// How much of a tool result [`Event::Tool::detail`] carries.
///
/// # Derived, not chosen
///
/// `read` returns a window of [`marlowe_exec::READ_WINDOW_BYTES`]. Binding this to that constant
/// is what makes §B6's promise — *"Enter for full output in place"* — literally true for the tool
/// whose whole purpose is to put a file on screen: a `read` is never cut here, because it was
/// already cut there. Anything bigger than a `read` window is already a `ToolBody::Reference` with
/// a head-and-tail preview, so the harness has decided the MODEL does not get it whole; a terminal
/// pane has no stronger claim than the model does.
///
/// **`MAX_INLINE_BYTES` would have been the wrong constant to reuse.** It is a token-budget bound
/// on what reaches attention. A pane and a token budget are different systems, and carrying a
/// number across that boundary because it is nearby is the mistake CLAUDE.md names as *"a
/// measurement is scoped to the system it was taken on"*.
///
/// The bound is not decorative. `bash` may return [`marlowe_exec::MAX_SHELL_OUTPUT_BYTES`] **per
/// stream** — about 2 MB — onto a newline-delimited JSON socket whose reader is an unbounded
/// `read_line`.
pub const MAX_TOOL_DETAIL_BYTES: usize = marlowe_exec::READ_WINDOW_BYTES;

/// **One thing the daemon said about itself**, on the wire instead of only on stderr.
///
/// # The defect this closes
///
/// The daemon announces real things on its way up:
///
/// ```text
/// marlowe: engine llama.cpp · http://127.0.0.1:11437 · started in 1935 ms · GPU-resident (96 tok/s measured)
/// marlowe: memory retrieval WRITE-ONLY — no --reranking directory
/// marlowe: 3 interrupted run(s) can be resumed
/// ```
///
/// Every one of them goes to **stderr**, and §B17's launcher opens the terminal Marlowe draws in —
/// so the frame occupies the screen and the announcements land in a stream nobody is reading. A
/// user whose first turn takes two seconds has the reason printed a metre away from where they are
/// looking, in a place they cannot get to.
///
/// # `Info` and `Warn` and nothing else
///
/// The level decides a tone, and §B2 allows exactly three state colours. A third level would need a
/// third meaning, and the meanings are taken: amber is *needs attention*, red is *conflict, failure,
/// irreversible*, and nothing the daemon says on its way up is irreversible. So `Warn` is amber and
/// `Info` is dim, and dimming is load-bearing — §B2 again: an item needing nothing recedes so the
/// eye goes to the ones that do.
///
/// **Diagnostics are not here and must not be.** `daemon.rs` already prefixes its two kinds of line
/// differently — `marlowe:` for facts about the user's machine, `[dev]` for the outbound-request
/// dump and the raw provider frames — and only the first kind is an [`Announcement`]. §B1's
/// carve-out keeps instrumentation behind `--dev`; a 1.9-second engine start is not instrumentation,
/// it is the answer to *why was that slow*.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Announcement {
    pub level: AnnounceLevel,
    /// The sentence, exactly as it went to stderr, minus the `marlowe: ` prefix.
    ///
    /// **Daemon-authored, and that is what makes a `String` legitimate here.** ADR-030 §5 forbids
    /// free text in `marlowe_view::Notice` because that vocabulary is what *the surface* renders as
    /// Marlowe's own speech. This is a fact the daemon computed — the same standing as
    /// `StatusReport::model_disclosure` and `StatusReport::degraded`, both of which are `String`s
    /// that the band already renders verbatim. The surface quotes it; it never composes it.
    pub text: String,
}

/// How much attention an [`Announcement`] wants. See that type's header for why there are two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnounceLevel {
    /// A fact about the machine. Dim.
    Info,
    /// Something is not the way it was asked for. Amber, and counted in the pane's summary.
    Warn,
}

/// Daemon → client. Render-only, mirroring `TurnEvent` plus the frames a client needs to know
/// the turn is over.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// The daemon's identity and health, including what §B5's band needs.
    Status(StatusReport),
    Text { delta: String },
    /// A turn the **user** took. Only ever sent by `Replay`.
    ///
    /// A live client already knows what its own user typed and appends it locally; a reconnecting
    /// one does not, and without this the wire had no way to express "the person said this", so a
    /// replayed conversation would have been Marlowe talking to nobody.
    User { text: String },
    /// A chunk of the model's reasoning. **Not the answer**, and never part of the transcript.
    ///
    /// # `tokens` is on the wire because the client cannot compute it
    ///
    /// The collapsed head line reports how much thinking has happened. Sending only `delta` would
    /// leave the client counting characters — which is what it did — or estimating tokens from
    /// them, which is worse because the estimate wears the unit of a measurement.
    ///
    /// The count is the engine's, taken where the engine reports it, and it is a **delta the
    /// client adds**. `0` is ordinary: a buffer released after the frames that filled it were
    /// counted. An empty `delta` with a non-zero `tokens` is also ordinary — it is the settlement
    /// of what the engine generated and never streamed, and it must not open a thinking block
    /// that does not already exist.
    Reasoning { delta: String, tokens: u64 },
    /// The speech streamed so far this turn was reasoning. The client moves it, and no text that
    /// belonged inside a think block is left in the response colour. `tokens` is what that speech
    /// cost; it moves into the thinking block with the text, or the block's head line would
    /// report a figure for prose it no longer holds.
    SpeechRetracted { tokens: u64 },
    /// §B6's one line per call, **and what it opens to**.
    ///
    /// `summary` is the right-hand side: typed metrics, already rendered. `detail` is what §B6's
    /// *"Enter for full output in place"* puts on screen — the tool's body, or a refusal's reason,
    /// derived once by [`marlowe_loop::ToolOutcome::screen_detail`].
    ///
    /// **`Option`, and never defaulted to the summary.** A running line has no output yet, and a
    /// tool that produced nothing produced nothing; filling the field with the summary would make
    /// an expansion that shows `48 lines` indistinguishable from one that shows the file, which is
    /// exactly the state this field was added to end.
    ///
    /// Bounded by [`MAX_TOOL_DETAIL_BYTES`] at `to_wire`, never at the producer: the loop's own
    /// text is what the model received and a display bound must not shorten it.
    ///
    /// `#[serde(default)]` so a client built before this field still parses the frame, and
    /// `skip_serializing_if` so a running line does not carry a null — the same pair
    /// `Event::Run::attribution` uses.
    Tool {
        id: u64,
        verb: String,
        target: String,
        state: String,
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    Compacted { turns: u32 },
    /// Something the daemon said about itself, **while the client is attached**.
    ///
    /// The backlog — everything said before anyone connected — rides on
    /// [`StatusReport::announcements`] instead, because a client that reconnects to a daemon that
    /// has been up for an hour needs the same lines a client that watched it start would have. Two
    /// routes for one fact, and they carry the identical [`Announcement`]: the ring is the single
    /// source, and this variant is a flush of it rather than a second author.
    Announce(Announcement),
    /// **The turn in flight, measured.** §B5: *"the numbers that matter for that state."*
    ///
    /// # Both numbers or neither, enforced past this point
    ///
    /// The wire carries the raw quantities; `marlowe_view::Cadence` is what refuses to render one
    /// without the other, because that is where the rendering happens. Putting a pre-rendered
    /// string here would move the decision to the daemon and leave the surface free to split it
    /// again — see that type's header for the 218-vs-426 ms measurement that is the reason.
    ///
    /// `ttft_ms` is time to the **first token of any kind, `thinking` included**. `tokens` is the
    /// count of streamed **deltas**, and `since_first_ms` is the interval they were produced over,
    /// sent rather than derived so the client cannot choose a different denominator from the one
    /// the daemon measured.
    ///
    /// **A delta is not a token, and this field is deliberately still a count of deltas.** That
    /// sentence used to end "— one per token on both local engines", which was measured false on
    /// 2026-08-31 (see `daemon.rs`'s counter, and `OllamaDriver::call_streaming_split`). A rate is
    /// a ratio over an interval, so its numerator has to be what this process saw arrive while the
    /// clock ran; the engine's own count is corrected at the end of a call and cannot feed it.
    /// [`Event::Reasoning`]'s `tokens` is the exact quantity, and it answers a different question.
    ///
    /// **Emitted several times a turn, not once at the end.** A summary printed after the fact
    /// answers a different question from a number that ticks: §B12's third craft target is that a
    /// user never wonders whether it is working, and the way a rate answers that is by moving.
    Cadence { ttft_ms: u64, tokens: u64, since_first_ms: u64, warm: bool },
    /// Invariant 4. Carries the **remedy**, not just the fact.
    Degraded { what: String, remedy: String },
    /// §B9. The client renders; the daemon decides.
    /// §B9's approval prompt. **The blast radius, never the command.**
    ///
    /// `novelty` is `Option` and is never defaulted: §B9 wants a novelty reason *and* a ceiling,
    /// the ceiling has no producer until the trust ledger at M6, and sending `"routine"` because
    /// nothing said otherwise would be a claim about promotion logic nobody has written. A missing
    /// field renders as missing.
    Approval {
        decision: u64,
        verb: String,
        scope: String,
        reversible: bool,
        novelty: Option<String>,
    },
    /// The turn ended. `outcome` distinguishes completed / paused / escalated / failed.
    Done { outcome: String, detail: String, spend_micros_usd: u64, elapsed_ms: u64 },
    /// A run the daemon owns.
    ///
    /// `attribution` is **ADR-046 §3**: which model actually answered and which upstream served
    /// it. `None` on the local path, where the question does not arise. `#[serde(default)]` so a
    /// client built before this field still parses the frame.
    Run {
        id: String,
        status: String,
        tokens: u64,
        depth: u8,
        #[serde(default)]
        attribution: Option<String>,
    },
    /// One run, in full. **The state a window renders, and the state `/watch` prints.**
    ///
    /// §6.6: *"One state, two renderings."* The window and the Runs tab read this; neither keeps
    /// its own. Every field is a fact the daemon holds — `last_checkpoint_step` is `None` when
    /// there is no checkpoint, because §6.3's rule is that a placeholder states a fact and never
    /// invents one to fill a layout.
    ///
    /// It carries **no** `CapabilityProfile`, no `Budget` object and no `Checkpoint`: numbers read
    /// off them, which is what `no_request_or_event_hands_the_client_run_state` is about.
    RunDetail {
        id: String,
        status: String,
        parent: Option<String>,
        elapsed_ms: u64,
        spend_micros_usd: u64,
        ceiling_micros_usd: u64,
        spent_tokens: u64,
        granted_tokens: u64,
        depth: u8,
        /// `None` means no checkpoint exists — not step zero.
        last_checkpoint_step: Option<u32>,
        resumable: bool,
        /// Stated plainly beside cancel, per §6.2.
        orphan_policy: String,
        pending_steers: usize,
        /// The runs this one spawned. **§6.3's roster panel, and until M3 Session B1 there was
        /// nothing that could fill it** — `run` could not spawn, so `RunView::subagents` was a
        /// hardcoded empty vector and the panel read `subagents — none` for every run there had
        /// ever been. Empty here is now a fact about a childless run rather than about the build.
        ///
        /// Derived from the checkpoint store, which is where `parent` already comes from — not
        /// from the run table, which is what the daemon remembers rather than what survived.
        subagents: Vec<RunChild>,
    },
    Error { detail: String },

    /// One frame of a watched run's output, in order. **Session F; ADR-055.**
    ///
    /// `RunDetail` above is what a run *is*; this is what it has *said*. They travel together in a
    /// `Watch` answer and are separate variants because they have different governing decisions:
    /// the detail is facts the harness computed, and a frame is prose ADR-055 permits to reach a
    /// terminal on the condition that the display predicate runs at the boundary.
    ///
    /// **There is deliberately no acknowledgement variant beside these.** A write is answered with
    /// a fresh `RunDetail`, which carries `pending_steers` — Session A's rule, and the better one:
    /// *"the count, not an acknowledgement. 'queued' is a claim about a mechanism; a number is a
    /// fact the next `/watch` can be checked against."*
    RunOutput { seq: u64, frame: RunFrame },
}

/// One child of a watched run, as its parent's roster panel shows it.
///
/// **Two fields, and the id is not decoration.** A run's label is its mnemonic; 4096 names means
/// two live runs can share one, so a roster folded by name could merge two children into a row.
/// [`marlowe_view::model::Item`] keeps the same pair for the same reason.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunChild {
    pub id: String,
    pub status: String,
}

/// What `Status` answers. Everything §B5's band and the first-run disclosure need, in one frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusReport {
    pub version: String,
    /// The workspace the run is scoped to. First-run onboarding states this (ADR-002).
    pub workspace: String,
    pub model: String,
    /// ADR-028 requirement 2 — carries its denominator, or says NOT MEASURED.
    pub model_disclosure: String,
    /// `None` when everything is ready. Otherwise the specific remedy.
    pub degraded: Option<String>,
    /// ADR-029: **the active provider is announced, never silently chosen.** This is read from
    /// the field the profile row already stamps; it is not a second source of the same fact.
    pub rerank_provider: String,
    /// ADR-046, and ADR-029's rule applied to the model provider: **announced, never inferred.**
    /// `ollama` or `openrouter`. Read from `DaemonConfig::model_provider()`, which is the same
    /// function the run path selects a driver with — not a second source of the same fact.
    #[serde(default)]
    pub model_provider: String,
    /// Runs the daemon currently owns. Non-zero across a client restart is what makes
    /// invariant 6 observable rather than asserted.
    pub live_runs: usize,
    /// Every model this machine's Ollama holds that the daemon would accept, `model` included.
    ///
    /// **The daemon's answer, not the client's guess.** §2.14: the surface holds no state the
    /// daemon lacks, so the model picker is built from this list rather than from a literal. Cloud
    /// tags are **excluded** — `Routing::uniform` refuses them, so offering one would be a control
    /// that produces a refusal on selection.
    ///
    /// Empty when the endpoint is unreachable, which is the truthful reading: the picker then
    /// carries only the configured model and `degraded` says why.
    #[serde(default)]
    pub models: Vec<String>,
    /// **Everything the daemon has said about itself, oldest first.** §B7's Status tab.
    ///
    /// Bounded — see `crate::announce::CAPACITY`. It rides on `Status` rather than only on
    /// [`Event::Announce`] because almost all of it is said *before any client exists*: the engine
    /// starts, the provider is announced and the resumable runs are counted while the socket is
    /// still being bound. A live-only channel would deliver an empty log to the one person who
    /// most wants it, and would deliver a different log to a client that reconnected.
    ///
    /// `#[serde(default)]` so a client built before this field still parses the frame.
    #[serde(default)]
    pub announcements: Vec<Announcement>,
    /// Milliseconds the daemon has been up. §B7 lists *daemon uptime* among what Status holds, and
    /// §B5's `idle` band carries it.
    ///
    /// **A duration, never a start timestamp.** A timestamp on this wire would be a clock reading
    /// crossing a boundary, and CONTRACTS §4.5's whole argument is about where those may exist; a
    /// monotonic elapsed count is not one and cannot be turned back into one by the client.
    #[serde(default)]
    pub uptime_ms: u64,
}

/// Read one NDJSON value per line.
pub fn read_line<T: for<'de> Deserialize<'de>>(
    reader: &mut impl std::io::BufRead,
) -> std::io::Result<Option<T>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    if line.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&line)
        .map(Some)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Write one NDJSON value and flush. **A response sitting in a buffer is indistinguishable from
/// a hang** — the same rule §4.0.2 states for the eval transport.
pub fn write_line<T: Serialize>(
    writer: &mut impl std::io::Write,
    value: &T,
) -> std::io::Result<()> {
    let mut s = serde_json::to_string(value)?;
    s.push('\n');
    writer.write_all(s.as_bytes())?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_request_or_event_hands_the_client_run_state() {
        // ARCHITECTURE §2.14: surfaces hold no state the daemon lacks, and §6 splits the binary
        // because of invariant 6. A frame carrying a Checkpoint, a ContextView or a transcript
        // would let a client hold something the daemon owns — and would make "closing the
        // terminal ends the run" true again by the back door.
        let json = serde_json::to_string(&Event::Done {
            outcome: "completed".into(),
            detail: "42".into(),
            spend_micros_usd: 0,
            elapsed_ms: 10,
        })
        .unwrap();
        for forbidden in ["checkpoint", "transcript", "context_view", "provenance", "taint"] {
            assert!(!json.contains(forbidden), "`{forbidden}` crossed the boundary: {json}");
        }

        // And the Run frame is a summary, not the object.
        let run = serde_json::to_string(&Event::Run {
            attribution: None,
            id: "r".into(),
            status: "running".into(),
            tokens: 10,
            depth: 0,
        })
        .unwrap();
        assert!(!run.contains("profile"), "{run}");
        assert!(!run.contains("budget"), "{run}");
    }

    #[test]
    fn a_degraded_event_carries_its_remedy() {
        // Invariant 4 as a wire requirement: a client that received only "degraded" could not
        // tell the user what to do, and a degraded state a user cannot act on is a crash with
        // better manners.
        let e = Event::Degraded {
            what: "no model available".into(),
            remedy: "start it with `ollama serve`".into(),
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("ollama serve"), "{json}");
    }

    #[test]
    fn frames_round_trip_as_ndjson() {
        let mut buf: Vec<u8> = Vec::new();
        write_line(&mut buf, &Request::Ask { session: "s".into(), message: "hi".into() }).unwrap();
        assert!(buf.ends_with(b"\n"), "every frame is newline-terminated");
        assert_eq!(buf.iter().filter(|b| **b == b'\n').count(), 1, "no embedded newlines");

        let mut reader = std::io::BufReader::new(&buf[..]);
        let back: Option<Request> = read_line(&mut reader).unwrap();
        assert_eq!(back, Some(Request::Ask { session: "s".into(), message: "hi".into() }));
    }

    #[test]
    fn the_status_report_announces_the_provider_rather_than_implying_it() {
        // ADR-029, inherited: the active provider is announced, never silently chosen, because
        // an unannounced fallback is indistinguishable from the failure mode it resembles.
        // `rerank_provider` is a REQUIRED field — a client cannot render the band without it,
        // so there is no path where it is quietly omitted.
        let r = StatusReport {
            version: "0.1.0".into(),
            workspace: "/ws".into(),
            model: "qwen3.5:9b".into(),
            model_disclosure: "qwen3.5:9b · tool calls 12/12".into(),
            degraded: None,
            rerank_provider: "cpu-sequential".into(),
            model_provider: "ollama".into(),
            live_runs: 0,
            models: vec!["qwen3.5:9b".into()],
            // Nothing has been announced into this fixture and nothing has been up.
            announcements: Vec::new(),
            uptime_ms: 0,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("rerank_provider"), "{json}");
        // Not an Option: a missing announcement must be a compile error, not a None.
        let parsed: StatusReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.rerank_provider, "cpu-sequential");
    }
}
