//! §B13: **memory-related regions in the default surface — zero.**
//!
//! §B1 is binding and an earlier draft got it wrong. The user experiences memory the way they
//! experience it in a good colleague: they just know things. There is no pane, tab, field or
//! citation, and no default-visible indication that retrieval occurred.
//!
//! **A `recall` tool line is not a memory UI.** `⋯ recall  open commitments  3 due` is a tool line
//! like any other and is permitted. What is forbidden is a *region whose subject is the memory
//! system* — so this test checks region labels and hotkeys, not the transcript.
//!
//! The second test is the one that matters over time: it fails **by name** if `TurnEvent` grows an
//! injection variant. CONTRACTS.md §13 says there must never be one; a rule that lives only in
//! prose is a rule that erodes the first time somebody wants a debug view.

mod common;

use std::fs;
use std::path::Path;

use marlowe_view::Tab;
use marlowe_surface::region::RegionTree;

/// Words that would mean a region has taken memory as its subject.
const MEMORY_VOCABULARY: &[&str] = &[
    "memor",
    "recall",
    "retriev",
    "inject",
    "provenance",
    "precision",
    "embedding",
    "belief",
    "salience",
    "consolidat",
];

#[test]
fn no_region_in_any_pane_has_memory_as_its_subject() {
    let session = marlowe_stub::Session::new();
    let mut checked = 0;
    for tab in Tab::ALL {
        for r in RegionTree::build(session.view(), tab).regions() {
            let label = r.label().to_lowercase();
            for word in MEMORY_VOCABULARY {
                assert!(
                    !label.contains(word),
                    "the region {:?} on the {} tab is named for the memory system. §B1: memory \
                     gets no special treatment in the interface, and an agent that narrates its \
                     own recall is demonstrating that it does not trust the user to notice. \
                     Retrieval instrumentation is a seventh tab under --dev only",
                    r.label(),
                    tab.title()
                );
            }
            checked += 1;
        }
    }
    println!("memory-related regions in the default surface: 0 of {checked} regions");
}

#[test]
fn no_inspector_item_reports_a_score_a_count_or_a_provenance() {
    let session = marlowe_stub::Session::new();
    for tab in Tab::ALL {
        for item in marlowe_surface::inspector::items_for(session.view(), tab) {
            let label = item.label.to_lowercase();
            for word in MEMORY_VOCABULARY {
                assert!(
                    !label.contains(word),
                    "the {} item {:?} takes memory as its subject",
                    tab.title(),
                    item.label
                );
            }
        }
    }
}

/// The standing guard. Fails **by name**, so the next person to want an injection variant reads
/// the reason instead of the diff.
#[test]
fn turn_event_has_no_injection_variant_and_must_never_gain_one() {
    // **C2d moved this guard's subject, and the guard said so.** `turn.rs` lived in
    // `marlowe-stub` until the view models were promoted to `marlowe-view`. A guard is a claim
    // about a path, so it needs to fail when the path is not there — this one does, because it
    // reads with `expect` rather than falling back to an empty string. The same check written
    // with `unwrap_or_default()` would have scanned nothing, found no memory variants, and passed
    // forever while §B1 went unguarded.
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("marlowe-view/src/turn.rs");
    let text = fs::read_to_string(&src).unwrap_or_else(|e| {
        panic!(
            "cannot read {} ({e}). This guard names a path; if TurnEvent moved again, point it \
             at the new one rather than deleting the check — §B1 is binding and an unread file \
             scans clean.",
            src.display()
        )
    });

    // Only the enum body, so the doc comment explaining the rule does not trip the rule.
    let body = text
        .split_once("pub enum TurnEvent {")
        .expect("TurnEvent enum")
        .1
        .split_once('}')
        .expect("enum body")
        .0;

    for line in body.lines() {
        let t = line.trim();
        if t.starts_with("//") {
            continue;
        }
        let lower = t.to_lowercase();
        for word in ["memor", "inject", "retriev", "recall", "provenance"] {
            assert!(
                !lower.contains(word),
                "TurnEvent gained a memory variant: {t:?}\n\n\
                 CONTRACTS.md §13: *there is no MemoryInjected variant in TurnEvent, and there \
                 must never be one.* Memory gets no representation in the interface (§B1). \
                 Diagnostics reach --dev through a separate channel that is not part of \
                 TurnEvent — build that channel instead."
            );
        }
    }
    println!("TurnEvent: 0 memory variants");
}
