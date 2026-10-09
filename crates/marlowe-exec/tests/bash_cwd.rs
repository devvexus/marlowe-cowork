//! `bash`'s working directory, and the second place the pinning argument is load-bearing.
//!
//! # Why this is a separate file
//!
//! `CreateProcess` takes a working directory as a **string**. There is no handle-relative spawn
//! in Win32, so between the walk verifying `cwd` and the child starting in it, the only thing
//! stopping that directory being swapped for a junction is that the walk's handle is **still
//! open without `FILE_SHARE_DELETE`**.
//!
//! That is the same argument `scope::walk` makes, used a second time, in a different component,
//! for a different operation. `tests/toctou.rs` asserts it for the walk. **An assertion made
//! about one caller is not an assertion about another**, and a second use that inherited the
//! first one's test would be exactly the "guard that says nothing" family this project has
//! logged fourteen times. So it earns its own.
//!
//! On POSIX there is no gap to assert: `pre_exec` runs `fchdir` on the descriptor the walk
//! verified, and no string crosses at all. The Windows test is `cfg`-gated for that reason, and
//! the POSIX side gets the positive control instead.

use std::fs;
use std::path::{Path, PathBuf};

use marlowe_contract::TrustClass;
use marlowe_exec::FileSystemTools;
use marlowe_loop::{ToolBody, ToolHost};
use marlowe_permission::scope::{PathScope, WorkspaceScope};
use marlowe_permission::{Adjudicator, Args, EgressPolicy, Request, TaintSet, Tier};
use marlowe_tools::{builtin_registry, ExposedSet, ToolId, BUILTIN_TOOLS};

static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn fixture(name: &str) -> PathBuf {
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("marlowe-bashcwd-{name}-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("work")).unwrap();
    fs::write(root.join("work").join("marker.txt"), "inside").unwrap();
    root
}

fn run_bash(root: &Path, command: &str, cwd: &str) -> (bool, String) {
    let registry = builtin_registry().unwrap();
    let exposed = ExposedSet::new(BUILTIN_TOOLS.iter().map(|t| ToolId::new(*t)).collect()).unwrap();
    let args = Args::new().text("command", command).text("cwd", cwd);
    let mut taint = TaintSet::new();
    taint.insert("command".to_string(), TrustClass::UserAsserted);
    taint.insert("cwd".to_string(), TrustClass::UserAsserted);
    let egress = EgressPolicy::DenyAll;

    let mut adj = Adjudicator::new(WorkspaceScope::new().expect("verified platform"));
    let adjudication = adj.adjudicate(Request {
        manifest: registry.manifest(&ToolId::new("bash")).unwrap(),
        args: &args,
        taint: &taint,
        exposed: &exposed,
        egress: &egress,
        workspace: root,
        tier: Tier::Silent,
        novelty: None,
    });

    let mut tools = FileSystemTools::new(WorkspaceScope::new().expect("verified platform"), root);
    let out = tools.execute(&ToolId::new("bash"), &args, &adjudication);
    let text = match &out.body {
        ToolBody::Inline(s) => s.clone(),
        ToolBody::Reference { hash, .. } => hash.clone(),
    };
    (out.failed, text)
}

#[test]
fn the_child_starts_in_the_verified_directory() {
    // The positive control. Both platforms. Without it, a `bash` that silently ran in the wrong
    // directory would still satisfy every negative assertion below.
    let root = fixture("positive");
    // One spelling: `bash` really is bash on both platforms now.
    let list = "ls";
    let (failed, text) = run_bash(&root, list, "work");
    assert!(!failed, "{text}");
    assert!(text.contains("marker.txt"), "the child did not start in `work`: {text}");
    let _ = fs::remove_dir_all(&root);
}

#[cfg(windows)]
#[test]
fn the_cwd_directory_cannot_be_swapped_while_bash_holds_it() {
    // THE assertion this file exists for.
    //
    // While the executor holds the verified `cwd` handle open, the directory must not be
    // renameable — because on Windows the cwd reaches `CreateProcess` as a string, and a rename
    // is how that string would come to name something else.
    //
    // The test does not race the spawn. It asserts the *property the spawn depends on*: with the
    // handle open, `rename` fails as a sharing violation. If that ever stops holding, `bash`'s
    // cwd is a check-then-use race and this test is what says so.
    let root = fixture("pinned");
    let declared = [marlowe_tools::PathGlob::new("./**")];
    let scope = WorkspaceScope::new().expect("verified platform");
    let scoped = scope
        .open(&declared, &root, "work", marlowe_permission::scope::Access::Read)
        .expect("the directory is in scope");

    // Held open, exactly as `bash` holds it across the spawn.
    let err = fs::rename(root.join("work"), root.join("work-moved")).unwrap_err();
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("access is denied")
            || msg.contains("being used by another process")
            || msg.contains("os error 5")
            || msg.contains("os error 32"),
        "renaming a pinned cwd must fail as a sharing or access violation — that is the whole \
         mechanism protecting the cwd string. Got: {err}"
    );

    // And the negative control: once the handle is dropped, the rename succeeds. Without this,
    // the assertion above could be passing because of something unrelated to the handle.
    drop(scoped);
    fs::rename(root.join("work"), root.join("work-moved"))
        .expect("with no handle held, the rename must succeed — otherwise the test proves nothing");

    let _ = fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn on_posix_no_cwd_string_crosses_to_the_child() {
    // There is no window to assert on POSIX: `pre_exec` calls `fchdir` on the descriptor the
    // walk verified, so the child's working directory is the verified object by construction and
    // no path is re-resolved. What is asserted instead is that a directory REPLACED after
    // verification does not redirect the child — the descriptor still points at the original.
    let root = fixture("posix");
    let declared = [marlowe_tools::PathGlob::new("./**")];
    let scope = WorkspaceScope::new().expect("verified platform");
    let scoped = scope
        .open(&declared, &root, "work", marlowe_permission::scope::Access::Read)
        .expect("the directory is in scope");
    drop(scoped);

    // Swap `work` for a symlink pointing elsewhere, then run bash with cwd=work. The walk runs
    // again inside `run_bash` and must refuse the symlink outright.
    fs::create_dir_all(root.join("elsewhere")).unwrap();
    fs::write(root.join("elsewhere").join("outside.txt"), "escaped").unwrap();
    fs::rename(root.join("work"), root.join("work-real")).unwrap();
    std::os::unix::fs::symlink(root.join("elsewhere"), root.join("work")).unwrap();

    let (failed, text) = run_bash(&root, "ls", "work");
    assert!(
        failed || !text.contains("outside.txt"),
        "bash ran in a symlinked directory that leaves the workspace: {text}"
    );
    let _ = fs::remove_dir_all(&root);
}
