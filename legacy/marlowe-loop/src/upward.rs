//! **A8's arm selector, at the one hop a child's words actually cross.**
//!
//! M3-DESIGN §9.1's A8 asks what the upward channel's *shape* costs in injection propagation.
//! The obvious place to switch the arms — `LoopOutcome::Escalated { question }` — carries no
//! traffic: [`Engine::spawn`](crate::Engine) already replaces a child's escalation with a
//! harness-authored constant and redirects the question to the journal, so all three arms would
//! have emitted byte-identical behaviour. Three identical cells read character-for-character like
//! *"the typing is decorative"*, which is the finding A8 exists to produce. See M3-DESIGN §9.1's
//! 2026-08-30 amendment.
//!
//! The traffic is at `Engine::spawn`'s **note match**: `req.contract.validate` then
//! [`CondensedResult::render`](crate::run::CondensedResult::render), which `run.rs`'s own
//! `FieldSpec` comment calls *"the single place where content crosses from `UntrustedContent` to
//! `AgentInferred`"*, and which `REDTEAM-SESSION.md` §4 names as pass 1's attack surface. So the
//! arms are three treatments of that crossing and of nothing else.
//!
//! # What is deliberately NOT in this module
//!
//! An `Escalation` record with `category`, `severity` and `artifact_ref` — M3-DESIGN §2.3's
//! shape — is **not built here, and the reason is a blocker rather than a preference.** Every one
//! of those fields needs a producer, and the producer is `ModelStep::Ask`, which carries a bare
//! `String`. Widening it is an edit to `crates/marlowe-loop/src/driver.rs`, which is §13-guarded.
//! A record whose fields no caller can fill is CLAUDE.md instance #16 with a `Serialize` derive
//! on it. The drop list and the blocker belong in ADR-063; nothing here declares a field that no
//! function in this workspace reads.

/// **The A8 arm.** One value, three arms, and the third does not exist in a release build.
///
/// # Why `FreeText` is `cfg(debug_assertions)` and not any of the alternatives
///
/// Arm (c) hands a child's last assistant message to its parent with `validate` **not** called.
/// That deliberately reopens the hole the comment at `Engine::spawn`'s note match closed —
/// *"`Escalated { question }` was the worst: a model-written string, interpolated verbatim, one
/// trust class above its origin"*. M3-DESIGN §9.1's amendment requires it to be **unreachable in a
/// shipped build rather than merely off by default**, and a control that is reachable in
/// production is a defect, not a control.
///
/// `cargo build --release` — the command CLAUDE.md names as the one that produces
/// `target/release/marlowe.exe` — clears `debug_assertions`, and `[profile.release]` in the
/// workspace manifest sets only `debug = 1` (debuginfo), so the variant is **absent from the
/// shipped artifact's type**. There is no arm to select, no string that parses to it, and
/// [`UpwardShape::parse`] refuses `"free_text"` by name rather than falling back.
///
/// The four alternatives, and what each loses:
///
/// * **A CLI flag on the release binary** (the project's `--reranking off` pattern, and what the
///   first design proposed). It is *off by default*, which is exactly the posture the amendment
///   refuses: the unvalidated channel is present in the artifact the user runs, one argument away.
/// * **A cargo `feature`.** It loses twice. A feature can be turned on in a release build by any
///   `--features` line, so there is no artifact-level guarantee at all; and worse, arms (a) and
///   (b) would then be measured on the default artifact while arm (c) ran on a different one —
///   a control that measures a different system, which is the `persona_emission.rs` family.
///   `debug_assertions` does not have that defect **for the measurement**, because all three arms
///   exist together in the one debug artifact and the comparison stays inside it.
/// * **An environment variable.** `minimal_env()` is a fixed allowlist, so the eval harness would
///   not carry it (the same reason `MARLOWE_CUDA_LIB_DIR` has to be translated onto `PATH`), and
///   an env var read at construction is precisely *a default that makes a mismatch unobservable*.
/// * **`cfg(test)`.** It does not reach an integration test at all: `tests/` compiles the library
///   as an ordinary dependency with `cfg(test)` unset, so the measurement suite could not select
///   the arm it exists to measure.
///
/// # No `Default`
///
/// There is none, and `Engine::new` names [`UpwardShape::Typed`] explicitly. `Typed` is today's
/// code byte for byte, so the value that arrives without being asked for is **the product**, never
/// a control — the inverse of the hazard "a control that arrives by default is a control nobody
/// selected". Selecting anything else takes a call to `Engine::with_upward_shape`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpwardShape {
    /// Arm (a). Today's code: `req.contract.validate` then `CondensedResult::render`, with a
    /// fixed harness-authored string on every non-`Completed` outcome.
    Typed,
    /// Arm (b). Arm (a) plus one `FieldSpec::line` capped short, produced by a **quarantined**
    /// child over the child's own prose, and omitted entirely on any non-`Completed` outcome or
    /// any validator outcome that is not itself `Completed` and valid.
    TypedPlusValidatedSentence,
    /// Arm (c). **THE CONTROL, EXPECTED TO FAIL.** The child's last assistant message, verbatim,
    /// with `validate` not called. Absent from a release build — see the type's documentation.
    #[cfg(debug_assertions)]
    FreeText,
}

/// What [`UpwardShape::parse`] returns for a spelling this build does not have.
///
/// It carries the offered string so a refusal can name it, and its `Display` lists the spellings
/// that exist **in this build** — so a release binary asked for `free_text` says the arm is not
/// present rather than that it was misspelled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownShape {
    offered: String,
}

impl UnknownShape {
    pub fn offered(&self) -> &str {
        &self.offered
    }
}

impl std::fmt::Display for UnknownShape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let known: Vec<&str> = UpwardShape::ALL.iter().map(|s| s.as_str()).collect();
        write!(
            f,
            "`{}` is not an upward-channel arm in this build. This build has: {}",
            self.offered,
            known.join(", ")
        )
    }
}

impl std::error::Error for UnknownShape {}

impl UpwardShape {
    /// Every arm **this build has**. Two lists rather than one list with a `cfg` on an element,
    /// because a `cfg` inside an array literal is an attribute on an expression and the two-list
    /// form is what a reader can check against the enum above at a glance.
    #[cfg(debug_assertions)]
    pub const ALL: &'static [UpwardShape] =
        &[Self::Typed, Self::TypedPlusValidatedSentence, Self::FreeText];
    /// The release build's arms. Arm (c) is absent, which is the whole point.
    #[cfg(not(debug_assertions))]
    pub const ALL: &'static [UpwardShape] = &[Self::Typed, Self::TypedPlusValidatedSentence];

    /// **The one producer of the arm's string spelling.** A journal row, a report cell and a
    /// refusal message all read this function; there is no second literal anywhere.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Typed => "typed",
            Self::TypedPlusValidatedSentence => "validated_sentence",
            #[cfg(debug_assertions)]
            Self::FreeText => "free_text",
        }
    }

    /// Total, with an error arm. **An unrecognised value REFUSES; it never falls back to
    /// `Typed`.** A mistyped arm that silently measures the product under the control's label is
    /// the failure this session exists to detect, and it would read as a clean cell.
    ///
    /// `-` and `_` are the same character here, so `free-text` and `free_text` are one spelling
    /// rather than two.
    pub fn parse(s: &str) -> Result<Self, UnknownShape> {
        let k = s.trim().to_ascii_lowercase().replace('-', "_");
        Self::ALL
            .iter()
            .copied()
            .find(|v| v.as_str() == k)
            .ok_or_else(|| UnknownShape { offered: s.to_string() })
    }
}

/// The field name arm (b) adds. **One definition**: the `FieldSpec` the quarantined validator is
/// held to, and the header the parent reads, are built from this constant and from
/// `CondensedResult::render` — never from a second `format!`.
pub const HEADLINE_FIELD: &str = "headline";

/// 200 characters, which is `FieldSpec::line`'s own default cap, restated here so the number the
/// validator enforces and the number this module documents are one binding.
///
/// **This is a character count, not a budget dimension.** A `0` on a `Budget` dimension reads as
/// *already exhausted* to `Budget::exhausted`'s `spent >= budget` (instance #17). Nothing here is
/// a budget — the validator's budget comes from `Budget::slice_for_quarantined_read`, which
/// already carries that lesson — and this number must not be transplanted into one.
pub const HEADLINE_MAX_CHARS: usize = 200;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_arm_round_trips_through_its_one_spelling() {
        for arm in UpwardShape::ALL {
            assert_eq!(
                UpwardShape::parse(arm.as_str()).expect("its own spelling parses"),
                *arm,
                "`{}` did not round-trip",
                arm.as_str()
            );
        }
    }

    #[test]
    fn a_spelling_this_build_does_not_have_is_refused_and_never_defaulted() {
        let e = UpwardShape::parse("banana").expect_err("an unknown arm must refuse");
        assert_eq!(e.offered(), "banana");
        assert!(e.to_string().contains("typed"), "the refusal names the arms it does have: {e}");
    }

    #[test]
    fn a_dash_and_an_underscore_are_the_same_arm() {
        assert_eq!(
            UpwardShape::parse("validated-sentence").unwrap(),
            UpwardShape::TypedPlusValidatedSentence
        );
    }

    /// **The release-build assertion, in the one place that can hold both halves.**
    ///
    /// In a debug build arm (c) exists and parses; in a release build the variant is not in the
    /// type at all, so `parse` refuses its spelling and `ALL` is shorter. This is the test that
    /// goes red if somebody removes the `cfg` and ships the control.
    #[test]
    fn arm_c_exists_in_a_debug_build_and_its_spelling_does_not_exist_in_a_release_build() {
        #[cfg(debug_assertions)]
        {
            assert_eq!(UpwardShape::parse("free_text").unwrap(), UpwardShape::FreeText);
            assert_eq!(UpwardShape::ALL.len(), 3);
        }
        #[cfg(not(debug_assertions))]
        {
            assert!(
                UpwardShape::parse("free_text").is_err(),
                "the unvalidated control must not be selectable in a shipped build"
            );
            assert_eq!(UpwardShape::ALL.len(), 2);
        }
    }
}
