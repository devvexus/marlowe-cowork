//! **`SteerMessage` is constructed in exactly one place.** ADR-054 §3.
//!
//! # Why a grep, and not a type
//!
//! `SteerMessage` is a plain struct with public fields, because the loop reads them and a getter
//! per field would be ceremony. That means Rust cannot stop a second construction site, and a
//! second construction site is precisely the failure `M3-DESIGN.md` §6.1 names: *"never a side door
//! that skips it."*
//!
//! The same shape as `b13_region_contract.rs`'s grep for `Block::bordered` outside `region.rs`, and
//! for the same reason it gives: the tree-walking half of that test *"would pass forever and catch
//! nothing, because the failure mode is not 'somebody set the label to an empty string' — it is
//! 'somebody drew a `Block::bordered()` without going through the tree at all'."*
//!
//! Here the failure mode is not "somebody wrote a bad cap". It is a window, or a second daemon
//! request, or a script, building a `SteerMessage` directly and never touching `admit` — at which
//! point the sanitiser and the cap are in one path and not the other, and the two agree until the
//! day they do not.
//!
//! # The negative control is in the test itself
//!
//! A grep that matched nothing would pass. So this asserts a **count**, and it asserts that the one
//! occurrence is where it is supposed to be — a guard that reads "zero matches" on a workspace where
//! the type was renamed is the "a guarded path that moved is unguarded" family, fourteenth instance.

use std::fs;
use std::path::{Path, PathBuf};

/// The one file permitted to construct one.
const DOOR: &str = "crates/marlowe-loop/src/steer.rs";

fn repo_root() -> PathBuf {
    // `CARGO_MANIFEST_DIR` is `crates/marlowe-loop`.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the workspace root is two levels above this crate")
        .to_path_buf()
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if p.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn steer_message_is_constructed_in_exactly_one_place_and_it_is_the_door() {
    let root = repo_root();
    let mut files = Vec::new();
    rust_files(&root.join("crates"), &mut files);
    assert!(files.len() > 50, "premise: the walk found the workspace, not an empty directory");

    let mut sites: Vec<String> = Vec::new();
    let mut door_hits = 0usize;
    for f in &files {
        let Ok(text) = fs::read_to_string(f) else { continue };
        let rel = f
            .strip_prefix(&root)
            .unwrap_or(f)
            .to_string_lossy()
            .replace('\\', "/");
        for (i, line) in text.lines().enumerate() {
            if !line.contains("SteerMessage {") {
                continue;
            }
            // **Comments are not construction sites**, and this file's own header names the literal
            // it is looking for — as does `steer.rs`'s. A guard that counted prose would have been
            // wrong on the day it was written, which is at least an honest way to find out.
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || trimmed.starts_with("*") {
                continue;
            }
            // The **definition** is not a construction. `pub struct SteerMessage {` matches the
            // same literal, which is how this guard first reported `driver.rs` — the file that
            // declares the type it is protecting.
            if trimmed.contains("struct SteerMessage {") {
                continue;
            }
            if rel == DOOR {
                door_hits += 1;
                continue;
            }
            // Tests may construct one: they are asserting on the loop's behaviour given a message,
            // not opening a path a run can reach. `#[cfg(test)]` modules and `tests/` are the two
            // shapes that is true of, and both are excluded by path rather than by guesswork about
            // what a line means.
            if rel.contains("/tests/") || in_test_module(&text, i) {
                continue;
            }
            sites.push(format!("{rel}:{}", i + 1));
        }
    }

    assert_eq!(
        door_hits, 1,
        "expected exactly one construction in {DOOR}, found {door_hits}. If the type moved or was \
         renamed, this guard is now watching a path that does not exist — see CLAUDE.md's \
         fourteenth instance"
    );
    assert!(
        sites.is_empty(),
        "ADR-054: `SteerMessage` may only be built by `marlowe_loop::steer::admit`, so the \
         sanitiser and the cap cannot be in one path and not the other. Found: {sites:?}"
    );
}

/// Whether line `i` sits inside a `#[cfg(test)] mod tests`. Crude and deliberately so: it errs
/// towards *not* excusing a line, which is the safe direction for a guard.
fn in_test_module(text: &str, line: usize) -> bool {
    text.lines()
        .take(line)
        .any(|l| l.trim_start().starts_with("#[cfg(test)]"))
}
