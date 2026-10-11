//! The adversarial path-traversal suite. ADR-002's table, brief §8.3, M2 acceptance.
//!
//! # A suite of refusals can be passed by refusing everything
//!
//! That is not hypothetical — it is exactly what M2 Session A shipped, deliberately, and it
//! would satisfy every negative assertion below. So the suite carries **positive controls**: a
//! legitimate deep read, a legitimate write target, and a file whose name contains characters
//! that merely *look* dangerous, all of which must open. A traversal suite without them measures
//! whether the scope is present, not whether it is correct.
//!
//! # An unrunnable class is reported, never skipped
//!
//! Creating a symlink on Windows needs Developer Mode or elevation. A test that quietly passed
//! when it could not create one would report a class as covered that was never exercised — the
//! believed-boundary failure, on the wall with no kernel behind it. So
//! [`every_traversal_class_is_accounted_for`] runs every class inline, prints a manifest, and
//! **names any class it could not run**. Set `MARLOWE_TRAVERSAL_STRICT=1` (CI) to turn an
//! unrunnable class into a failure.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use marlowe_permission::scope::{Access, PathScope, ScopeError, WorkspaceScope};
use marlowe_tools::PathGlob;

const SECRET: &str = "WORKSPACE-FILE";
const OUTSIDE: &str = "OUTSIDE-FILE";

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
    outside: PathBuf,
}

/// Fixture directories are unique per *instance*, not per class name.
///
/// The manifest test runs the same class functions the standalone tests run, and cargo runs
/// tests in parallel — so a name-keyed directory means one test's `Drop` deletes the tree
/// another is reading. That surfaced as a `NotFound` in the manifest and is a property of the
/// harness, not of the scope.
static FIXTURE_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

impl Fixture {
    fn new(name: &str) -> Self {
        let n = FIXTURE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir()
            .join(format!("marlowe-traversal-{name}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let workspace = root.join("ws");
        let outside = root.join("outside");
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::create_dir_all(workspace.join("out")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(workspace.join("src").join("main.rs"), SECRET).unwrap();
        fs::write(workspace.join("out").join("report.txt"), SECRET).unwrap();
        fs::write(outside.join("passwd"), OUTSIDE).unwrap();
        Self { root, workspace, outside }
    }

    fn open(&self, globs: &[&str], requested: &str) -> Result<String, ScopeError> {
        let declared: Vec<PathGlob> = globs.iter().map(|g| PathGlob::new(*g)).collect();
        let scoped = WorkspaceScope::new()
            .expect("verified platform")
            .open(&declared, &self.workspace, requested, Access::Read)?;
        let mut s = String::new();
        scoped
            .handle()
            .try_clone()
            .and_then(|mut f| f.read_to_string(&mut s))
            .map_err(|e| ScopeError::Unopenable {
                requested: requested.to_string(),
                detail: e.to_string(),
            })?;
        Ok(s)
    }

    /// Open with `CreateOrOpen`, as `edit`'s `WritePath` parameter does.
    fn create(&self, globs: &[&str], requested: &str) -> Result<PathBuf, ScopeError> {
        let declared: Vec<PathGlob> = globs.iter().map(|g| PathGlob::new(*g)).collect();
        let scoped = WorkspaceScope::new().expect("verified platform").open(
            &declared,
            &self.workspace,
            requested,
            Access::CreateOrOpen,
        )?;
        Ok(scoped.resolved().to_path_buf())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// Positive controls — a suite that refuses everything must fail these
// ─────────────────────────────────────────────────────────────────────────────────────────

#[test]
fn a_legitimate_in_scope_read_succeeds() {
    let fx = Fixture::new("positive");
    assert_eq!(fx.open(&["./**"], "src/main.rs").unwrap(), SECRET);
    assert_eq!(fx.open(&["./src/**"], "src/main.rs").unwrap(), SECRET);
    assert_eq!(fx.open(&["./**"], "./src/main.rs").unwrap(), SECRET);
}

#[test]
fn a_name_that_merely_looks_dangerous_still_opens() {
    // The false-positive check. A scope tuned until every negative test passes tends to refuse
    // legitimate names too, and nothing in a refusal-only suite would notice.
    let fx = Fixture::new("lookalike");
    for name in ["console.log", "nullable.rs", "a..b.txt", "dot.in.name.rs", "-leading-dash"] {
        fs::write(fx.workspace.join("src").join(name), SECRET).unwrap();
        assert_eq!(
            fx.open(&["./**"], &format!("src/{name}")).unwrap(),
            SECRET,
            "`{name}` is a legal filename and must open"
        );
    }
}

#[test]
fn a_write_target_may_be_created_inside_the_verified_parent() {
    // The positive control for `Access::CreateOrOpen`, which is what `edit`'s WritePath
    // parameter asks for. A scope that only ever opened existing files would pass every other
    // test in this suite and make `edit` impossible.
    let fx = Fixture::new("create");
    let made = fx.create(&["./**"], "out/new-report.txt").expect("a new file in scope");
    assert!(made.exists(), "the file was not created: {}", made.display());
    assert!(made.starts_with(&fx.workspace));

    // Creation obeys the declaration exactly as reading does.
    let e = fx.create(&["./out/**"], "src/sneaky.txt").unwrap_err();
    assert!(matches!(e, ScopeError::Undeclared { .. }), "{e:?}");
    assert!(!fx.workspace.join("src").join("sneaky.txt").exists(), "a refused create must not create");

    // ...and it cannot create through a link that leaves the workspace.
    let link = fx.workspace.join("escape");
    if make_junction(&link, &fx.outside).is_ok() {
        let e = fx.create(&["./**"], "escape/planted.txt").unwrap_err();
        assert!(
            matches!(e, ScopeError::OutsideScope { .. } | ScopeError::Unopenable { .. }),
            "{e:?}"
        );
        assert!(!fx.outside.join("planted.txt").exists(), "a file was created outside the workspace");
    }
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// String-level classes
// ─────────────────────────────────────────────────────────────────────────────────────────

#[test]
fn relative_traversal_is_refused() {
    let fx = Fixture::new("dotdot");
    for p in [
        "../outside/passwd",
        "..\\outside\\passwd",
        "src/../../outside/passwd",
        "src/../..//outside/passwd",
        "a/b/c/../../../../outside/passwd",
        "./../outside/passwd",
    ] {
        let e = fx.open(&["./**"], p).unwrap_err();
        assert!(matches!(e, ScopeError::Malformed { .. }), "`{p}` gave {e:?}");
    }
}

#[test]
fn rooted_and_windows_path_forms_are_refused() {
    let fx = Fixture::new("rooted");
    for p in [
        "/etc/passwd",
        "\\windows\\win.ini",
        "C:\\Windows\\win.ini",
        "C:windows",
        "\\\\server\\share\\file",
        "\\\\?\\C:\\Windows\\win.ini",
        "\\\\?\\UNC\\server\\share",
        "\\\\.\\PhysicalDrive0",
    ] {
        let e = fx.open(&["./**"], p).unwrap_err();
        assert!(matches!(e, ScopeError::Malformed { .. }), "`{p}` gave {e:?}");
    }
}

#[test]
fn alternate_data_streams_and_device_names_are_refused() {
    let fx = Fixture::new("adsdev");
    for p in ["src/main.rs:hidden", "src::$INDEX_ALLOCATION", "CON", "src/NUL", "out/COM1.txt"] {
        let e = fx.open(&["./**"], p).unwrap_err();
        assert!(matches!(e, ScopeError::Malformed { .. }), "`{p}` gave {e:?}");
    }
}

#[test]
fn win32_munging_and_short_names_are_refused() {
    let fx = Fixture::new("munge");
    for p in ["src/main.rs.", "src/main.rs ", "src./main.rs", "PROGRA~1/x", "src/MYFILE~1.TXT"] {
        let e = fx.open(&["./**"], p).unwrap_err();
        assert!(matches!(e, ScopeError::Malformed { .. }), "`{p}` gave {e:?}");
    }
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// Declaration and case
// ─────────────────────────────────────────────────────────────────────────────────────────

#[test]
fn an_undeclared_subtree_is_refused_even_though_it_exists_and_is_in_the_workspace() {
    let fx = Fixture::new("undeclared");
    assert_eq!(fx.open(&["./out/**"], "out/report.txt").unwrap(), SECRET);
    let e = fx.open(&["./out/**"], "src/main.rs").unwrap_err();
    assert!(matches!(e, ScopeError::Undeclared { .. }), "{e:?}");
}

#[test]
fn a_case_variation_cannot_dodge_a_narrow_declaration() {
    // On a case-insensitive volume `SRC/MAIN.RS` and `src/main.rs` are one file. The glob match
    // is case-sensitive, so the variant does not match a narrow declaration — and that failure
    // direction is the safe one: refused rather than admitted.
    let fx = Fixture::new("case");
    let e = fx.open(&["./out/**"], "OUT/report.txt").unwrap_err();
    assert!(matches!(e, ScopeError::Undeclared { .. }), "{e:?}");
    let e = fx.open(&["./src/**"], "SRC/main.rs").unwrap_err();
    assert!(matches!(e, ScopeError::Undeclared { .. }), "{e:?}");
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// Filesystem classes — links
// ─────────────────────────────────────────────────────────────────────────────────────────

#[cfg(windows)]
fn make_junction(link: &Path, target: &Path) -> std::io::Result<()> {
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

#[cfg(windows)]
fn make_symlink_dir(link: &Path, target: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

#[cfg(unix)]
fn make_junction(link: &Path, target: &Path) -> std::io::Result<()> {
    // POSIX has no junction; the directory-reparse class is a symlink here.
    std::os::unix::fs::symlink(target, link)
}

#[cfg(unix)]
fn make_symlink_dir(link: &Path, target: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

/// A directory link planted *inside* the workspace that resolves outside it. ADR-002's row:
/// *"links planted inside a declared path that resolve outside it"*.
fn link_escape(make: fn(&Path, &Path) -> std::io::Result<()>, name: &str) -> Result<(), String> {
    let fx = Fixture::new(name);
    let link = fx.workspace.join("escape");
    make(&link, &fx.outside).map_err(|e| e.to_string())?;

    // Prove the link really points out of the workspace, or the assertion below is vacuous.
    let via_link = fs::read_to_string(link.join("passwd")).map_err(|e| e.to_string())?;
    assert_eq!(via_link, OUTSIDE, "the fixture's link does not actually escape");

    let e = fx.open(&["./**"], "escape/passwd").unwrap_err();
    assert!(
        matches!(e, ScopeError::OutsideScope { .. } | ScopeError::Unopenable { .. }),
        "a link out of the workspace must be refused, got {e:?}"
    );
    // And nothing leaked: the error carries no file content.
    assert!(!e.to_string().contains(OUTSIDE));
    Ok(())
}

#[test]
fn a_junction_out_of_the_workspace_is_refused() {
    link_escape(make_junction, "junction").expect("junctions need no privilege on Windows");
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// The manifest
// ─────────────────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
enum Class {
    Ran,
    Unrunnable(String),
}

fn run_class(f: impl FnOnce() -> Result<(), String>) -> Class {
    match f() {
        Ok(()) => Class::Ran,
        Err(why) => Class::Unrunnable(why),
    }
}

#[test]
fn every_traversal_class_is_accounted_for() {
    // Runs every class inline so the manifest is authoritative — a per-test registry would
    // depend on test ordering, and cargo runs tests in parallel in an unspecified order.
    let mut manifest: Vec<(&str, Class)> = Vec::new();

    manifest.push(("relative traversal", run_class(|| { relative_traversal_is_refused(); Ok(()) })));
    manifest.push(("rooted / UNC / \\\\?\\ / device", run_class(|| { rooted_and_windows_path_forms_are_refused(); Ok(()) })));
    manifest.push(("alternate data streams + device names", run_class(|| { alternate_data_streams_and_device_names_are_refused(); Ok(()) })));
    manifest.push(("win32 munging + 8.3 short names", run_class(|| { win32_munging_and_short_names_are_refused(); Ok(()) })));
    manifest.push(("case collisions", run_class(|| { a_case_variation_cannot_dodge_a_narrow_declaration(); Ok(()) })));
    manifest.push(("undeclared subtree", run_class(|| { an_undeclared_subtree_is_refused_even_though_it_exists_and_is_in_the_workspace(); Ok(()) })));
    manifest.push(("unicode normalization", run_class(|| { unicode_normalization_is_matched_not_dodged(); Ok(()) })));
    manifest.push(("positive control: legitimate read", run_class(|| { a_legitimate_in_scope_read_succeeds(); Ok(()) })));
    manifest.push(("positive control: lookalike names", run_class(|| { a_name_that_merely_looks_dangerous_still_opens(); Ok(()) })));
    manifest.push(("positive control: create in scope", run_class(|| { a_write_target_may_be_created_inside_the_verified_parent(); Ok(()) })));

    // The two that can be unrunnable.
    manifest.push(("junction escape", run_class(|| link_escape(make_junction, "manifest-junction"))));
    manifest.push(("symlink escape", run_class(|| link_escape(make_symlink_dir, "manifest-symlink"))));

    let mut unrunnable = Vec::new();
    println!("\n  path-traversal coverage");
    println!("  ───────────────────────");
    for (name, class) in &manifest {
        match class {
            Class::Ran => println!("  RAN         {name}"),
            Class::Unrunnable(why) => {
                println!("  UNRUNNABLE  {name}  — {why}");
                unrunnable.push((*name, why.clone()));
            }
        }
    }
    println!();

    let strict = std::env::var("MARLOWE_TRAVERSAL_STRICT").is_ok();
    if !unrunnable.is_empty() {
        let summary = unrunnable
            .iter()
            .map(|(n, w)| format!("{n} ({w})"))
            .collect::<Vec<_>>()
            .join("; ");
        assert!(
            !strict,
            "MARLOWE_TRAVERSAL_STRICT is set and these traversal classes could not run: {summary}"
        );
        // Not a failure without strict mode, because a developer machine without Developer Mode
        // is a real and common environment. It is printed every run and recorded in STATE.md, so
        // it cannot be mistaken for coverage.
        eprintln!(
            "WARNING: {} traversal class(es) did not run on this machine: {summary}. \
             This suite therefore does NOT certify those classes here. Enable Developer Mode or \
             run elevated, and set MARLOWE_TRAVERSAL_STRICT=1 in CI.",
            unrunnable.len()
        );
    }

    assert!(
        manifest.iter().filter(|(_, c)| matches!(c, Class::Ran)).count() >= 9,
        "at least the platform-independent classes and the positive controls must run"
    );
}

#[test]
fn unicode_normalization_is_matched_not_dodged() {
    // Two spellings of one name must reach one decision. The filesystem half differs by
    // platform — NTFS preserves bytes, APFS normalizes — so the assertion is on the decision the
    // scope reaches, which is the part this crate owns.
    let fx = Fixture::new("unicode");
    let nfc = "caf\u{e9}.md";
    let nfd = "caf\u{65}\u{301}.md";
    fs::write(fx.workspace.join("src").join(nfc), SECRET).unwrap();

    // A narrow declaration written in one form must admit a request written in the other.
    let declared = format!("./src/caf\u{65}\u{301}.md");
    let outcome = fx.open(&[&declared], &format!("src/{nfc}"));
    assert!(
        !matches!(outcome, Err(ScopeError::Undeclared { .. })),
        "an NFD glob must admit an NFC request naming the same file: {outcome:?}"
    );
    let _ = nfd;
}
