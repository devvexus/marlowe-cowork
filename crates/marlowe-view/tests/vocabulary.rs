//! **ADR-030's rules, as tests.** A rule that only lives in prose erodes the first time somebody
//! is in a hurry.
//!
//! Three properties, and each one is the mechanical form of a sentence in the ADR:
//!
//! 1. §5 — no `Notice` field is a free-text `String`, so a variant cannot carry a sentence.
//! 2. §6 — an unhandled `Intent` is a **build error**, not a runtime no-op.
//! 3. §C1/§C4 — every rendered variant passes the persona probe set.

use std::fs;
use std::path::{Path, PathBuf};

fn crate_src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn read(name: &str) -> String {
    let p = crate_src().join(name);
    fs::read_to_string(&p).unwrap_or_else(|e| {
        panic!(
            "cannot read {} ({e}). This guard names a path, so it fails when the path moves \
             rather than scanning nothing and passing.",
            p.display()
        )
    })
}

/// ADR-030 §5, mechanically: **a variant carries typed facts, never prose.**
///
/// The exemptions are three newtypes, and they are newtypes precisely so this test can tell them
/// apart from a `String` field: [`marlowe_view::Echo`] is text the *user* typed, quoted back
/// verbatim; `PathLabel` is a path the permission layer resolved; and `CommandLine` is a command the
/// harness assembled for the reader to copy (`M3-DESIGN.md` §6.7). **None of the three is composed
/// prose**, which is the property §5 is actually about — the count is not the rule, and a fourth
/// would be fine on the same terms.
#[test]
fn no_notice_field_is_a_free_text_string() {
    let src = read("notice.rs");
    let body = src
        .split_once("pub enum Notice {")
        .expect("the Notice enum")
        .1;
    let body = &body[..body.find("\n}").expect("enum body")];

    let mut offenders = Vec::new();
    for (n, line) in body.lines().enumerate() {
        let t = line.trim();
        if t.starts_with("//") || t.starts_with("///") {
            continue;
        }
        if t.contains("String") {
            offenders.push(format!("{}: {t}", n + 1));
        }
    }
    assert!(
        offenders.is_empty(),
        "a `Notice` variant carries a String, which means it can carry a sentence.\n\n\
         ADR-030 §5: a variant is added only when a producer must say something the existing set \
         cannot express — NEVER to carry a string the surface already has. A String field is that \
         rule being broken, and it turns this enum back into `say(String)` with extra steps.\n\n\
         If the value is text the user typed, wrap it in `Echo`. If it is a fact, give it a type.\n\
         {}",
        offenders.join("\n")
    );

    // The parse is asserted, so a reader that silently found nothing cannot report "no Strings".
    assert!(body.contains("NotBuilt"), "the enum body did not parse:\n{body}");
}

/// ADR-030 §6: **an unhandled intent is a build error.**
///
/// `Intent` is closed and every `Produce::apply` matches it exhaustively, so adding a variant fails
/// to compile in every producer. That is stronger than a registry — but it is defeated by one
/// `_ => Ok(())` arm, which is what this scans for.
///
/// A silently-dropped intent is worse than the violation it would replace: a surface emitting an
/// intent nobody handles **fails silently**, and a working build and a broken one look identical.
#[test]
fn no_produce_impl_has_a_catch_all_arm() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("crates/");
    let mut impls = 0;
    let mut offenders = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                if p.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                let src = fs::read_to_string(&p).unwrap_or_default();
                let Some(i) = src.find("fn apply(&mut self, intent") else { continue };
                impls += 1;
                let body = &src[i..];
                let end = body.find("\n    }").unwrap_or(body.len());
                for (n, line) in body[..end].lines().enumerate() {
                    let t = line.trim();
                    if t.starts_with("//") {
                        continue;
                    }
                    if t.starts_with("_ =>") || t.starts_with("_ if") {
                        offenders.push(format!("{}:{}: {t}", p.display(), n + 1));
                    }
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "a `Produce::apply` has a catch-all arm, so a new `Intent` variant would compile and be \
         silently dropped.\n\nADR-030 §6: an unhandled intent must be a BUILD error. Match every \
         variant; a producer that cannot perform one returns a named `IntentError` — refusing out \
         loud is fine, refusing in silence is not.\n{}",
        offenders.join("\n")
    );

    // **The positive control.** Without it, a walk that found nothing would report "no catch-alls"
    // for a workspace full of them — the vacuous pass this project has logged as a family.
    assert!(
        impls >= 1,
        "the scan found no `Produce::apply` implementations at all, so its silence is not evidence"
    );
    println!("Produce::apply implementations scanned: {impls}, catch-all arms: 0");
}

/// Addendum C §C1 and §C4, over **every** `Notice` variant.
///
/// The persona is not configurable and anything producing user-visible prose carries it. This is
/// the harness half of that rule made executable: `Notice::render` is the one place harness prose
/// is written, so the whole vocabulary can be enumerated and probed.
#[test]
fn every_notice_variant_passes_the_persona_probes() {
    use marlowe_view::notice::*;
    use marlowe_view::{ControlId, Tab};

    let ctx = RenderContext { commands: &[], keys: &[], control: None };
    let variants = vec![
        Notice::Refused(Refusal::UnknownCommand { name: Echo::new("foo"), nearest: Some("runs") }),
        Notice::Refused(Refusal::UnknownCommand { name: Echo::new("zzz"), nearest: None }),
        Notice::Refused(Refusal::Usage { command: "state", expects: "listening|idle" }),
        Notice::Refused(Refusal::NoSuchOption {
            control: ControlId::Autonomy,
            given: Echo::new("god-mode"),
        }),
        Notice::NotBuilt { capability: Capability::CommandPalette, arrives: Milestone::M2C3 },
        Notice::NotBuilt { capability: Capability::TrustPane, arrives: Milestone::M6 },
        Notice::PaneOpened {
            tab: Tab::Runs,
            summary: PaneSummary::Runs { running: 2, spend_cents: 120, ceiling_cents: 300 },
        },
        Notice::PaneOpened {
            tab: Tab::Schedule,
            summary: PaneSummary::Schedule { needing_you: 3, next_at: (11, 0), next_is_conflict: true },
        },
        Notice::PaneOpened {
            tab: Tab::Trust,
            summary: PaneSummary::NotBuilt { arrives: Milestone::M6 },
        },
        Notice::ApprovalResolved { disposition: Disposition::Sent },
        Notice::ApprovalResolved { disposition: Disposition::Declined },
        Notice::ApprovalResolved { disposition: Disposition::OpenedForEditing },
        Notice::Undone { turns: 1 },
        Notice::Undone { turns: 4 },
    ];

    // §C4's shape. Each probe is a phrase that would mean the persona had eroded.
    const FORBIDDEN: &[&str] = &[
        "great question", "happy to", "i'd be happy", "certainly!", "of course!",
        "let me know if", "feel free to", "i hope this helps", "absolutely",
        "sorry about that", "my apologies", "as an ai",
    ];

    for v in &variants {
        for line in v.render(&ctx) {
            let lower = line.to_lowercase();
            for probe in FORBIDDEN {
                assert!(
                    !lower.contains(probe),
                    "§C4: {v:?} rendered {line:?}, which contains {probe:?}"
                );
            }
            assert!(
                !line.chars().any(|c| c as u32 > 0x1F000),
                "§C1: no emoji. {v:?} rendered {line:?}"
            );
            assert!(!line.trim().is_empty(), "{v:?} rendered an empty line");
            // §C1: the first sentence carries the answer. A line opening with a hedge does not.
            for hedge in ["well,", "so,", "actually,", "just "] {
                assert!(
                    !lower.starts_with(hedge),
                    "§C1: the first sentence must carry the answer. {v:?} opens with {hedge:?}"
                );
            }
        }
    }
    println!("notice variants probed: {} (0 §C1/§C4 violations)", variants.len());
}
