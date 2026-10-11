//! **The one door a steer comes through.** ADR-054.
//!
//! # Why a steer needs a door at all, when a chat message does not
//!
//! `Engine::run` does this at every iteration boundary:
//!
//! ```ignore
//! provenance.attribute_user_message(&steer.text);
//! state.push(Block::new(SourceKind::History, format!("[steer] {}", steer.text),
//!                       TrustClass::UserAsserted));
//! ```
//!
//! [`Provenance::attribute_user_message`] inserts the whole message **and every whitespace-separated
//! token** at `UserAsserted`. And [`Provenance::taint_for`] consults that map *before* it reaches for
//! the run's floor:
//!
//! ```ignore
//! ArgValue::Text(s) => self.attributed.get(s.as_str()).copied().unwrap_or(floor),
//! ```
//!
//! An attributed token therefore does **not** carry the latched floor — which is correct, because
//! ADR-023 blocks targets *composed by untrusted content* and a target the human typed is not
//! model-composed. A floor that swallowed the user's own words would make a poisoned run unusable
//! rather than safe.
//!
//! The consequence is the reason this file exists:
//!
//! > **A steer is the only channel that writes new strings into `UserAsserted` in a run that is
//! > already latched at `UntrustedContent`.**
//!
//! In a latched run every other value is at the bottom, so the floor has no discriminating power
//! left. What survives saturation is not *how trusted is this text* but **who asserted it** — the
//! question CLAUDE.md records as the one that outlives a saturated floor.
//!
//! # One door, and it is greppable
//!
//! [`admit`] is the only function in the workspace that constructs a [`SteerMessage`], and this file
//! is the only one outside tests where the literal `SteerMessage {` appears.
//! `tests/steer_has_one_door.rs` greps for it and fails by name — the same construction
//! `region_contract.rs` uses to keep `Block::bordered` inside `Region::block`.
//!
//! The window's steer field and `/steer` are **not two paths that happen to agree**. They are this
//! call with a different [`SteerOrigin`], which is what makes §6.1's *"never a side door that skips
//! it"* a property of the call graph rather than of a review.

use marlowe_contract::text::sanitize_prose;

use crate::driver::{SteerMessage, Urgency};

/// A generous ceiling on one correction. Reached by no ordinary steer.
///
/// **The number is a refusal threshold, not a truncation point** — see [`SteerRefused::TooLong`] and
/// ADR-054 §5. It is sized so that a paragraph of redirection passes and a pasted document does not:
/// the tokeniser inserts every whitespace-separated token at `UserAsserted`, so this is the bound on
/// how many strings one steer can attribute.
pub const MAX_STEER_CHARS: usize = 2_000;

/// **Who is asserting this text.** The channel, never the content.
///
/// There is deliberately no `Model` or `Tool` variant. A steer that could arrive from something
/// other than a person is a laundering path into `UserAsserted`, and expressing it as "admitted at a
/// lower class" would leave a shape somebody later widens. The type simply cannot say it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteerOrigin {
    /// A line a person typed, arriving on an authenticated control-plane connection — `/steer`, or
    /// a run window's steer field. **The only origin admitted.**
    Human,
}

/// Why a steer was not queued. Every variant is something to show the person who typed it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SteerRefused {
    #[error("a steer with nothing in it is not a correction; type something, or cancel the run")]
    Empty,
    #[error(
        "that steer is {len} characters and the limit is {max}. It is refused rather than \
         truncated: half an instruction is worse than none. Shorten it and send it again"
    )]
    TooLong { len: usize, max: usize },
}

/// **The only constructor of a [`SteerMessage`] in the workspace.**
///
/// Three checks, in this order, and the order matters: shape is settled *before* the text is ever
/// handed to something that will attribute it.
///
/// 1. **Authority** — carried by [`SteerOrigin`], which is a channel fact. It is a parameter rather
///    than something inferred here, because inferring it from the text is exactly the mistake.
/// 2. **Shape** — `sanitize_prose` then the cap. Neither is hygiene. The sanitiser keeps C1, the
///    BiDi overrides and the tag block out of a block that renders in two surfaces; the cap bounds
///    how many tokens one steer can push into `UserAsserted`.
/// 3. **Emptiness** — checked on the *sanitised* text, so a steer made entirely of characters the
///    sanitiser marks does not become an empty `[steer]` block in a run's history.
///
/// **What this does not do:** it does not lift the run's latched trust floor, and it cannot —
/// `Run::latch_trust_floor` only ever lowers. It does not call `Adjudicator::adjudicate`, because a
/// steer is not a tool call over a manifest and a decision record naming a tool nobody called would
/// read as evidence while being about nothing.
pub fn admit(
    origin: SteerOrigin,
    text: &str,
    urgency: Urgency,
) -> Result<SteerMessage, SteerRefused> {
    let SteerOrigin::Human = origin;

    let clean = sanitize_prose(text);
    let clean = clean.trim();

    if clean.is_empty() {
        return Err(SteerRefused::Empty);
    }
    // Counted in characters, not bytes: the limit the refusal quotes must be the one a person can
    // count in the field they typed into.
    let len = clean.chars().count();
    if len > MAX_STEER_CHARS {
        return Err(SteerRefused::TooLong { len, max: MAX_STEER_CHARS });
    }

    Ok(SteerMessage { text: clean.to_string(), urgency })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ordinary_correction_is_admitted_unchanged() {
        // The control for everything below: the door is a door and not a wall.
        let m = admit(SteerOrigin::Human, "stop editing and summarise what you found", Urgency::Advisory)
            .expect("an ordinary correction must pass");
        assert_eq!(m.text, "stop editing and summarise what you found");
        assert_eq!(m.urgency, Urgency::Advisory);
    }

    #[test]
    fn the_display_predicate_runs_before_the_text_is_ever_attributed() {
        // Not hygiene. This string is pushed into a `History` block that renders in the main pane
        // AND in a run window, and every token of it is inserted at `UserAsserted`.
        let m = admit(
            SteerOrigin::Human,
            "stop \u{202e}gnidaer\u{202c} and \u{e0041}summarise",
            Urgency::Advisory,
        )
        .unwrap();
        for c in ['\u{202e}', '\u{202c}', '\u{e0041}'] {
            assert!(!m.text.contains(c), "{c:?} survived admission: {}", m.text);
        }
        // The marker names the codepoint rather than swallowing it — `marlowe_contract::text`'s
        // house style, asserted here so a future switch to dropping is a failing test.
        assert!(m.text.contains("<U+202E>"), "{}", m.text);
    }

    #[test]
    fn emptiness_is_judged_on_the_sanitised_text_not_the_raw_input() {
        // A steer of nothing but a zero-width character is empty to a reader and non-empty to
        // `str::is_empty`. Checking the raw input would push a blank `[steer]` block into history.
        assert_eq!(admit(SteerOrigin::Human, "   ", Urgency::Advisory), Err(SteerRefused::Empty));
        assert_eq!(admit(SteerOrigin::Human, "", Urgency::Advisory), Err(SteerRefused::Empty));
    }

    #[test]
    fn an_oversized_steer_is_refused_by_name_and_never_truncated() {
        let long = "word ".repeat(MAX_STEER_CHARS);
        let e = admit(SteerOrigin::Human, &long, Urgency::Advisory).unwrap_err();
        match e {
            SteerRefused::TooLong { len, max } => {
                assert_eq!(max, MAX_STEER_CHARS);
                assert!(len > MAX_STEER_CHARS);
            }
            other => panic!("expected TooLong, got {other:?}"),
        }
        // The refusal names both numbers, because "too long" without a limit is not actionable.
        assert!(e.to_string().contains(&MAX_STEER_CHARS.to_string()), "{e}");
    }

    #[test]
    fn the_cap_is_counted_in_characters_not_bytes() {
        // A multi-byte correction that fits on a screen must not be refused for fitting in more
        // bytes than characters. `len()` would refuse this at a third of the stated limit.
        let text = "é".repeat(MAX_STEER_CHARS);
        assert!(text.len() > MAX_STEER_CHARS, "premise: this is longer in bytes than in chars");
        assert!(admit(SteerOrigin::Human, &text, Urgency::Advisory).is_ok());
    }
}
