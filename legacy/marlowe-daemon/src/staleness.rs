//! **Is the running binary older than the source it was built from?**
//!
//! # This file reads no clock, and that is why it is exempt rather than fenced
//!
//! It compares two **filesystem timestamps** — the executable's mtime against the newest `.rs`
//! under `crates/`. `SystemTime::now()` is never called, and nothing here can influence a
//! journal timestamp, a memory id or a reproduction hash.
//!
//! `determinism_guard.rs` greps for the spelling rather than the call, which is deliberate: a
//! crude enforced mechanism beats an elegant unenforced one. So this lives in its own file, and
//! the guard's exemption names it in a list separate from the clock fences — because *"legitimately
//! reads real time"* and *"compares two mtimes"* are different claims, and one list holding both
//! would let a genuine clock read in under an exemption granted for a file comparison.

use std::path::Path;

/// The phrase [`stale_against_source`] builds its sentence from, and **the string
/// `classify_degradation` matches on**.
///
/// A constant rather than two literals, for the reason `FELL_BACK_MARKER` is one: the projection's
/// arm and the sentence it classifies are otherwise free to drift, and the drift is silent — an
/// unmatched remedy falls to `DegradedPath::Unclassified`, whose headline is *"degraded · see the
/// Status tab"*. That is exactly the vague line this variant exists to replace, so a drift here
/// would restore the defect while every test stayed green.
pub(crate) const STALE_MARKER: &str = "older than the source it was built from";

/// Whether the running binary predates the source it was built from.
///
/// # Why a daemon needs this and a CLI does not
///
/// A daemon is long-lived. It loads a binary once and serves from it until something stops it —
/// so a rebuild changes the source, the tests and the developer's mental model while the process
/// keeps answering from the old code. **This cost M2 C2e two turns**: the persona was wired,
/// committed and unit-tested, and the daemon had been serving pre-persona code for an hour. The
/// symptom was "the persona does not work", which is a true observation about a false cause.
///
/// That is the same family as everything else in CLAUDE.md's table — a reading that is correct
/// about a system other than the one being asked about — so it gets an instrument rather than a
/// habit. Returns the newest source mtime when it is newer than the executable's.
///
/// **Absent outside a source tree**, which is correct: an installed binary has no `crates/` to be
/// stale against, and inventing a warning there would be noise.
pub(crate) fn stale_against_source() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let exe_time = exe.metadata().ok()?.modified().ok()?;

    // Walk up from the executable looking for the workspace's `crates/`.
    let mut root = exe.parent()?;
    let crates = loop {
        // LOOP-EXEMPT: walking up a path, not a driving loop.
        let candidate = root.join("crates");
        if candidate.is_dir() {
            break candidate;
        }
        root = root.parent()?;
    };

    let mut newest: Option<std::time::SystemTime> = None;
    let mut stack = vec![crates];
    while let Some(dir) = stack.pop() {
        // LOOP-EXEMPT: a filesystem walk, not a driving loop.
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                if p.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                if let Ok(t) = p.metadata().and_then(|m| m.modified()) {
                    if newest.is_none_or(|n| t > n) {
                        newest = Some(t);
                    }
                }
            }
        }
    }

    let newest = newest?;
    if newest <= exe_time {
        return None;
    }
    let behind = newest.duration_since(exe_time).ok()?;
    let mins = behind.as_secs() / 60;
    Some(format!(
        "this daemon's binary is {} {STALE_MARKER} — rebuild with `cargo build --release` and restart it, or it will keep serving the old code",
        if mins >= 1 { format!("{mins} min") } else { format!("{} s", behind.as_secs()) }
    ))
}

