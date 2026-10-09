//! The brief §13 hook guards paths. This asserts the paths still exist.
//!
//! CLAUDE.md: *"a component with no entry is unguarded regardless of what this list says — the
//! entry is the enforcement, and the list is only a map of it."* The corollary is the failure
//! this test exists for: **a component whose file moved is unguarded too, and nothing says so.**
//!
//! It happened in M2 Session B. `crates/marlowe-permission/src/scope.rs` became
//! `scope/{mod,request,glob,walk}.rs`, the hook's entry named a file that no longer existed, and
//! path scoping — the wall, per ADR-002 — was silently unprotected. A pipe test caught it; a pipe
//! test that nobody runs would not have.
//!
//! This lives in `marlowe-permission` because that is where most guarded paths now are, and
//! because `marlowe-loop/tests/hp10_budgets.rs` already sets the precedent for a repo-scanning
//! test. It shells out to the hook rather than reimplementing the list: two copies of a
//! protected-path list is how they disagree.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

#[test]
fn every_guarded_path_in_the_boundary_hook_still_exists() {
    let root = repo_root();
    let hook = root.join(".claude").join("hooks").join("protect-boundaries.py");
    assert!(hook.exists(), "the boundary hook is missing: {}", hook.display());

    let out = Command::new("python")
        .arg(&hook)
        .arg("--self-check")
        .arg(&root)
        .output()
        .or_else(|_| {
            Command::new("python3").arg(&hook).arg("--self-check").arg(&root).output()
        })
        .expect("python is needed to run the boundary hook's self-check");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "the brief §13 hook names paths that no longer exist, so those components are \
         UNGUARDED and nothing else would report it:\n{stderr}"
    );
}

#[test]
fn the_hook_actually_fires_on_a_guarded_path() {
    // The self-check above proves the list is not stale. This proves the list does something —
    // together they are the pair, and either alone is a proxy.
    let root = repo_root();
    let hook = root.join(".claude").join("hooks").join("protect-boundaries.py");

    let guarded = "C:/anywhere/crates/marlowe-permission/src/scope/walk.rs";
    let payload = format!(r#"{{"tool_input":{{"file_path":"{guarded}"}}}}"#);

    let mut child = Command::new("python")
        .arg(&hook)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .or_else(|_| {
            Command::new("python3")
                .arg(&hook)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
        })
        .expect("python is needed to run the boundary hook");
    {
        use std::io::Write;
        child.stdin.as_mut().unwrap().write_all(payload.as_bytes()).unwrap();
    }
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    // `json.dumps` escapes non-ASCII by default, so the section sign arrives as `§`.
    // Asserting on the literal would have made this test fail for a reason that has nothing to
    // do with the boundary.
    assert!(
        stdout.contains("\"permissionDecision\": \"ask\"") && stdout.contains("BOUNDARY"),
        "the hook must ask for a decision on a guarded path, got: {stdout}"
    );
}

/// The set itself is pinned here, because `--self-check` cannot see the list shrink.
///
/// **This is the gap a mutation pass measured on 2026-08-29, and the measurement is the reason
/// the test exists.** Deleting the `crates/marlowe-loop/src/driver.rs` row from the hook's
/// `PROTECTED` dict turned NOTHING red: `--self-check` exited 0, both tests above passed, and a
/// stdin probe on that path returned empty stdout while a same-shape probe on a still-listed path
/// returned `"permissionDecision": "ask"` in the same command — so the silence was the deletion,
/// not the probe. `self_check` iterates `for suffix in PROTECTED`, so a row that is no longer in
/// the dict is trivially satisfied.
///
/// It is instance #14 one step earlier. #14 is a guard whose SUBJECT moved — `scope.rs` becoming
/// `scope/mod.rs` — and the fix for that is `--self-check`. This is a guard that was REMOVED, and
/// no check that reads the list can catch it, because the evidence is the row's absence.
///
/// **The module doc above says two copies of a protected-path list is how they disagree. That is
/// still true and it is now the mechanism rather than the objection.** A disagreement here fails
/// the build and asks a human to say which copy is right — which is the decision the §13 boundary
/// exists to require. The failure the pin prevents is a row leaving with nothing to say so.
///
/// The two directions are NOT the same event and the assertion reports them separately:
/// an ADDED row is monotonic and safe (the hook returns `ask`, so more rows means the agent asks
/// more often) and only needs recording here so the deletion check keeps covering it; a REMOVED
/// row is a component silently unguarded, which is the failure being guarded against.
const EXPECTED_PROTECTED: &[&str] = &[
    "/marlowe-permission/src/scope/",
    "/persona/",
    "crates/marlowe-daemon/src/mcp.rs",
    "crates/marlowe-daemon/src/memory.rs",
    "crates/marlowe-journal/src/journal.rs",
    "crates/marlowe-journal/src/signature.rs",
    "crates/marlowe-loop/src/driver.rs",
    "crates/marlowe-loop/src/profile.rs",
    "crates/marlowe-loop/src/provenance.rs",
    "crates/marlowe-loop/src/steer.rs",
    "crates/marlowe-memory/src/trust.rs",
    "crates/marlowe-permission/src/adjudicate.rs",
    "crates/marlowe-permission/src/egress.rs",
    "crates/marlowe-permission/src/taint.rs",
    "crates/marlowe-tools/src/pin.rs",
];

#[test]
fn no_guarded_path_leaves_the_boundary_hook_unnoticed() {
    let root = repo_root();
    let hook = root.join(".claude").join("hooks").join("protect-boundaries.py");

    let out = Command::new("python")
        .arg(&hook)
        .arg("--list-protected")
        .output()
        .or_else(|_| Command::new("python3").arg(&hook).arg("--list-protected").output())
        .expect("python is needed to enumerate the boundary hook's protected set");
    assert!(
        out.status.success(),
        "the hook could not enumerate its protected set: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let actual: Vec<&str> = stdout.lines().map(str::trim).filter(|l| !l.is_empty()).collect();

    // A non-empty result is asserted before the comparison. `--list-protected` on a hook whose
    // dicts were emptied would print nothing and exit 0, and an empty-vs-empty comparison is the
    // vacuous pass this whole test is aimed at — the pin has to fail loudly on that, not agree
    // with it.
    assert!(
        !actual.is_empty(),
        "the hook enumerated ZERO protected entries. Either both dicts are empty — every §13 \
         component unguarded — or `--list-protected` has stopped reading them."
    );

    let removed: Vec<&str> =
        EXPECTED_PROTECTED.iter().copied().filter(|e| !actual.contains(e)).collect();
    let added: Vec<&str> =
        actual.iter().copied().filter(|a| !EXPECTED_PROTECTED.contains(a)).collect();

    assert!(
        removed.is_empty(),
        "A §13 GUARD WAS REMOVED FROM THE HOOK AND NOTHING ELSE WOULD REPORT IT.\n\
         These paths are no longer in `PROTECTED`/`PROTECTED_DIRS`, so edits to them raise no \
         boundary prompt: {removed:?}\n\
         `--self-check` cannot see this — it iterates the list, so a deleted row is trivially \
         satisfied. If the removal is INTENDED it needs a DECISIONS.md entry and a human's \
         approval, exactly as any other change to safety machinery does; then update \
         EXPECTED_PROTECTED in this file."
    );
    assert!(
        added.is_empty(),
        "New guarded paths are in the hook but not pinned here: {added:?}\n\
         Adding a guard is safe and monotonic — the hook returns `ask`, so more rows means the \
         agent asks MORE often. This is not a complaint about the addition. Add the paths to \
         EXPECTED_PROTECTED so a later DELETION of them fails by name too; until then they carry \
         the same blind spot this test was written to close."
    );
}
