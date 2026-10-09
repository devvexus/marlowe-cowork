//! CONTRACTS.md §13 — the loop → surface stream. **Render-only.**
//!
//! # There is no `MemoryInjected` variant, and there must never be one
//!
//! §B1 is binding: memory gets no representation in the interface. A `recall` **tool line** is
//! permitted and is not an exception — it is a `ToolLine` like any other, carrying the verb
//! `recall`, and it says nothing about scores, provenance or precision. What is forbidden is a
//! region whose subject is the memory system. Diagnostics reach `--dev` through a separate
//! channel that is not part of this enum.
//!
//! # Duplication with `marlowe-stub::turn`, named rather than left to be discovered
//!
//! M1's scripted stub defines its own `TurnEvent` for the surface it drives. This is the
//! canonical one — it is produced by the actual loop and carries the real
//! [`marlowe_permission::BlastRadius`] rather than the stub's rendered view model. **Session E
//! deletes the stub's copy and points `marlowe-surface` here.** It is not done in this session
//! because reconciling the two `BlastRadius` shapes is TUI work, and churning 87 passing M1
//! tests buys nothing an acceptance row asks for.

use marlowe_permission::BlastRadius;
use marlowe_tools::ResultSummary;

/// A tool call's rendered state. §B6: one line, typed summary, failures auto-expand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolLineState {
    /// Animating in place with elapsed time. **Never scrolled in and then cleared.**
    Running { elapsed_ms: u64 },
    Ok(ResultSummary),
    /// Auto-expands. The one case where the user always wants detail.
    Failed(ResultSummary),
}

/// Which degraded path the run is on. §B5, invariant 4 — degrade, never break, and never
/// silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DegradedPath {
    DenseRetrievalOffline,
    VoiceUnavailable,
    ProviderFailedOver,
    /// **The audit log refused a write.** Invariant 7: a run that continued past a dropped event
    /// would be unreconstructable, so this is never silent.
    ///
    /// It has its own variant because it was previously emitted as a `TextDelta` — harness text
    /// rendered as Marlowe's own prose, interleaved mid-sentence into the conversation. That made
    /// a data-integrity failure look like the model babbling, which is the wrong message about the
    /// wrong component.
    JournalAppendFailed,
    /// The run read untrusted content; model-composed targets are blocked from here on.
    ///
    /// Surfaced because it changes what Marlowe can do for the rest of the run, and a user who
    /// sees a later action refused deserves to know when the constraint engaged.
    TrustFloorLatched,
    /// ADR-028: the local Ollama endpoint is absent, or the routed model is not pulled.
    ///
    /// A declared value on the run, not a crash — invariant 4. The *specific* remedy
    /// (`ollama serve`, `ollama pull <model>`) lives in `Availability::remedy`, because a
    /// degraded state a user cannot act on is a crash with better manners.
    ModelUnavailable,
}

impl DegradedPath {
    pub fn headline(self) -> &'static str {
        match self {
            DegradedPath::JournalAppendFailed => "audit log write refused · run not recorded",
            DegradedPath::TrustFloorLatched => {
                "read untrusted content · composed targets blocked for this run"
            }
            DegradedPath::DenseRetrievalOffline => "dense retrieval offline · lexical only",
            DegradedPath::VoiceUnavailable => "voice offline · text only",
            DegradedPath::ProviderFailedOver => "failed over · secondary provider",
            DegradedPath::ModelUnavailable => "no model available · see status",
        }
    }
}

/// No `Eq`, because `Done` carries a ratio. Every other variant is comparable.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnEvent {
    TextDelta(String),
    /// A chunk of the model's **reasoning**, not its answer.
    ///
    /// Reasoning models emit these before any answer text — `qwen3.5:9b` spent **2,615 of 2,862
    /// frames** on them in one `Hello`. Without a variant of their own they were invisible, so a
    /// model working hard read as a hung process, and §B5's *motion means Marlowe is working* had
    /// nothing to move on.
    ///
    /// **Separate from `TextDelta` deliberately.** It is not what Marlowe said, it must never
    /// enter the transcript a `Y` copy produces, and it is collapsed by default — the user asked
    /// for a question, not a monologue.
    ///
    /// # `tokens` is COUNTED AT THE WIRE, NOT DERIVED FROM `text`
    ///
    /// The collapsed head line reports how much thinking has happened, and it reported
    /// `text.len()` — characters — because that is the number a renderer can compute for itself.
    /// A renderer cannot tokenize, so any count it derives is an estimate wearing a unit.
    ///
    /// So the count travels with the chunk. **One streamed frame is one token on both local
    /// engines**, and the provider is the only component that can see a frame, so it is the only
    /// component that may say how many tokens a chunk carries. Downstream **adds**; it never
    /// re-derives. A chunk that carries no new token — a buffer flushed after the frames that
    /// filled it were already counted — carries `0`, and that is not a bug to be tidied away.
    ReasoningDelta { text: String, tokens: u64 },
    /// **Everything streamed as speech this turn was reasoning.** Take it back.
    ///
    /// A model can close a think block it opened before the `content` channel began, so the
    /// harness learns the text was reasoning only *after* it has rendered it as Marlowe speaking
    /// — the `</think>` on screen in the Cass Lake transcript. There is no online way to know a
    /// closing tag is coming; the choice is to hold every content byte until the turn ends, for
    /// every model, or to correct the rare turn that misbehaves. Streaming is a hard requirement,
    /// so it is corrected.
    ///
    /// `tokens` is how many the retracted speech cost. The text moves into the thinking block, so
    /// its tokens move with it — without this the block's head line would report a count for
    /// prose it no longer holds and omit the prose it just gained.
    SpeechRetracted { tokens: u64 },
    ToolLine { id: u64, verb: String, target: String, state: ToolLineState },
    Compacted { turns: u32 },
    Degraded { what: DegradedPath },
    /// The only element permitted to DIM the frame, §B9.
    ApprovalPrompt(BlastRadius),
    Done { spend_micros_usd: u64, elapsed_ms: u64, fill_pct: f32 },
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_variant_set_carries_nothing_about_memory() {
        // §B1, asserted the only way a closed enum can be: by naming what is absent. This
        // mirrors `marlowe-surface`'s existing check and exists here too because this is the
        // enum the real loop emits, and the rule has to hold where the events are produced.
        let names = [
            "TextDelta",
            "ReasoningDelta",
            "ToolLine",
            "Compacted",
            "Degraded",
            "ApprovalPrompt",
            "Done",
        ];
        // Seven since M2 C2e added `ReasoningDelta`. **The count is the prompt, not the rule**:
        // it forces whoever adds a variant to answer the question below rather than slipping one
        // past a list nobody re-reads. `ReasoningDelta` carries the model's chain of thought — it
        // is about the provider, not about what Marlowe knows, so §B1 is untouched.
        assert_eq!(names.len(), 7, "a variant was added; is its subject the memory system?");
        for n in names {
            let lower = n.to_lowercase();
            assert!(
                !lower.contains("memory")
                    && !lower.contains("inject")
                    && !lower.contains("recall")
                    && !lower.contains("retriev"),
                "`{n}` names the memory system in the interface, which §B1 forbids"
            );
        }
    }
}
