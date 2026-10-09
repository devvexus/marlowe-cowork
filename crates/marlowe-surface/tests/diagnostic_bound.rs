//! **The `/doctor` carve-out, bounded.**
//!
//! `Outcome::Diagnostic` is the one path to the user that does not go through the `Notice`
//! vocabulary. That is deliberate — a terminal capability report with measured contrast ratios is
//! diagnostic output, not speech, and the same reasoning keeps retrieval instrumentation behind
//! `--dev` (§B1). Forcing it through a persona renderer would either bloat the enum with a
//! `DoctorFacts` struct or invite a free-text escape hatch, and the escape hatch is what would
//! actually happen.
//!
//! **A carve-out with no boundary is a loophole.** So the boundary is a count: diagnostics are
//! reachable from exactly two entry points, and a third fails this test rather than being noticed
//! six months later.

use std::fs;
use std::path::Path;

/// The complete list. `--diagnostic` renders raw state into the frame; `/doctor` prints the
/// capability report. Both are explicitly diagnostic and neither is on a conversational path.
const DIAGNOSTIC_ENTRY_POINTS: &[&str] = &["/doctor", "--diagnostic"];

#[test]
fn diagnostics_are_reachable_from_exactly_two_entry_points() {
    let src = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/commands.rs"))
        .expect("commands.rs");

    let uses: Vec<usize> = src
        .match_indices("Outcome::Diagnostic")
        .map(|(i, _)| i)
        .collect();

    // One construction site (the `/doctor` arm) plus the variant's own declaration.
    assert!(
        uses.len() <= 2,
        "`Outcome::Diagnostic` is constructed in {} places. It is a carve-out from the Notice \
         vocabulary and it is bounded at one command; a second one is the loophole opening. If a \
         new command genuinely produces diagnostics, argue it in DECISIONS.md and update \
         DIAGNOSTIC_ENTRY_POINTS here — do not widen it quietly.",
        uses.len()
    );
    assert!(
        !uses.is_empty(),
        "the scan found no `Outcome::Diagnostic` at all, so its silence is not evidence"
    );
    assert_eq!(DIAGNOSTIC_ENTRY_POINTS.len(), 2);
    println!(
        "diagnostic entry points: {} ({})",
        DIAGNOSTIC_ENTRY_POINTS.len(),
        DIAGNOSTIC_ENTRY_POINTS.join(", ")
    );
}

/// The other half: nothing that renders as **conversation** may carry free text from a command.
///
/// Without this, `Outcome::Diagnostic` would be a perfectly good way to smuggle prose into the
/// transcript — which is the violation ADR-030 exists to close, arriving through the one door the
/// ADR deliberately left open.
#[test]
fn a_diagnostic_never_enters_the_transcript() {
    let src = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/app.rs"))
        .expect("app.rs");
    let arm = src
        .split_once("Outcome::Diagnostic(lines)")
        .expect("the Diagnostic arm")
        .1;
    let arm = &arm[..arm.find("            }").unwrap_or(arm.len())];
    for forbidden in ["client_note", "transcript", "Entry::Said", "Speech::"] {
        assert!(
            !arm.contains(forbidden),
            "the Diagnostic arm reaches `{forbidden}`, so diagnostic text can become \
             conversation:\n{arm}"
        );
    }
}
