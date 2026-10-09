//! **The model is told what is in its workspace, without spending a call to find out.**
//!
//! `SourceKind::ProjectFiles` has existed since the assembler did. It has a tier, a 15% budget, a
//! trimmability rule and a reported line — and a grep for the variant outside `context.rs` returned
//! **nothing at all**. The slot was declared, budgeted, and permanently empty.
//!
//! Measured live 2026-08-27. Asked to write `session-handoff.md`, the model tried
//! `scratchpad/session-handoff.md` (refused), `bash dir` (declined — no approval surface),
//! `scratchpad/wsC/session-handoff.md` (refused), then the right path. **Three wasted calls and
//! 190 seconds**, because it had been given the workspace's absolute path and nothing about its
//! contents, so it invented prefixes out of the path string itself.
//!
//! A tool would answer this too and a tool is the wrong shape: knowing where you are is context,
//! not an action.

use std::fs;

use marlowe_daemon::workspace_map;

fn fixture(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("marlowe-wsmap-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("docs").join("design")).unwrap();
    fs::create_dir_all(root.join("target").join("debug")).unwrap();
    fs::create_dir_all(root.join(".git").join("objects")).unwrap();
    fs::write(root.join("README.md"), "hi").unwrap();
    fs::write(root.join("src").join("main.rs"), "fn main() {}").unwrap();
    fs::write(root.join("docs").join("design").join("deep.md"), "deep").unwrap();
    fs::write(root.join("target").join("debug").join("junk.o"), "junk").unwrap();
    root
}

#[test]
fn the_map_names_the_top_level_and_skips_the_noise() {
    let root = fixture("basic");
    let map = workspace_map(&root).expect("a workspace with files produces a map");

    assert!(map.contains("README.md"), "a top-level file is missing: {map}");
    assert!(map.contains("src/"), "a top-level directory is missing: {map}");
    assert!(map.contains("src/main.rs"), "depth two is missing: {map}");

    // **The skips, which are what make the budget affordable.** A workspace whose map is 90%
    // build output has told the model nothing.
    assert!(!map.contains("junk.o"), "build output reached the context tier: {map}");
    assert!(!map.contains(".git/objects"), "version control internals reached it: {map}");

    // Depth is bounded: `docs/design/` is named, its contents are not.
    assert!(map.contains("docs/design/"), "the directory should be named: {map}");
    assert!(!map.contains("deep.md"), "depth three must be left to `grep`: {map}");

    // The instruction that the 190 seconds were actually spent on.
    assert!(
        map.contains("relative") && map.contains("do NOT prefix"),
        "the map must say paths are relative to the root and not to be prefixed with it, which is \
         the exact mistake it exists to prevent: {map}"
    );
}

/// **Two runs on the same workspace describe it identically.**
///
/// `read_dir` order is a filesystem detail. If it leaked into the prompt, the stable prefix would
/// differ between runs for no reason and the provider's cache would be thrown away every turn —
/// a performance bug with no symptom anyone would trace back to a directory listing.
#[test]
fn the_map_is_the_same_twice() {
    let root = fixture("stable");
    assert_eq!(workspace_map(&root), workspace_map(&root));
}

/// An empty workspace produces no block at all, rather than a heading with nothing under it.
#[test]
fn an_empty_workspace_contributes_nothing() {
    let root = std::env::temp_dir().join(format!("marlowe-wsmap-empty-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    assert_eq!(workspace_map(&root), None);
}

/// **A truncated listing says so.** Silently stopping is worse than not listing at all: the model
/// would conclude a file is absent when it was merely past the cap.
#[test]
fn a_listing_that_hit_its_cap_says_so() {
    let root = std::env::temp_dir().join(format!("marlowe-wsmap-many-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    for i in 0..400 {
        fs::write(root.join(format!("f{i:03}.txt")), "x").unwrap();
    }
    let map = workspace_map(&root).expect("a map");
    assert!(
        map.contains("listing stopped"),
        "the cap was hit and the map did not say so, so absence is indistinguishable from \
         truncation: {}",
        &map[map.len().saturating_sub(300)..]
    );
    assert!(map.contains("`grep`"), "a truncated map must name the way to look further: {map}");
}
