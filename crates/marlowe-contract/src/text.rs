//! **The display sanitiser: one predicate, shared by the value check and every render site.**
//!
//! # Why this is in *this* crate, which claims to hold nothing but section 4
//!
//! It is here because it is the only floor both sides can stand on. The predicate was born in
//! `marlowe-loop`'s [`FieldSpec::validate_value`], where it decides whether a *contract value* is
//! acceptable; it is needed again in `marlowe` and `marlowe-surface`, which sit far above the loop
//! and cannot depend on it without inverting the dependency graph. `marlowe-contract` is the
//! deepest crate in the workspace — serde, serde_json, thiserror and nothing Marlowe-shaped — so a
//! definition placed here is reachable from every render site without adding a single edge that
//! points the wrong way.
//!
//! The alternative is two definitions, and the whole reason the loop's version carries the comment
//! *"one predicate, so the render side and the validate side cannot come to disagree about what a
//! character is"* is that two definitions is the defect. A checker that refuses `U+202E` beside a
//! renderer that prints it is not a partial defence; it is a defence that reports success.
//!
//! # The gap this closes, found by the M2 security audit
//!
//! `is_renderable` had **exactly one caller in the entire product** — the contract value check —
//! and **no render site was it**. Every §B9 approval prompt therefore printed the model's composed
//! `bash` command unfiltered, so `\u{1b}[2K\r` in that command overwrites the very line the human
//! is reading in order to decide. The decision surface was forgeable by the thing being decided on.
//!
//! The TUI escaped this **incidentally**: ratatui filters control characters on its way into a
//! `Buffer`. Incidental is the operative word — nothing in this repository asserted it, named it,
//! or would notice it going away. See `marlowe-surface/tests/display_sanitiser.rs`, which is
//! labelled a characterisation test of a dependency rather than a guard, because that is what it is.
//!
//! **AMENDED BY ADR-047, 2026-08-22, and the amendment is a measurement.** ratatui's filtering was
//! measured rather than assumed: it discards `ESC`, C0/C1, `TAB`, U+202E, U+200B, U+FEFF **and**
//! U+2028 — more than this paragraph implied. What it passes through is the **tag block
//! U+E0000–U+E007F**, which [`is_renderable`] refuses by name, so the two layers are complementary
//! rather than redundant.
//!
//! The TUI therefore has **two** callers of this module now, both on model-composed text: model
//! prose, before the markdown parser sees it (`marlowe-surface/src/chrome.rs`), and a §B6 tool
//! line's target and detail (`marlowe-surface/src/render.rs`). `Entry::User` and harness notices
//! are still unsanitised, deliberately — the user is not smuggling instructions past themselves,
//! and a notice is a closed vocabulary the harness authored.
//!
//! # What a sanitised string promises, and what it does not
//!
//! **Promise:** no character that survives can move the cursor, repaint the line, reverse the
//! visual order of what follows, or occupy zero columns while carrying bytes.
//!
//! **Not promised: unforgeability.** A hostile string may contain the literal text `<U+001B>` and
//! there is no way to tell it from a marker this module wrote. That asymmetry is deliberate and it
//! is the safe direction: the forgeable claim is *"something was stripped here"*, whose worst case
//! is a human looking harder at a command that was in fact clean. The dangerous direction — a real
//! `ESC` displayed as nothing — is closed. Do not read a marker as provenance.

use std::borrow::Cow;

/// Whether a character may appear in a contract value **or on a screen**.
///
/// # Audit finding C2 — the old check was C0/C1/DEL and nothing else
///
/// That leaves three families through, and each one defeats a specific defence:
///
/// * **U+2028 / U+2029** (LINE and PARAGRAPH SEPARATOR). Mandatory line breaks that
///   `str::lines()` does **not** split on — so `CondensedResult::render` cannot indent what
///   follows one, and the whole column-0 forgery defence is bypassed by a character it never
///   sees. Worse, two consumers in this repo disagree: the memory tokenizer *does* treat them as
///   line breaks, so the same bytes are one line here and two lines there.
/// * **U+202A–U+202E, U+2066–U+2069** (BiDi embeddings and overrides — Trojan Source). The
///   two-space indent that is the entire forgery defence is a **visual** property, and an RLO can
///   move it. What is rendered indented can be displayed as though it were not.
/// * **U+200B–U+200D, U+2060, U+FEFF** (zero-width). `evil<ZWSP>.example` displays as
///   `evil.example` and tokenizes as one term, while every `assert!(!contains("evil.example"))`
///   in the suite reads clean. A test that cannot see the string it is looking for is not a test.
///
/// `Cf` covers the BiDi and zero-width families by category rather than by a list that would need
/// extending with each Unicode revision; `Zl`/`Zp` are the two separators.
///
/// **`\n` and `\t` are refused here and permitted by the caller**, because whether a newline is
/// prose or an attack depends entirely on where the string is going. See [`Shape`].
pub fn is_renderable(c: char) -> bool {
    let cp = c as u32;
    if cp < 0x20 || cp == 0x7F || (0x80..=0x9F).contains(&cp) {
        return false;
    }
    // Zl and Zp. Rust has no category API in std, and these are the entire membership of both.
    if c == '\u{2028}' || c == '\u{2029}' {
        return false;
    }
    // Cf — format characters. Enumerated by range rather than pulled in as a Unicode-tables
    // dependency: these are the blocks that reach text in practice, and the interesting ones
    // (BiDi, zero-width, the deprecated tag block used for smuggling) are all here.
    !matches!(cp,
        0x00AD                    // SOFT HYPHEN
        | 0x0600..=0x0605 | 0x061C | 0x06DD | 0x070F
        | 0x08E2 | 0x110BD | 0x110CD
        | 0x180E
        | 0x200B..=0x200F         // zero-width space/non-joiner/joiner, LRM, RLM
        | 0x202A..=0x202E         // BiDi embedding and OVERRIDE — Trojan Source
        | 0x2060..=0x2064 | 0x2066..=0x206F  // word joiner, invisible ops, BiDi isolates
        | 0xFEFF                  // ZERO WIDTH NO-BREAK SPACE / BOM
        | 0xFFF9..=0xFFFB         // interlinear annotation
        | 0x13430..=0x1343F       // Egyptian format controls
        | 0x1BCA0..=0x1BCA3
        | 0x1D173..=0x1D17A       // musical beam/slur controls
        | 0xE0000..=0xE007F       // TAG characters — the classic invisible-instruction channel
    )
}

/// What the destination does with the two whitespace characters that carry meaning in prose.
///
/// This is the same distinction `FieldType::{Text, Line}` draws on the validate side, and it is
/// drawn again here rather than shared, because the two crates answer different questions with it:
/// there, whether to *refuse*; here, whether to *mark*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// Multi-line prose — a model's reply, an error detail. `\n` and `\t` pass through.
    Prose,
    /// **One line, and the line is a decision surface.** `\n` and `\t` are marked like any other
    /// refused character.
    ///
    /// This is the shape that matters. An approval prompt states a verb and a scope on fixed
    /// lines; a scope containing `\n  approve? [y/N] y` writes a second, fraudulent prompt below
    /// the real one, and a human answering the wrong prompt has approved something they never saw.
    /// A newline is not a formatting nuisance here, it is the attack.
    Line,
}

/// Replace every character the destination cannot safely display with a visible marker.
///
/// Returns [`Cow::Borrowed`] when nothing needed replacing, which is the overwhelmingly common
/// case — this sits on the render path of every event.
///
/// The marker names the codepoint rather than swallowing it: `<U+001B>`. The precedent is
/// deliberate — `ContractViolation::DisallowedCharacter` already reports `U+{codepoint:04X}` and
/// never the character, for the same reason. A stripped payload and a clean string **must not be
/// indistinguishable to the human approving a `bash` command**; silently dropping the escape would
/// make them exactly that.
pub fn sanitize(s: &str, shape: Shape) -> Cow<'_, str> {
    let permitted = |c: char| match c {
        '\n' | '\t' => shape == Shape::Prose,
        _ => is_renderable(c),
    };
    if s.chars().all(permitted) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if permitted(c) {
            out.push(c);
        } else {
            out.push_str(&format!("<U+{:04X}>", c as u32));
        }
    }
    Cow::Owned(out)
}

/// [`sanitize`] for a single line — the approval-prompt shape. See [`Shape::Line`].
pub fn sanitize_line(s: &str) -> Cow<'_, str> {
    sanitize(s, Shape::Line)
}

/// [`sanitize`] for multi-line prose. See [`Shape::Prose`].
pub fn sanitize_prose(s: &str) -> Cow<'_, str> {
    sanitize(s, Shape::Prose)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_string_is_borrowed_not_rebuilt() {
        // Not a micro-optimization check: this is the assertion that the common path does not
        // allocate on every rendered event.
        assert!(matches!(sanitize("ordinary text", Shape::Line), Cow::Borrowed(_)));
        assert!(matches!(sanitize("two\nlines", Shape::Prose), Cow::Borrowed(_)));
    }

    #[test]
    fn the_escape_that_overwrites_the_approval_line_is_marked_not_dropped() {
        // The exact payload from the audit finding: erase-line + carriage-return, which repaints
        // the line the human is deciding on.
        let hostile = "rm -rf /\u{1b}[2K\rls";
        let out = sanitize_line(hostile);
        assert!(!out.contains('\u{1b}'), "the escape survived: {out:?}");
        assert!(!out.contains('\r'), "the carriage return survived: {out:?}");
        assert!(out.contains("<U+001B>"), "silently dropped instead of marked: {out:?}");
        assert!(out.contains("<U+000D>"), "silently dropped instead of marked: {out:?}");
        // And the command itself is still legible, which is the point of marking rather than
        // truncating: the human must still be able to read what they are approving.
        assert!(out.starts_with("rm -rf /"), "{out:?}");
    }

    #[test]
    fn a_newline_is_an_attack_on_a_line_and_prose_on_prose() {
        // The same bytes, two destinations, two correct answers.
        let forged = "/etc\n  approve? [y/N] y";
        assert!(sanitize(forged, Shape::Line).contains("<U+000A>"));
        assert!(!sanitize(forged, Shape::Line).contains('\n'));
        assert_eq!(sanitize(forged, Shape::Prose), forged);
    }

    #[test]
    fn the_three_invisible_families_are_all_marked() {
        for (c, why) in [
            ('\u{202E}', "RIGHT-TO-LEFT OVERRIDE — Trojan Source"),
            ('\u{200B}', "ZERO WIDTH SPACE — hides a hostile host inside a legible one"),
            ('\u{2028}', "LINE SEPARATOR — a break str::lines() does not see"),
            ('\u{FEFF}', "BOM as a zero-width joiner"),
            ('\u{E0041}', "TAG character — the invisible-instruction channel"),
        ] {
            // Bound first: `sanitize` returns a `Cow` borrowing its input, so a temporary here
            // would be freed while the result still points into it.
            let input = format!("a{c}b");
            let out = sanitize(&input, Shape::Line);
            assert!(!out.contains(c), "{why}: survived as {out:?}");
            assert_eq!(out, format!("a<U+{:04X}>b", c as u32), "{why}");
        }
    }

    #[test]
    fn marking_is_not_provenance_and_the_doc_says_so() {
        // A hostile string can forge the marker. Asserted rather than left implicit, so nobody
        // later builds a check on top of "this string contains a marker, therefore we wrote it".
        let forged = "<U+001B>";
        assert_eq!(sanitize(forged, Shape::Line), forged, "the marker is plain text and passes");
    }
}
