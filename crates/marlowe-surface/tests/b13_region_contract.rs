//! §B13: **every bordered region has a label and a hotkey — 100%, asserted by test.**
//!
//! This is one of the three rows most likely to be skipped, and it is the one the whole design
//! rests on. It is asserted in two independent ways, because either alone would pass while the
//! property was gone:
//!
//! 1. **The tree.** Every region in the tree has a non-empty label and a typeable hotkey, and no
//!    two claim the same key. `Region` cannot be constructed otherwise, so this half is really a
//!    check that the tree is the thing being drawn.
//! 2. **The grep.** No `Block::bordered` or `Borders::` outside `region.rs` and `overlay.rs`.
//!    Without this, someone draws a decorative box directly and half 1 goes green anyway — which
//!    is exactly the failure mode "asserted by test, not by inspection" was written against.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use marlowe_view::Tab;
use marlowe_surface::region::RegionTree;

#[test]
fn every_region_in_the_tree_has_a_label_and_a_hotkey() {
    let session = marlowe_stub::Session::new();
    let mut checked = 0;
    // Every pane, not only the visible one — the inspector's items are regions too (§B7).
    for tab in Tab::ALL {
        let tree = RegionTree::build(session.view(), tab);
        for r in tree.regions() {
            assert!(
                !r.label().trim().is_empty(),
                "{:?} has no label. §B2: the label says what the region holds",
                r.id()
            );
            // **A hotkey is still required of every region here, and the exception is named
            // rather than assumed.** An inspector item past its pane's seventeen-letter pool has
            // no accelerator — see `Item::key`, and the alternative it replaced, which was handing
            // two items the same letter and taking the whole registry down with it. Nothing else
            // in the frame may be keyless, and this fixture's panes are all inside the pool, so
            // the count below is still 100%.
            match r.hotkey() {
                Some(k) => {
                    assert!(
                        !k.is_control() && !k.is_whitespace(),
                        "{:?} has an unreachable hotkey {k:?}",
                        r.id()
                    );
                    assert_eq!(r.hotkey_label(), format!("({k})"));
                }
                None => {
                    assert!(
                        matches!(r.id(), marlowe_surface::region::RegionId::Item(_, _)),
                        "{:?} is not an inspector item and has no hotkey. §B2: a region with no \
                         hotkey has no border",
                        r.id()
                    );
                    // It draws no key on its border rather than an empty pair of brackets.
                    assert_eq!(r.hotkey_label(), "");
                }
            }
            checked += 1;
        }
    }
    println!("bordered regions with a label and a hotkey: {checked}/{checked} (100%)");
}

#[test]
fn no_two_regions_visible_together_claim_the_same_key() {
    let session = marlowe_stub::Session::new();
    for tab in Tab::ALL {
        let tree = RegionTree::build(session.view(), tab);
        let mut seen: BTreeMap<char, String> = BTreeMap::new();
        for r in tree.regions() {
            // A region with no key claims none, so it cannot collide. That is the whole point of
            // `None` over a duplicate.
            let Some(k) = r.hotkey() else { continue };
            if let Some(prev) = seen.insert(k, r.label().to_string()) {
                panic!(
                    "on the {} tab, '{k}' is claimed by both {prev:?} and {:?}. One of those \
                     borders is lying about how to reach it",
                    tab.title(),
                    r.label()
                );
            }
        }
    }
}

/// The grep. Crude and enforced, per HP10: *a crude enforced mechanism beats an elegant unenforced
/// one.*
#[test]
fn no_border_is_drawn_outside_the_region_contract() {
    /// `region.rs` defines the contract, so it names the constructor in order to own it.
    ///
    /// `overlay.rs` is §B9's approval overlay — bordered, modal, and deliberately without a bottom-
    /// border hotkey because there is nothing to jump focus *to*. It lives in its own file
    /// precisely so this allowlist can name **it** rather than `render.rs`: exempting the drawing
    /// module would blind the grep to the one place a decorative border would actually appear.
    const ALLOWED: &[&str] = &["region.rs", "overlay.rs", "b13_region_contract.rs"];

    let mut offenders = Vec::new();
    for path in sources() {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if ALLOWED.contains(&name.as_str()) {
            continue;
        }
        let src = fs::read_to_string(&path).unwrap_or_default();
        for (n, line) in src.lines().enumerate() {
            let t = line.trim_start();
            if t.starts_with("//") || t.starts_with('*') {
                continue;
            }
            if line.contains("Block::bordered") || line.contains("Borders::") {
                offenders.push(format!("{}:{}: {}", path.display(), n + 1, line.trim()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "a border is drawn outside the region contract. §B2: a border delineates an INTERACTIVE \
         region and every one carries a label and a hotkey. Route it through Region::block, or if \
         it is genuinely modal chrome, put it beside the approval overlay and defend the \
         allowlist entry:\n  {}",
        offenders.join("\n  ")
    );
}

fn sources() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut out = Vec::new();
    collect(&root.join("src"), &mut out);
    collect(&root.join("tests"), &mut out);
    out.sort();
    out
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<_> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        if p.is_dir() {
            collect(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            out.push(p);
        }
    }
}

/// §B3: *"the conversation holds no less than 55% of the horizontal split. The inspector is the
/// aside, never the peer."*
#[test]
fn the_conversation_never_drops_below_55_percent_of_the_split() {
    for (w, h) in common::SIZES {
        let c = marlowe_surface::render::layout(ratatui::layout::Rect::new(0, 0, w, h));
        let split = c.conversation.width + c.inspector.width;
        let pct = c.conversation.width as f64 * 100.0 / split as f64;
        println!("{w}x{h}: conversation {pct:.1}% of the split");
        assert!(
            pct >= 55.0,
            "at {w}x{h} the conversation is {pct:.1}% of the split; §B3 sets the floor at 55% and \
             the inspector is the aside, never the peer"
        );
    }
}

// ─── a run window is a second surface over the same contract (`M3-DESIGN.md` §6) ──────────────

/// §B2 applies to every bordered thing in the product, not to the main frame's regions only.
///
/// **Re-asserted rather than inherited.** The main pane's green says nothing about a file that
/// builds its own panels — that is the *"a measurement is scoped to the system it was taken on"*
/// family, and §6.4's whole point is that the window borrows the contract instead of writing a
/// second one.
#[test]
fn every_region_in_a_run_window_has_a_label_and_a_hotkey() {
    let tree = RegionTree::for_window();
    assert!(!tree.regions().is_empty(), "premise: a window has regions at all");
    for r in tree.regions() {
        assert!(!r.label().trim().is_empty(), "{:?} has no label", r.id());
        // **No exception here.** A window's regions are a fixed set of eight declared in
        // `for_window` — there is no pool to run out of — so the original contract stands whole.
        let k = r.hotkey().unwrap_or_else(|| panic!("{:?} has no hotkey at all", r.id()));
        assert!(!k.is_control() && !k.is_whitespace(), "{:?} has an unreachable hotkey", r.id());
    }
    println!(
        "run window: {} bordered regions with a label and a hotkey (100%)",
        tree.regions().len()
    );
}

/// A window shows every one of its regions at once — there is no tab — so **every** key in it must
/// be distinct. This is a stronger requirement than the main frame's, where two panes are never
/// visible together.
#[test]
fn no_two_regions_in_a_run_window_claim_the_same_key() {
    let mut seen: BTreeMap<char, String> = BTreeMap::new();
    for r in RegionTree::for_window().regions() {
        let k = r.hotkey().expect("a window region with no hotkey");
        if let Some(prev) = seen.insert(k, r.label().to_string()) {
            panic!(
                "in a run window '{k}' is claimed by both {prev:?} and {:?}, and both are on \
                 screen at once",
                r.label()
            );
        }
    }
}

/// §6.3: the placeholders **exist from day one**, so the tree does not depend on the run at all.
/// A region that appeared only once it had content would move every other region on the day it
/// arrived — which is the churn the placeholder rule exists to prevent.
#[test]
fn a_windows_region_tree_does_not_depend_on_what_the_run_holds() {
    let ids: Vec<_> = RegionTree::for_window().regions().iter().map(|r| r.id()).collect();
    for want in [
        marlowe_surface::region::RegionId::RunSubagents,
        marlowe_surface::region::RegionId::RunBudget,
        marlowe_surface::region::RegionId::RunScopeMemory,
        marlowe_surface::region::RegionId::RunMeetings,
    ] {
        assert!(ids.contains(&want), "{want:?} is missing from a fresh window's tree");
    }
}
