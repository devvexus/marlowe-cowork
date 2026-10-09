//! `TurnEvent`, exactly as pinned in CONTRACTS.md §13.
//!
//! **Render-only. Surfaces hold no policy and no state the daemon lacks.**
//!
//! The M1 shapes are narrower than the pinned ones — `CallId`, `Money`, `BlastRadius` and
//! `ContentRef` are M2 types and do not exist yet — but the **variant set is the contract's, and
//! it is closed**. Nothing is added.
//!
//! # There is no `MemoryInjected` variant, and there must never be one
//!
//! §B1 is binding: memory gets no representation in the interface. This is not a rule the code
//! merely follows — `tests/no_memory_surface.rs` fails **by name** if a variant matching the
//! memory vocabulary appears here, because a rule that only lives in prose is a rule that erodes
//! the first time somebody wants a debug view.
//!
//! A `recall` **tool line** is permitted and is not an exception: it is a `ToolLine` like any
//! other, carrying the verb `recall`, and it says nothing about scores, provenance or precision.
//! What is forbidden is a region whose subject is the memory system.

/// A tool call's rendered state. §B6: one line, typed summary, failures auto-expand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolLineState {
    /// Animating in place with elapsed time. **Never scrolled in and then cleared.**
    Running { elapsed_ms: u64 },
    Ok(ResultSummary),
    /// Auto-expands. The one case where the user always wants detail (§B6).
    Failed(ResultSummary),
}

/// §B6: *"Summaries are typed, never generic. `done` is not acceptable."*
///
/// The type is what enforces that. There is no `ResultSummary::Text(String)` variant to reach for,
/// so a tool cannot report `done` without adding a metric kind and defending it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultSummary {
    pub metrics: Vec<Metric>,
    /// Expansion payload. `None` means there is nothing more to show, not that it was not fetched.
    pub detail: Option<String>,
}

impl ResultSummary {
    pub fn new(metrics: Vec<Metric>) -> Self {
        Self {
            metrics,
            detail: None,
        }
    }

    pub fn with_detail(metrics: Vec<Metric>, detail: impl Into<String>) -> Self {
        Self {
            metrics,
            detail: Some(detail.into()),
        }
    }

    /// The right-hand side of a tool line: `48 lines`, `+3 −0`, `12 passed · 1.4s`.
    pub fn render(&self) -> String {
        self.metrics
            .iter()
            .map(Metric::render)
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

/// CONTRACTS.md §8's `Metric`, which is the typed-summary contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Metric {
    Count { n: u64, unit: &'static str },
    Diff { added: u32, removed: u32 },
    Test { passed: u32, failed: u32, decis: u32 },
    Duration { ms: u64 },
    Bytes { n: u64 },
    /// A process exit that was not zero. Distinct from `Count` because it renders in the failure
    /// colour and because "exit 1" is not a count of anything.
    Exit { code: i32 },
    /// A one-word outcome for a call with nothing to count — `spawned`, `queued`. Deliberately
    /// `&'static str`, so it cannot become a free-text escape hatch for `done`.
    State(&'static str),
}

impl Metric {
    pub fn render(&self) -> String {
        match self {
            Metric::Count { n, unit } => {
                if *n == 1 {
                    // "1 lines" is the tell of a system that never looked at its own output.
                    format!("1 {}", unit.trim_end_matches('s'))
                } else {
                    format!("{n} {unit}")
                }
            }
            // U+2212 MINUS SIGN, matching the mockup. A hyphen next to a plus reads as a dash.
            Metric::Diff { added, removed } => format!("+{added} \u{2212}{removed}"),
            Metric::Test {
                passed,
                failed,
                decis,
            } => {
                let secs = format!("{}.{}s", decis / 10, decis % 10);
                if *failed == 0 {
                    format!("{passed} passed · {secs}")
                } else {
                    format!("{passed} passed · {failed} failed · {secs}")
                }
            }
            Metric::Duration { ms } => format!("{ms} ms"),
            Metric::Bytes { n } => format!("{n} B"),
            Metric::Exit { code } => format!("exit {code}"),
            Metric::State(s) => (*s).to_string(),
        }
    }
}

/// Which degraded path the run is on. §B5: rendered in the status band, in amber, with the reason
/// in the Status tab. Invariant 4 — degrade, never break, and **never silently**.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DegradedPath {
    /// `dense retrieval offline · lexical only`
    DenseRetrievalOffline,
    /// Voice pipeline down; text still works.
    VoiceUnavailable,
    /// A provider failed over. The run continues on another.
    ProviderFailedOver,
    /// Something is degraded that this enum has no specific name for. **The remedy carries it.**
    ///
    /// The projection used to fall back to `ProviderFailedOver` for any unrecognised remedy, so
    /// the band read *"failed over · secondary provider"* for a daemon whose only problem was a
    /// stale binary. That is worse than saying nothing: it is a specific, false claim about a
    /// component that never failed, and a user acting on it would go looking at providers.
    ///
    /// A degraded state must be visible (invariant 4) AND accurate. When the class is unknown,
    /// the honest headline says so and defers to the remedy text.
    Unclassified,
    /// ADR-023's latch engaged: the run read untrusted content and cannot act on composed targets.
    TrustFloorLatched,
    /// **ADR-060.** The `ollama/llama.cpp` provider is selected, `llama-server` could not serve,
    /// and **Ollama is answering instead**. The run continues; the engine is not the one chosen.
    ///
    /// # Deliberately NOT `ProviderFailedOver`, and the distance is the point
    ///
    /// That path's headline is *"failed over · secondary provider"* and its reason is *"run state
    /// preserved across the switch"*. Both sentences describe a **hosted secondary taking over
    /// from a primary** — a different event, involving a provider this user may not have, possibly
    /// costing money, possibly off this machine. Nothing of the kind happened: the model is the
    /// same model, the weights are the same blob, the answers are unaffected, and the only thing
    /// that changed is which local process ran the forward pass.
    ///
    /// This project has already shipped one banner whose text asserted something the event did not
    /// mean (`TrustFloorLatched`, M2 C2f, where *"read untrusted"* fired on the string
    /// `"Marlowe."`). Reusing `ProviderFailedOver` here would be the same mistake with a different
    /// subject, so this gets its own path and its own words.
    ///
    /// **The specific cause travels in the remedy, not here.** *Which* failure — port taken, blob
    /// unresolvable, GPU full, server exited, binary missing — varies per incident, and a
    /// `&'static str` cannot carry it. §B5 puts the headline in the band and the reason in the
    /// Status tab; the daemon's `degraded` string is what names the cause, and it is held for the
    /// life of the session rather than flashed once.
    EngineFellBackToOllama,
    /// **The running binary predates the source it was built from.** Nothing in the product is
    /// broken; the process is answering from code that no longer exists on disk.
    ///
    /// # Why this earned a variant instead of staying `Unclassified`
    ///
    /// It was `Unclassified`, whose headline is *"degraded · see the Status tab"* — and the daemon
    /// **knew exactly what was wrong**. `staleness::stale_against_source` compares the executable's
    /// mtime against the newest `.rs` under `crates/` and returns a sentence naming the remedy;
    /// `classify_degradation` then matched none of its keywords and threw that specificity away
    /// one step before the band.
    ///
    /// So the band said *go and look* about the one condition that is trivially actionable, while
    /// `EngineFellBackToOllama` — rarer and less actionable — got a precise headline. That is
    /// backwards, and this file's own rule says so: *"a degraded state a user cannot act on is a
    /// crash with better manners."* Reported by the human, who read the vague line and had to ask
    /// what it meant.
    ///
    /// **This is the condition a developer hits most**, because it fires on every source edit — so
    /// it is the headline most worth being exact.
    BinaryBehindSource,
}

impl DegradedPath {
    /// The status-band line. Specific, never the word "degraded" alone.
    pub fn headline(self) -> &'static str {
        match self {
            DegradedPath::DenseRetrievalOffline => "dense retrieval offline · lexical only",
            DegradedPath::VoiceUnavailable => "voice offline · text only",
            DegradedPath::ProviderFailedOver => "failed over · secondary provider",
            DegradedPath::Unclassified => "degraded · see the Status tab",
            DegradedPath::TrustFloorLatched => "read untrusted · composed targets blocked",
            // Names BOTH engines: the one that is not serving and the one that is. A headline
            // saying only "llama.cpp unavailable" would leave the reader unable to tell whether
            // anything is answering at all.
            DegradedPath::EngineFellBackToOllama => "llama.cpp not serving · Ollama is",
            // Names the ACTION, not the condition. "binary is stale" is a description; "rebuild
            // and restart" is what closes it, and it fits the band.
            DegradedPath::BinaryBehindSource => "binary behind source · rebuild and restart",
        }
    }

    /// The reason, which lives in the Status tab rather than the band (§B5).
    pub fn reason(self) -> &'static str {
        match self {
            DegradedPath::DenseRetrievalOffline => "recall quality reduced · not silent",
            DegradedPath::VoiceUnavailable => "text is unaffected",
            DegradedPath::ProviderFailedOver => "run state preserved across the switch",
            DegradedPath::Unclassified => "the remedy is stated where this was raised",
            DegradedPath::TrustFloorLatched => {
                "ADR-023 — spawn a quarantined reader and let an untainted run act on its findings"
            }
            DegradedPath::EngineFellBackToOllama => {
                "same model, same answers — only the engine changed; ~225 ms/request slower"
            }
            DegradedPath::BinaryBehindSource => {
                "nothing in the product is degraded — this process is serving code that has changed on disk"
            }
        }
    }
}

/// The loop → surface stream. CONTRACTS.md §13, closed.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnEvent {
    TextDelta(String),
    ToolLine {
        id: u64,
        verb: &'static str,
        target: String,
        state: ToolLineState,
    },
    Compacted {
        turns: u32,
    },
    /// The status band, in amber, §B5.
    Degraded {
        what: DegradedPath,
    },
    /// The only element permitted to DIM the frame, §B9.
    ApprovalPrompt(crate::approval::BlastRadius),
    Done {
        spend_cents: u32,
        elapsed_ms: u64,
        fill_pct: f32,
    },
}
