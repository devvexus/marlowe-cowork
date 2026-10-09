//! **DOES ANYTHING IN THIS CRATE WAIT FOREVER ON AN EXTERNAL COMMAND?**
//!
//! That is the question. The guard this replaces answered a different one — *does **this one
//! file** wait forever* — and the difference cost two suite outages.
//!
//! # The two outages, because the guard's scope is the whole story
//!
//! **2026-08-25.** `cue::dense::vram` ran `ollama` and `nvidia-smi` through a bare
//! `Command::output()`, which waits with no ceiling. Three `rerank_provider` tests sat inside
//! `Reserve::read()` and **the entire workspace suite wedged for 12+ minutes**; every crate after
//! that binary went unrun, and the run looked alive the whole time. The fix was
//! `vram::bounded_output` plus a guard — `include_str!("vram.rs")`, asserting no bare `.output()`
//! in the module it lived in.
//!
//! **2026-08-26.** `tests/rerank_provider.rs` grew a bare, unbounded `ollama list` spawn. Same
//! shape, same command, one file away. That binary hung **~25 minutes at 1 GB RSS**, stalling
//! everything queued behind it, where a clean run finishes it in 3.89 s. **The guard was green
//! throughout.** It had not regressed and had not moved: it was structurally incapable of seeing a
//! sibling file, and its own name — *"no external command **in this module** waits forever"* — was
//! an accurate description of a scope nobody had chosen.
//!
//! This is CLAUDE.md's instance **#14 with the sign flipped**. There a guarded path MOVED and the
//! guard silently covered nothing. Here the guard never moved and the hazard was **copied outside
//! its reach**. Both fail the same way: a protective mechanism reads identically whether or not the
//! property it names holds.
//!
//! # Structural containment, not pattern-matching on hazards
//!
//! There are several ways to wait forever on a child — `.output()`, `.status()`, `.wait()`,
//! `.wait_with_output()` — and a guard enumerating them is a blocklist that the next shape walks
//! past. So the subject is narrower and total: **a child process may be CONSTRUCTED in exactly one
//! module**, [`ALLOWED`], whose one entry point applies a ceiling, a null stdin and a
//! kill-and-reap. Everywhere else in the crate — `src/`, `tests/`, `examples/` — routing through
//! `vram::bounded_output` is the only way to run anything.
//!
//! Same shape as the load-time refusal in `CapabilityProfile`: make the hazard *unconstructible*
//! outside one audited place rather than detectable after the fact. The constructor is the choke
//! point because every route to a child process goes through it — fully qualified, `use`-imported,
//! or a builder held in a variable.
//!
//! # WHAT THIS REPORTS IF ITS OWN SUBJECT MOVES — the question #14 exists to force
//!
//! A guard is a claim about a set of paths, and a claim about paths needs something that breaks
//! when a path stops existing. `protect-boundaries.py --self-check` is the precedent in this repo:
//! it fails when a guarded path does not exist, so a rename cannot silently un-guard anything.
//! Five mechanisms here, and each fails **by name**:
//!
//! | if this happens | what fails |
//! |---|---|
//! | `src/`, `tests/` or `examples/` is renamed or emptied | [`ROOTS`] — the directory is named and the walk refuses to proceed |
//! | `vram.rs` moves, so the containment module has no file | [`ALLOWED`] — the one exemption must resolve to a real file |
//! | a file that historically shelled out is renamed | [`ROSTER`] — every entry must be found by the walk |
//! | the walk silently stops finding files | [`MIN_FILES`] — a fixed floor, so a walk that returns three files fails |
//! | **this file is deleted** | `vram.rs` fails to COMPILE: its back-link is an `include_str!` on this path |
//!
//! The last one is the pair to the first four and the reason `vram.rs` still carries a test at all.
//! Widening coverage is a one-line edit to `ROSTER` or `ROOTS`; *narrowing* it is impossible to do
//! quietly, which is the asymmetry the whole design is for.
//!
//! # The one gap, named rather than left implicit
//!
//! A file pulled in from **outside** this crate by `#[path = "..."]` is not under the crate root
//! and is not walked. Today that is `marlowe-loop/tests/common/exclusive.rs`, which constructs no
//! process. A guard is scoped to the tree it walks, and that tree is this crate.

use std::path::{Path, PathBuf};

/// The one module in which a child process may be constructed.
///
/// It is `vram.rs` because that is where the ceiling lives: `bounded_output` polls `try_wait` a
/// fixed number of times, then kills and reaps. Moving the helper means moving this constant, and
/// the roster below means the move cannot be silent.
const ALLOWED: &str = "src/cue/dense/vram.rs";

/// Every directory under the crate root that is walked. **Each must exist.**
///
/// `examples/` is here because an example is code a human runs on this machine, and a wedged
/// example is a wedged terminal; three of the four call sites fixed on 2026-08-27 were in one.
const ROOTS: [&str; 3] = ["src", "tests", "examples"];

/// Files that MUST be found by the walk, checked by name.
///
/// **This is the `--self-check`.** Every entry either ran an external command historically or is
/// load-bearing for the containment. If one is renamed, the walk stops covering it — and instead
/// of going quietly green, the guard fails naming the missing path, which is the whole point of
/// the mechanism. Update this list in the same commit as the rename.
const ROSTER: [&str; 6] = [
    // The containment module: holds the ceiling and the only permitted process construction.
    "src/cue/dense/vram.rs",
    // Spawned `powershell` through a bare `.output()` until 2026-08-27.
    "src/cue/dense/hostmem.rs",
    // Spawned `ollama list` through a bare `.output()`; hung a test binary ~25 min on 2026-08-26.
    "tests/rerank_provider.rs",
    // Four bare `.output()` call sites (`powershell`, `nvidia-smi` x3) until 2026-08-27.
    "examples/embed_provider_bench.rs",
    // Read the card; the places a future device probe would most plausibly be added.
    "tests/session_footprint.rs",
    "tests/embedder_provider.rs",
];

/// A floor on the number of `.rs` files walked, as a FIXED LITERAL.
///
/// Not derived from anything: a floor computed from what the walk found is satisfied by whatever
/// the walk found, which is the "declared control that nothing reads" shape. 30 sits below today's
/// count and far above the handful a broken walk would return, so deleting the crate's tests
/// cannot leave this green.
const MIN_FILES: usize = 30;

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every `.rs` file under `dir`, recursively, as paths relative to the crate root.
fn rs_files_under(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("the guard cannot read {}: {e}", dir.display()));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rs_files_under(root, &path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let rel =
                path.strip_prefix(root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
            out.push(rel);
        }
    }
}

/// `(line number, line)` for every line of `text` containing `needle`.
///
/// Factored out so the vacuity control below can drive the **same** function the walk drives. A
/// control that exercises a re-implementation of the check is a control for the re-implementation.
fn offending_lines(text: &str, needle: &str) -> Vec<(usize, String)> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| l.contains(needle))
        .map(|(i, l)| (i + 1, l.trim().to_string()))
        .collect()
}

/// The process constructor's spelling, assembled at run time.
///
/// **Deliberately not a literal.** This file is walked like every other, so a literal here would
/// match itself and the guard would flag its own source. Building the needle keeps this file
/// *inside* the guarded set rather than exempted from it — and if someone folds the concatenation
/// back into a literal, the guard fails on itself, loudly, which is the safe direction.
fn construct_needle() -> String {
    format!("{}::{}", "Command", "new")
}

#[test]
fn every_external_command_in_this_crate_is_bounded() {
    let root = crate_root();
    assert!(
        root.is_dir(),
        "the guard's own subject is missing: {} does not exist. CARGO_MANIFEST_DIR is baked in at \
         compile time, so this means the crate moved.",
        root.display()
    );

    // ---- self-check 1: every walked directory exists, BY NAME ---------------------------------
    let mut files: Vec<String> = Vec::new();
    for dir in ROOTS {
        let path = root.join(dir);
        assert!(
            path.is_dir(),
            "the guard walks `{dir}/` and it is not there ({}). Coverage has SHRUNK: either the \
             directory was renamed, in which case fix ROOTS in the same commit, or it was deleted, \
             in which case say so. A guard that quietly walks less is a comment.",
            path.display()
        );
        rs_files_under(&root, &path, &mut files);
    }
    files.sort();

    // ---- self-check 2: a floor on coverage ----------------------------------------------------
    assert!(
        files.len() >= MIN_FILES,
        "the walk found only {} .rs files under {ROOTS:?}, below the floor of {MIN_FILES}. Either \
         the crate shrank drastically or the walk is broken; either way this guard is no longer \
         covering what it claims to.",
        files.len()
    );

    // ---- self-check 3: every rostered path is actually being walked ---------------------------
    for named in ROSTER {
        assert!(
            files.iter().any(|f| f == named),
            "the guard's roster names `{named}` and the walk did not find it. THIS IS THE #14 \
             FAILURE: a guarded path moved, and without this assertion the guard would have gone \
             green while covering one file fewer. Rename it in ROSTER in the same commit, or if it \
             was deleted, remove the entry and say why. Walked: {files:?}"
        );
    }

    // ---- self-check 4: the one exemption resolves to a real file ------------------------------
    assert!(
        root.join(ALLOWED).is_file(),
        "the containment module `{ALLOWED}` does not exist. Every other file in the crate is \
         forbidden from constructing a process and the one that is allowed to is missing, so \
         either the ceiling moved (update ALLOWED) or it is gone (then nothing is bounded)."
    );
    assert!(
        files.iter().any(|f| f == ALLOWED),
        "`{ALLOWED}` exists but is not inside any walked directory, so the guard cannot check that \
         the module it exempts is the one that applies the ceiling."
    );

    // ---- the check ----------------------------------------------------------------------------
    let needle = construct_needle();
    let mut violations: Vec<String> = Vec::new();
    for rel in &files {
        if rel == ALLOWED {
            continue;
        }
        let text = std::fs::read_to_string(root.join(rel))
            .unwrap_or_else(|e| panic!("the guard cannot read {rel}: {e}"));
        for (line, body) in offending_lines(&text, &needle) {
            violations.push(format!("  {rel}:{line}\n      {body}"));
        }
    }
    assert!(
        violations.is_empty(),
        "a child process is constructed outside `{ALLOWED}`, which is the shape that wedged this \
         workspace's suite on 2026-08-25 (12+ min) and hung a test binary on 2026-08-26 (~25 min \
         at 1 GB RSS).\n\n{}\n\nRoute it through \
         `marlowe_memory::cue::dense::vram::bounded_output`, which polls `try_wait` a bounded \
         number of times and then kills and reaps. It gives a null stdin and CREATE_NO_WINDOW for \
         free, and every caller in this crate already degrades to `None` with a named reason -- so \
         a timeout costs a READING, never the process.\n\nThe check is textual: if this fired on \
         prose, spell the type differently in the comment.",
        violations.join("\n")
    );

    // ---- the exempted module must be the one that BOUNDS --------------------------------------
    //
    // Without this the guard degenerates into "all the processes live in vram.rs", which would be
    // satisfied by moving an unbounded wait into vram.rs.
    let allowed_src = std::fs::read_to_string(root.join(ALLOWED)).expect("checked above");
    let body = allowed_src.split("mod tests").next().expect("there is code before the tests");
    assert!(
        body.contains("fn bounded_output"),
        "`{ALLOWED}` is the one module allowed to construct a process and it no longer defines the \
         bounded entry point. The exemption is then an exemption from nothing."
    );
    for waiter in [".output()", ".status()"] {
        let hits = offending_lines(body, waiter);
        assert!(
            hits.is_empty(),
            "`{ALLOWED}` is the only place a process may be constructed, and it waits on one with \
             a bare `{waiter}` -- no ceiling at all:\n{}",
            hits.iter().map(|(n, l)| format!("  {n}: {l}")).collect::<Vec<_>>().join("\n")
        );
    }
    for evidence in ["try_wait", "kill()"] {
        assert!(
            body.contains(evidence),
            "`{ALLOWED}`'s bounded path no longer contains `{evidence}`, so there is nothing \
             showing the wait is bounded or that an outlived child is killed. An unreaped child is \
             the next hang."
        );
    }

    eprintln!(
        "bounded-command guard: {} .rs files walked under {ROOTS:?}, {} rostered by name, one \
         module exempt ({ALLOWED})",
        files.len(),
        ROSTER.len()
    );
}

/// **THE VACUITY CONTROL. The check above asserts an ABSENCE, so it is green on an empty crate.**
///
/// Every assertion in this file passes if the detector cannot see anything — which is the exact
/// failure mode of the guard it replaces, one level up. So: hand the same function the walk uses a
/// source file that contains the hazard, and require it to be seen. And hand it one that does not,
/// and require silence, because a detector that flags everything is equally useless.
#[test]
fn the_detector_can_actually_see_an_unbounded_command() {
    let needle = construct_needle();

    // The line as it stood in `tests/rerank_provider.rs` on 2026-08-26, assembled rather than
    // written out for the same reason the needle is.
    let hazard = format!(
        "fn probe() {{\n    let installed = std::process::{}(\"ollama\").arg(\"list\").output();\n}}",
        needle
    );
    let hits = offending_lines(&hazard, &needle);
    assert_eq!(
        hits.len(),
        1,
        "the detector cannot see the exact line that hung a test binary for 25 minutes on \
         2026-08-26, so the walk above proves nothing: {hits:?}"
    );
    assert_eq!(hits[0].0, 2, "the line number reported must be the offending line");

    // The negative half: a file that routes through the helper must NOT be flagged, or the guard
    // fires on the fix and gets disabled.
    let fixed = "fn probe() -> Option<Vec<String>> {\n    vram::ollama_model_names()\n}";
    assert!(
        offending_lines(fixed, &needle).is_empty(),
        "the detector flags a call site that routes through the bounded helper"
    );

    // And the needle is the spelling the compiler sees, not an approximation of it.
    assert_eq!(needle, "Command".to_string() + "::" + "new");
}
