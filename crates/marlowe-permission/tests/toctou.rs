//! The check-then-use race, tested by actually racing.
//!
//! Brief §8.3, and the reason ADR-024 refused to ship a path check without this:
//!
//! > canonicalize-then-open leaves a check-then-use race, so a traversal suite passing against a
//! > check-then-open implementation reports a boundary that is not there.
//!
//! # This file proves the suite would fail against a check-then-open implementation
//!
//! [`naive_check_then_open`] below is a deliberately vulnerable reference: canonicalize, verify
//! the result is under the workspace, then open by path. It is the implementation ADR-024
//! refused to ship, written here so the same interleaving can be run against both.
//!
//! **Two assertions, and neither is sufficient alone.**
//!
//! - The naive implementation, given the interleaving, **escapes** — it returns the contents of
//!   a file outside the workspace. That is what proves the race window is real and the test
//!   actually lands in it. Without this half, a green suite would be consistent with a test that
//!   never raced at all.
//! - The real implementation, given the *same* interleaving, does not escape.
//!
//! # The interleaving is deterministic, not hopeful
//!
//! A thread racing a resolver and hoping to land in a microsecond window is a test that passes
//! for the wrong reason most of the time. Instead the walk calls a `WalkObserver` at exactly the
//! instant a race would have to land — after component *k* is open, before *k+1* — and the test
//! swaps the directory there. In production the observer is `()` and compiles away.
//!
//! # What the swap is, per platform
//!
//! **Windows.** `rename` the pinned directory aside and drop a junction in its place. The rename
//! is what the walk's share mode (no `FILE_SHARE_DELETE`) is meant to refuse, so the test also
//! asserts *why* it failed — a swap that failed because `mklink` was missing would leave the
//! test green while measuring nothing.
//!
//! **POSIX.** `rename` the directory aside and drop a symlink in its place. `openat` with
//! `O_NOFOLLOW` refuses the symlink, so the rename is allowed to succeed and the walk still does
//! not follow it. **This half is written and NOT executed on the development machine** — see
//! `STATE.md`; Linux is the CI-only surface under ADR-002.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use marlowe_permission::scope::walk::WalkObserver;
use marlowe_permission::scope::{Access, PathScope, ScopeError, WorkspaceScope};
use marlowe_tools::PathGlob;

const IN_SCOPE: &str = "IN-SCOPE-CONTENT";
const OUT_OF_SCOPE: &str = "OUT-OF-SCOPE-CONTENT";

/// A workspace with a two-level path and an out-of-scope twin to escape to.
///
/// ```text
/// <tmp>/ws/a/b/secret.txt   -> IN-SCOPE-CONTENT
/// <tmp>/elsewhere/b/secret.txt -> OUT-OF-SCOPE-CONTENT
/// ```
struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
    elsewhere: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("marlowe-toctou-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let workspace = root.join("ws");
        let elsewhere = root.join("elsewhere");
        fs::create_dir_all(workspace.join("a").join("b")).unwrap();
        fs::create_dir_all(elsewhere.join("b")).unwrap();
        fs::write(workspace.join("a").join("b").join("secret.txt"), IN_SCOPE).unwrap();
        fs::write(elsewhere.join("b").join("secret.txt"), OUT_OF_SCOPE).unwrap();
        Self { root, workspace, elsewhere }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Replace `dir` with a link to `target`, by moving it aside first.
///
/// `rename` rather than `remove_dir_all` deliberately: removing would destroy the in-scope file,
/// so a test asserting "the real implementation still read the in-scope content" would be
/// asserting against a file that no longer exists, and would fail for the wrong reason.
fn swap_for_link(dir: &Path, target: &Path) -> std::io::Result<()> {
    let aside = dir.with_extension("moved-aside");
    fs::rename(dir, &aside)?;
    let created = create_link(dir, target);
    if created.is_err() {
        let _ = fs::rename(&aside, dir);
    }
    created
}

#[cfg(windows)]
fn create_link(link: &Path, target: &Path) -> std::io::Result<()> {
    // A junction, not a symlink: junctions need no privilege, and they are the Windows
    // directory-reparse escape in their own right.
    let out = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(String::from_utf8_lossy(&out.stderr).to_string()))
    }
}

#[cfg(unix)]
fn create_link(link: &Path, target: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

/// What the observer did when it fired, so the test can assert on the mechanism rather than only
/// on the outcome.
#[derive(Debug, Default)]
struct SwapReport {
    fired: bool,
    swap_error: Option<String>,
}

struct SwapAt {
    index: usize,
    dir: PathBuf,
    target: PathBuf,
    report: Mutex<SwapReport>,
}

impl WalkObserver for SwapAt {
    fn after_component(&self, index: usize, _opened: &Path) {
        if index != self.index {
            return;
        }
        let mut r = self.report.lock().unwrap();
        r.fired = true;
        r.swap_error = swap_for_link(&self.dir, &self.target).err().map(|e| e.to_string());
    }
}

/// **The implementation ADR-024 refused to ship.** Canonicalize, check, then open by path.
///
/// It is here to be defeated. If a future change makes the real scope behave like this, the
/// assertion in `the_naive_implementation_escapes_which_is_what_makes_this_a_race` still passes
/// and the one in `the_real_implementation_does_not_escape` starts failing — which is the
/// signal, and the reason both live in one file.
fn naive_check_then_open(
    workspace: &Path,
    requested: &str,
    observer: &dyn WalkObserver,
) -> std::io::Result<String> {
    let joined = workspace.join(requested);

    // 1. Canonicalize and check. Correct, as far as it goes.
    let resolved = fs::canonicalize(&joined)?;
    let root = fs::canonicalize(workspace)?;
    if !resolved.starts_with(&root) {
        return Err(std::io::Error::other("outside scope"));
    }

    // 2. The window. In the real world this is scheduler jitter; here it is deterministic.
    observer.after_component(0, &joined);

    // 3. Open by path — re-resolving the string the kernel already resolved once.
    let mut s = String::new();
    fs::File::open(&joined)?.read_to_string(&mut s)?;
    Ok(s)
}

fn declared() -> Vec<PathGlob> {
    vec![PathGlob::new("./**")]
}

#[test]
fn the_naive_implementation_escapes_which_is_what_makes_this_a_race() {
    // The anti-vacuity half. If this ever stops escaping, the interleaving has stopped landing
    // in the window and every other assertion in this file is worthless.
    let fx = Fixture::new("naive");
    let observer = SwapAt {
        index: 0,
        dir: fx.workspace.join("a"),
        target: fx.elsewhere.clone(),
        report: Mutex::new(SwapReport::default()),
    };

    let got = naive_check_then_open(&fx.workspace, "a/b/secret.txt", &observer);

    let report = observer.report.lock().unwrap();
    assert!(report.fired, "the observer never fired; the test did not reach the window");
    assert_eq!(
        report.swap_error, None,
        "the swap itself must succeed here — nothing pins the directory in the naive path"
    );
    assert_eq!(
        got.expect("the naive implementation opens something"),
        OUT_OF_SCOPE,
        "the check-then-open implementation must escape; if it does not, the window is not real \
         and this suite proves nothing"
    );
}

#[test]
fn the_real_implementation_does_not_escape() {
    let fx = Fixture::new("real");
    let observer = SwapAt {
        index: 0,
        dir: fx.workspace.join("a"),
        target: fx.elsewhere.clone(),
        report: Mutex::new(SwapReport::default()),
    };

    let opened = WorkspaceScope::new().expect("verified platform").open_observed(
        &declared(),
        &fx.workspace,
        "a/b/secret.txt",
        Access::Read,
        &observer,
    );

    let report = observer.report.lock().unwrap();
    assert!(report.fired, "the observer never fired; the test did not reach the window");

    match opened {
        Ok(scoped) => {
            let mut s = String::new();
            scoped.handle().try_clone().unwrap().read_to_string(&mut s).unwrap();
            assert_eq!(
                s, IN_SCOPE,
                "the handle must refer to the in-scope file, whatever happened mid-walk"
            );
            assert!(
                scoped.resolved().starts_with(&fx.workspace),
                "resolved outside the workspace: {}",
                scoped.resolved().display()
            );
        }
        // Refusing is also correct: what is forbidden is returning a handle to the out-of-scope
        // file. Both acceptable outcomes are named so a refusal is not mistaken for a failure.
        Err(ScopeError::OutsideScope { .. })
        | Err(ScopeError::IdentityChanged { .. })
        | Err(ScopeError::Unopenable { .. }) => {}
        Err(other) => panic!("unexpected refusal: {other}"),
    }
}

#[cfg(windows)]
#[test]
fn on_windows_the_swap_is_refused_by_the_share_mode_and_the_error_says_so() {
    // Assert the MECHANISM, not only the outcome. If the swap had failed because `mklink` was
    // absent, `the_real_implementation_does_not_escape` would still pass while measuring
    // nothing — the twelfth-instance failure family, applied to a security test.
    let fx = Fixture::new("mechanism");
    let observer = SwapAt {
        index: 0,
        dir: fx.workspace.join("a"),
        target: fx.elsewhere.clone(),
        report: Mutex::new(SwapReport::default()),
    };

    let _ = WorkspaceScope::new().expect("verified platform").open_observed(
        &declared(),
        &fx.workspace,
        "a/b/secret.txt",
        Access::Read,
        &observer,
    );

    let report = observer.report.lock().unwrap();
    assert!(report.fired);
    let err = report
        .swap_error
        .as_ref()
        .expect("the rename must FAIL: the walk holds `a` open without FILE_SHARE_DELETE");
    let lowered = err.to_lowercase();
    assert!(
        lowered.contains("access is denied")
            || lowered.contains("being used by another process")
            || lowered.contains("os error 5")
            || lowered.contains("os error 32"),
        "the rename must fail as a sharing or access violation — that is the pinning firing. \
         Got: {err}"
    );
}

#[test]
fn the_observer_is_a_no_op_in_production() {
    // The seam exists in shipping code, so the shipping behaviour is asserted: the default
    // implementation does nothing, and the same walk with `()` reads the in-scope file.
    let fx = Fixture::new("noop");
    let scoped = WorkspaceScope::new()
        .expect("verified platform")
        .open(&declared(), &fx.workspace, "a/b/secret.txt", Access::Read)
        .expect("an ordinary in-scope read succeeds");
    let mut s = String::new();
    scoped.handle().try_clone().unwrap().read_to_string(&mut s).unwrap();
    assert_eq!(s, IN_SCOPE);
}
