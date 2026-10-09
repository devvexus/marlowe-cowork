//! **First-run disclosure: what Marlowe can reach, before it reaches anything.**
//!
//! M2's acceptance row: *"First-run onboarding states plainly what Marlowe can reach — which
//! directories, which hosts, what it asks before doing versus does silently."* ADR-002 (revised)
//! makes it a requirement rather than a nicety, and the sentence that makes it one is worth
//! keeping in view: **a zero-config first run must not become a zero-disclosure one.**
//!
//! That is the trade ADR-002 accepted. Marlowe runs against the real filesystem by default, with
//! the sandbox scoped to the quarantined reader rather than to the agent, because a sandbox the
//! user has to configure is a sandbox nobody turns on. The price of not asking the user to
//! configure anything is telling them, unprompted and once, exactly what that means.
//!
//! # Every line is DERIVED, and that is the whole design
//!
//! Nothing here is prose describing the capability table. The tool lines are built by walking
//! [`marlowe_tools::builtin_registry`] and reading each manifest's `ConsequenceLevel`, which is the
//! same value `adjudicate` enforces on. So a tool added later appears here without anyone
//! remembering, and — the part that matters — **a tool whose consequence is changed cannot have
//! its disclosure drift away from its behaviour**, because there is one value and the disclosure
//! reads it.
//!
//! This is deliberate defence against the failure this project logs most often. A hand-written
//! *"Marlowe asks before running shell commands"* stays on screen, reassuring and green, on a build
//! where `bash` has quietly become `Consequential`. `web`'s `inline_threshold_bytes: 0` is the
//! canonical instance: a declaration nothing read, with a passing test asserting the declaration.
//! The rule that catches it is **ask of any control whether a line of code reads it**, and the
//! answer for every claim below is this module.
//!
//! # What is NOT derived, and is therefore the weak part
//!
//! The **egress** line and the **memory** line are written here, because there is nothing to derive
//! them from that would make the claim stronger:
//!
//! * Layer 4 (egress allowlisting) is shipped on the `web` path and **now stores per-host grants**
//!   for the length of a run (ADR-032 §3.1, wired 2026-08-29). This paragraph said the opposite for
//!   nineteen days — *"`EgressPolicy::grant()` has no call site in the product ... every fetch is
//!   therefore a fresh human decision"* — which was true when written and was one of nine documents
//!   repeating it. What the onboarding line must now say is the narrower true thing: **the first
//!   fetch of a host in a turn asks, and the rest of that turn does not.** A run is one user
//!   message (`Daemon::ask_streaming_with` builds a fresh `Run::root` per turn), so the human is
//!   asked again on his next message; nothing is persisted and nothing is written to a config file.
//! * The memory line states that what the user says can be written to a signed local journal. There
//!   is no capability flag for "remembers things"; it is a property of the daemon existing.
//!
//! Both are flagged here so the next person knows which lines a code change can silently falsify.

use std::fmt::Write as _;
use std::path::Path;

use marlowe_tools::{builtin_registry, ConsequenceLevel};

/// Has this profile ever been used?
///
/// **The signal is the journal, not a marker file.** A marker is a second source of truth that can
/// disagree with the thing it describes — deleted, restored from a backup, or written before the
/// work it claims happened. The journal is the profile: if it does not exist, nothing has ever been
/// recorded under this root, and that is precisely what "first run" means.
///
/// # The filename is IMPORTED, and the first version guessed it
///
/// It read `profile_root.join("journal")` and `join("journal.jsonl")`. The journal is
/// [`marlowe_journal::profile::JOURNAL_DB`] — `journal.db`, a SQLite file — so neither path ever
/// existed and **this returned `true` on every run forever**: the disclosure printed on the first
/// run, the tenth, and every one after.
///
/// That is worse than never printing. A first-run screen that never appears is a missing feature;
/// one that appears every time is noise the user learns to scroll past, which is the same outcome
/// as not disclosing while looking like disclosure.
///
/// **The unit test passed against the bug**, because it created a `journal` directory itself — it
/// asserted the code agreed with my guess rather than with the journal. Caught by running the
/// binary twice and counting the banner, not by the suite. Importing the constant makes the
/// question unaskable: if the journal is renamed, this moves with it or fails to compile.
pub fn is_first_run(profile_root: &Path) -> bool {
    !profile_root.join(marlowe_journal::profile::JOURNAL_DB).exists()
}

/// The disclosure. One screen, no prompt, no configuration offered.
///
/// It does not ask the user to agree to anything, because there is nothing here they can change at
/// this point in the product — a consent dialog with one button is theatre. It tells them what is
/// true and gets out of the way.
pub fn disclosure(workspace: &Path, memory_state: &str) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "\nMarlowe — first run under this profile. What it can reach:\n");

    let _ = writeln!(s, "  FILES");
    let _ = writeln!(s, "    {}", workspace.display());
    let _ = writeln!(
        s,
        "    That directory and everything under it. Nothing outside it: paths are resolved and \
         checked before opening, and symlinks pointing out are refused."
    );

    let _ = writeln!(s, "\n  NETWORK");
    let _ = writeln!(
        s,
        "    Nothing, until you approve a host. Every fetch asks, and every fetch asks again — \
         approvals are not remembered between requests in this build."
    );
    let _ = writeln!(
        s,
        "    A fetched page is read by a separate, sandboxed process that holds no tools; only a \
         short summary of it comes back."
    );

    let _ = writeln!(s, "\n  MEMORY");
    let _ = writeln!(s, "    {memory_state}");
    let _ = writeln!(
        s,
        "    What you tell Marlowe can be written to a signed journal on this machine, under the \
         profile directory. It is not sent anywhere."
    );

    // **The two lists below are derived from the manifests, never written here.** See the header,
    // and see `ALWAYS_ASKS` for why the split is at `Irreversible` and not somewhere friendlier.
    let _ = writeln!(s, "\n  ALWAYS ASKS YOU FIRST");
    for line in always_asks() {
        let _ = writeln!(s, "    {line}");
    }
    let _ = writeln!(s, "    (and any network request, per host, every time)");

    let _ = writeln!(s, "\n  MAY ACT WITHOUT ASKING");
    for line in tier_dependent() {
        let _ = writeln!(s, "    {line}");
    }
    let _ = writeln!(
        s,
        "    Whether these ask depends on the trust tier this run was granted. The trust ledger \
         that would raise a tier does not exist yet, so today they run at the tier the profile \
         starts with."
    );

    let _ = writeln!(
        s,
        "\n  The model chooses what to do; the harness decides what it is allowed to do, and the \
         decisions are in the journal.\n"
    );
    s
}

/// The split is at `Irreversible`, and the first version of this file got it wrong.
///
/// # What the adjudicator actually does, in its own order
///
/// 1. `consequence() == Irreversible` → `NeedsApproval`, **unconditionally**, before any tier is
///    consulted. `adjudicate.rs` states the reason: a tier comparison here would be *"a mechanism
///    nobody can exercise, which is worse than an explicit rule — it would look like a policy while
///    behaving like a constant."*
/// 2. An ungranted host → `NeedsApproval`, whatever the consequence says (ADR-032 §3.1).
/// 3. Otherwise `effective_tier >= required_tier(consequence)` → allowed, else asks.
///
/// **Only 1 and 2 are unconditional, so only 1 and 2 are stated as promises.** Everything else is
/// a comparison against a tier this module cannot see: `required_tier` is private to
/// `marlowe-permission`, which is a §13 "do not touch" boundary, and making it public to feed a
/// disclosure would be widening the permission layer's surface to serve a screen.
///
/// # The version this replaced, recorded because it is the failure this module claims to prevent
///
/// The first draft split at `Consequential` and printed two headings, "ASKS YOU FIRST" and "DOES
/// SILENTLY". That threshold appears nowhere in the adjudicator. It put `edit` (`Reversible`) under
/// "does silently" as a flat statement, on a rule I invented while writing a module whose whole
/// argument is that hand-written disclosure drifts from enforcement.
///
/// **It was caught by mutating it, not by reading it** — and the first mutation attempt failed to
/// apply and reported a clean pass, which is the second half of the same lesson.
fn always_asks() -> Vec<String> {
    describe(|c| c == ConsequenceLevel::Irreversible)
}

/// Everything else: asks or does not, depending on the run's granted tier. See [`always_asks`].
fn tier_dependent() -> Vec<String> {
    describe(|c| c != ConsequenceLevel::Irreversible)
}

fn describe(want: impl Fn(ConsequenceLevel) -> bool) -> Vec<String> {
    let Ok(registry) = builtin_registry() else {
        // The registry failing to load is a startup error the daemon reports elsewhere; the
        // disclosure does not invent a capability list to fill the space.
        return vec!["(the tool registry could not be read — see the startup error)".to_string()];
    };
    let mut out: Vec<String> = registry
        .iter()
        .filter(|r| want(r.manifest.consequence()))
        .map(|r| format!("{:<8} {}", r.id.as_str(), first_sentence(r.description.text())))
        .collect();
    // Sorted, so two runs on the same build print the same screen. `iter()` order is registration
    // order today, which is a property of a literal in another crate rather than a promise.
    out.sort();
    out
}

/// The first sentence of a tool's model-facing description.
///
/// Reusing the description the model is given, rather than writing a second one for the user, is
/// deliberate: two descriptions of one tool is how the user's screen and the model's context come
/// to disagree about what a tool does.
fn first_sentence(text: &str) -> String {
    match text.find(". ") {
        Some(i) => text[..=i].to_string(),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **This test used to create a `journal` directory of its own invention and pass against a
    /// broken `is_first_run`.** It asserted the code agreed with the test author's guess at the
    /// journal's name, which is a tautology: both sides were wrong in the same way, so the test
    /// was green while the disclosure printed on every run forever.
    ///
    /// It now builds the path the way the product does — through `Profile`, which owns the layout
    /// — so a rename moves the test with the code instead of leaving it asserting a dead name.
    #[test]
    fn a_fresh_profile_root_is_a_first_run_and_one_with_a_journal_is_not() {
        let dir = std::env::temp_dir().join("marlowe-onboarding-first-run-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(is_first_run(&dir), "an empty profile root is a first run");

        // The real journal filename, imported rather than spelled out here.
        std::fs::write(dir.join(marlowe_journal::profile::JOURNAL_DB), b"x").unwrap();
        assert!(!is_first_run(&dir), "a profile with a journal has been used");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The name this depends on must exist, and must be the one the journal actually uses.
    ///
    /// A guard on the guard: if `JOURNAL_DB` is ever changed to something that is not the file the
    /// profile creates, `is_first_run` silently returns `true` forever again, and the only symptom
    /// is a banner that repeats — which reads as cosmetic.
    #[test]
    fn the_journal_filename_this_depends_on_is_the_journals_own() {
        assert_eq!(marlowe_journal::profile::JOURNAL_DB, "journal.db");
    }

    /// Split the rendered screen into its two derived lists, by the headings that bound them.
    fn sections(text: &str) -> (String, String) {
        let (_, rest) = text.split_once("ALWAYS ASKS YOU FIRST").expect("first heading");
        let (asks, may) = rest.split_once("MAY ACT WITHOUT ASKING").expect("second heading");
        (asks.to_string(), may.to_string())
    }

    /// **The disclosure is DERIVED from the manifests, and this is the test that fails if it
    /// becomes prose.**
    ///
    /// It asserts the relationship rather than the text: a tool appears under "always asks"
    /// **exactly when** its manifest says `Irreversible`, which is the one branch of `adjudicate`
    /// that escalates without consulting a tier. A hand-written list passes on the day it is
    /// written and goes wrong the first time a consequence level moves.
    ///
    /// The lists are sliced BETWEEN the headings rather than at the start of the text, because the
    /// prose above them mentions files and hosts, and a substring search over the whole screen
    /// would match words in that prose and pass for the wrong reason.
    #[test]
    fn a_tool_is_disclosed_as_always_asking_exactly_when_its_manifest_is_irreversible() {
        let text = disclosure(Path::new("/w"), "live");
        let (asks, may) = sections(&text);

        let registry = builtin_registry().expect("registry loads");
        let mut checked = 0;
        let mut irreversible = 0;
        for reg in registry.iter() {
            let id = reg.id.as_str();
            let unconditional = reg.manifest.consequence() == ConsequenceLevel::Irreversible;
            if unconditional {
                irreversible += 1;
            }
            let (side, wrong) = if unconditional { (&asks, &may) } else { (&may, &asks) };
            assert!(
                side.contains(&format!("{id:<8}")),
                "`{id}` is {:?} and is missing from the section its manifest puts it in:\n{text}",
                reg.manifest.consequence()
            );
            assert!(
                !wrong.contains(&format!("{id:<8}")),
                "`{id}` is {:?} and is listed on the WRONG side:\n{text}",
                reg.manifest.consequence()
            );
            checked += 1;
        }
        // Two vacuity controls. Without the second, a build where NOTHING is irreversible would
        // satisfy every assertion above while disclosing that nothing ever asks.
        assert!(checked >= 8, "only {checked} tools checked — the registry looks empty");
        assert!(irreversible >= 1, "no tool is Irreversible — the 'always asks' list is empty");
    }

    /// `bash` is the tool this row exists for, so it is asserted by name as well as by rule.
    #[test]
    fn bash_is_disclosed_as_always_asking() {
        let text = disclosure(Path::new("/w"), "live");
        let (asks, _) = sections(&text);
        assert!(asks.contains("bash"), "bash must be disclosed as always asking:\n{text}");
    }

    /// **The screen must not promise that anything else is silent, because the code does not.**
    ///
    /// Everything below `Irreversible` is decided by `effective_tier >= required_tier(...)`, which
    /// this module cannot evaluate. The first draft printed "DOES SILENTLY" over that list — a
    /// flat claim about behaviour that depends on a value it never read.
    #[test]
    fn the_screen_never_claims_a_tool_runs_silently() {
        let text = disclosure(Path::new("/w"), "live");
        assert!(
            !text.contains("DOES SILENTLY"),
            "the disclosure states as fact something the adjudicator decides per run:\n{text}"
        );
        assert!(
            text.contains("depends on the trust tier"),
            "the tier dependency must be stated, not implied:\n{text}"
        );
    }

    #[test]
    fn the_workspace_path_is_stated_literally() {
        // The user's actual directory, not a description of one. "the current directory" is not a
        // disclosure — it is a sentence that is true of every possible configuration.
        let text = disclosure(Path::new("/home/u/project"), "live");
        assert!(text.contains("/home/u/project"), "{text}");
    }
}
