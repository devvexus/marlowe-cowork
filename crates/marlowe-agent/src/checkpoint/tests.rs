//! The shadow repository against real git on temp directories: a rewind is byte for byte, the
//! user's repository is never written, and the ignored/outside-the-workspace path works.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use marlowe_os::child::{run, ChildSpec, RunLimits};
use marlowe_proto::SessionId;

use super::*;

fn tmp(name: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!("marlowe-ckpt-{name}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// Plain git in `dir` (a fixture's own user repository), through the harness spawner.
fn git(dir: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    cmd.args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"]).args(args);
    let spec = ChildSpec::new().with_cwd(dir);
    let out = run(cmd, &spec, RunLimits { timeout: Duration::from_secs(60), max_output_bytes: 1 << 20 }, &|| false).unwrap();
    assert_eq!(out.code(), 0, "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn put(root: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, bytes).unwrap();
    p
}

/// Every file under `root` except the user's `.git`, by relative path.
fn fingerprint(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for e in fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.file_name().is_some_and(|n| n == ".git") {
                continue;
            }
            if p.is_dir() {
                walk(root, &p, out);
            } else {
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
                out.insert(rel, fs::read(&p).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// Number of files and bytes under the user's `.git`, and its refs: "the user repository is
/// unchanged".
fn repo_census(root: &Path) -> (usize, u64, String) {
    fn walk(dir: &Path, n: &mut usize, bytes: &mut u64) {
        for e in fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, n, bytes);
            } else {
                *n += 1;
                *bytes += fs::metadata(&p).unwrap().len();
            }
        }
    }
    let (mut n, mut bytes) = (0, 0);
    walk(&root.join(".git"), &mut n, &mut bytes);
    (n, bytes, git(root, &["for-each-ref"]))
}

fn sid(s: &str) -> SessionId {
    SessionId(s.into())
}

fn warn_sink() -> (Warn, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    (Arc::new(move |_, m| s.lock().unwrap().push(m)), seen)
}

/// A workspace that is a user git repository with tracked, untracked and ignored files.
fn project(name: &str) -> (PathBuf, PathBuf) {
    let base = tmp(name);
    let ws = base.join("proj");
    fs::create_dir_all(&ws).unwrap();
    git(&ws, &["init", "-q"]);
    put(&ws, ".gitignore", b"out/\n*.log\n");
    put(&ws, "src/main.rs", b"fn main() {}\r\n");
    put(&ws, "src/old.rs", b"// to be deleted\n");
    put(&ws, "notes.txt", b"first\n");
    git(&ws, &["add", "-A"]);
    git(&ws, &["commit", "-q", "-m", "init"]);
    put(&ws, "scratch/untracked.txt", b"not in git\n");
    put(&ws, "out/build.log", b"build v0\n");
    (base, ws)
}

#[test]
fn a_rewind_restores_tracked_untracked_and_ignored_files_byte_for_byte() {
    let (base, ws) = project("bytes");
    let store = CheckpointStore::new(base.join("home").join("checkpoints"));
    let repo = store.repo(&ws).unwrap();
    let s = sid("sess-1");
    let census0 = repo_census(&ws);
    let state0 = fingerprint(&ws);

    // Turn 1: edit a tracked file, create one, delete one, write an ignored log.
    let st = repo
        .begin_turn(
            &s,
            1,
            &Mutation::Paths(vec![ws.join("src/main.rs"), ws.join("src/new.rs"), ws.join("src/old.rs"), ws.join("out/build.log")]),
        )
        .unwrap();
    assert!(st.snapshotted, "the first change of a turn takes the pre snapshot");
    assert_eq!(st.preimages, 1, "the ignored log needs a saved pre-image, the tree cannot hold it");
    fs::write(ws.join("src/main.rs"), b"fn main() { println!(\"hi\"); }\r\n").unwrap();
    put(&ws, "src/new.rs", b"pub fn new() {}\n");
    fs::remove_file(ws.join("src/old.rs")).unwrap();
    fs::write(ws.join("out/build.log"), b"build v1\n").unwrap();
    let state1 = fingerprint(&ws);

    // A second call in the same turn changes nothing about the snapshot.
    let again = repo.begin_turn(&s, 1, &Mutation::Paths(vec![ws.join("notes.txt")])).unwrap();
    assert!(!again.snapshotted);
    fs::write(ws.join("notes.txt"), b"first\nsecond (turn 1)\n").unwrap();
    let state1 = {
        let mut st = state1;
        st.insert("notes.txt".into(), b"first\nsecond (turn 1)\n".to_vec());
        st
    };

    // Turn 2: a shell command somewhere under the workspace.
    assert!(repo.begin_turn(&s, 2, &Mutation::Under(ws.clone())).unwrap().snapshotted);
    fs::remove_dir_all(ws.join("scratch")).unwrap();
    put(&ws, "deep/er/dir/made.txt", "ünïcode ✓\n".as_bytes());
    fs::write(ws.join("notes.txt"), b"rewritten\n").unwrap();
    assert_ne!(fingerprint(&ws), state1);

    let cps = repo.checkpoints(&s).unwrap();
    assert_eq!(cps.iter().map(|c| c.turn).collect::<Vec<_>>(), vec![1, 2]);

    // Rewind to the start of turn 2: exactly the end of turn 1.
    let r = repo.restore_to_turn(&s, 2).unwrap();
    assert_eq!(r.from_turn, Some(2));
    assert!(r.verified, "{r:?}");
    assert_eq!(fingerprint(&ws), state1);

    // And to the start of turn 1: exactly the beginning, ignored log and CRLF file included.
    let r = repo.restore_to_turn(&s, 1).unwrap();
    assert_eq!(r.from_turn, Some(1));
    assert!(r.verified && r.undo_ref.is_some(), "{r:?}");
    assert_eq!(r.preimages.restored, 1);
    assert_eq!(fingerprint(&ws), state0);

    // The user's repository was only ever read.
    assert_eq!(repo_census(&ws), census0, "no object, ref or file in the user's .git changed");
    assert!(git(&ws, &["status", "--porcelain"]).contains("scratch/"), "their untracked file is still untracked");

    // A rewind to a turn with nothing at or after it is a no-op.
    let r = repo.restore_to_turn(&s, 9).unwrap();
    assert_eq!((r.from_turn, r.restored, r.deleted), (None, 0, 0));
    assert_eq!(fingerprint(&ws), state0);
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn a_rewind_can_itself_be_undone() {
    let (base, ws) = project("undo");
    let store = CheckpointStore::new(base.join("cp"));
    let repo = store.repo(&ws).unwrap();
    let s = sid("undo");
    let before = fingerprint(&ws);
    repo.begin_turn(&s, 0, &Mutation::Under(ws.clone())).unwrap();
    put(&ws, "src/feature.rs", b"work worth keeping\n");
    let after = fingerprint(&ws);
    let r = repo.restore_to_turn(&s, 0).unwrap();
    assert_eq!(fingerprint(&ws), before);
    // The state before the rewind is a ref: its tree equals what the work looked like.
    let undo = r.undo_ref.unwrap();
    let tree = git_in_shadow(&repo, &["rev-parse", &format!("{undo}^{{tree}}")]);
    let files = git_in_shadow(&repo, &["ls-tree", "-r", "--name-only", tree.trim()]);
    assert!(files.contains("src/feature.rs"));
    assert!(after.contains_key("src/feature.rs"));
    let _ = fs::remove_dir_all(&base);
}

fn git_in_shadow(repo: &ShadowRepo, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    cmd.arg("--git-dir").arg(repo.git_dir()).args(args);
    let out = run(cmd, &ChildSpec::new(), RunLimits { timeout: Duration::from_secs(60), max_output_bytes: 1 << 20 }, &|| false).unwrap();
    assert_eq!(out.code(), 0, "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn files_outside_the_workspace_are_saved_and_restored_too() {
    let base = tmp("outside");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).unwrap();
    put(&ws, "a.txt", b"a\n");
    let elsewhere = put(&base, "elsewhere/config.toml", b"v0\n");
    let store = CheckpointStore::new(base.join("cp"));
    let repo = store.repo(&ws).unwrap();
    let s = sid("out");
    let st = repo.begin_turn(&s, 4, &Mutation::Paths(vec![elsewhere.clone(), base.join("elsewhere/new.toml")])).unwrap();
    assert!(!st.snapshotted, "nothing in the tree changes");
    assert_eq!(st.preimages, 2);
    fs::write(&elsewhere, b"v1\n").unwrap();
    put(&base, "elsewhere/new.toml", b"created\n");
    let r = repo.restore_to_turn(&s, 4).unwrap();
    assert_eq!((r.preimages.restored, r.preimages.deleted), (1, 1));
    assert_eq!(fs::read(&elsewhere).unwrap(), b"v0\n");
    assert!(!base.join("elsewhere/new.toml").exists());
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn a_directory_that_is_not_a_git_repository_works_the_same() {
    let base = tmp("plain");
    let ws = base.join("notes");
    put(&ws, "todo.md", b"- one\n");
    put(&ws, "sub/two.md", b"two\n");
    let store = CheckpointStore::new(base.join("cp"));
    let repo = store.repo(&ws).unwrap();
    let s = sid("plain");
    let before = fingerprint(&ws);
    repo.begin_turn(&s, 0, &Mutation::Paths(vec![ws.join("todo.md")])).unwrap();
    fs::write(ws.join("todo.md"), b"- one\n- two\n").unwrap();
    assert!(repo.restore_to_turn(&s, 0).unwrap().verified);
    assert_eq!(fingerprint(&ws), before);
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn the_diff_is_the_shadow_trees_diff() {
    let (base, ws) = project("diff");
    let store = CheckpointStore::new(base.join("cp"));
    let repo = store.repo(&ws).unwrap();
    let s = sid("diff");
    repo.begin_turn(&s, 2, &Mutation::Paths(vec![ws.join("notes.txt")])).unwrap();
    fs::write(ws.join("notes.txt"), b"first\nadded line\n").unwrap();
    let pre = repo.checkpoints(&s).unwrap().remove(0);
    let now = repo.current_tree().unwrap();
    let diff = repo.diff(&pre.tree, &now).unwrap();
    assert_eq!(
        diff.lines().filter(|l| !l.starts_with("index ")).collect::<Vec<_>>(),
        vec!["diff --git a/notes.txt b/notes.txt", "--- a/notes.txt", "+++ b/notes.txt", "@@ -1 +1,2 @@", " first", "+added line"]
    );
    assert_eq!(repo.changed(&pre.tree, &now).unwrap(), vec!["notes.txt".to_string()]);
    assert!(repo.diff(&now, &now).unwrap().is_empty());
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn oversized_untracked_files_stay_out_of_the_snapshot_with_a_warning() {
    let (base, ws) = project("big");
    let store = CheckpointStore::new(base.join("cp"));
    let repo = store.repo(&ws).unwrap();
    let big = ws.join("data").join("blob.bin");
    fs::create_dir_all(big.parent().unwrap()).unwrap();
    let f = fs::File::create(&big).unwrap();
    f.set_len(MAX_SNAPSHOT_FILE_BYTES + 1).unwrap();
    drop(f);
    let st = repo.begin_turn(&sid("big"), 0, &Mutation::Under(ws.clone())).unwrap();
    assert!(st.warnings.iter().any(|w| w.contains("data/blob.bin")), "{:?}", st.warnings);
    let cp = repo.checkpoints(&sid("big")).unwrap().remove(0);
    let listing = git_in_shadow(&repo, &["ls-tree", "-r", "--name-only", &cp.tree]);
    assert!(listing.contains("notes.txt") && !listing.contains("blob.bin"), "{listing}");
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn two_threads_in_one_turn_take_one_snapshot_before_either_proceeds() {
    let (base, ws) = project("race");
    let store = Arc::new(CheckpointStore::new(base.join("cp")));
    let s = sid("race");
    let snapped = Arc::new(AtomicUsize::new(0));
    std::thread::scope(|scope| {
        for i in 0..4 {
            let (store, ws, s, snapped) = (store.clone(), ws.clone(), s.clone(), snapped.clone());
            scope.spawn(move || {
                let repo = store.repo(&ws).unwrap();
                let m = Mutation::Paths(vec![ws.join(format!("par{i}.txt"))]);
                if repo.begin_turn(&s, 7, &m).unwrap().snapshotted {
                    snapped.fetch_add(1, Ordering::SeqCst);
                }
                // The "tool" runs only after its pre_mutation returned.
                put(&ws, &format!("par{i}.txt"), b"x");
            });
        }
    });
    assert_eq!(snapped.load(Ordering::SeqCst), 1);
    let repo = store.repo(&ws).unwrap();
    let pre = repo.checkpoints(&s).unwrap().remove(0);
    let listing = git_in_shadow(&repo, &["ls-tree", "-r", "--name-only", &pre.tree]);
    assert!(!listing.contains("par"), "no parallel edit leaked into the pre state: {listing}");
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn discard_archives_and_fork_copies() {
    let (base, ws) = project("fork");
    let store = CheckpointStore::new(base.join("cp"));
    let repo = store.repo(&ws).unwrap();
    let a = sid("a");
    for t in [0, 1, 2] {
        repo.begin_turn(&a, t, &Mutation::Under(ws.clone())).unwrap();
        put(&ws, &format!("t{t}.txt"), b"x");
    }
    repo.fork(&a, &sid("b"), 2).unwrap();
    assert_eq!(repo.checkpoints(&sid("b")).unwrap().iter().map(|c| c.turn).collect::<Vec<_>>(), vec![0, 1]);
    repo.discard_from(&a, 1, true).unwrap();
    assert_eq!(repo.checkpoints(&a).unwrap().iter().map(|c| c.turn).collect::<Vec<_>>(), vec![0]);
    let refs = git_in_shadow(&repo, &["for-each-ref", "--format=%(refname)", "refs/marlowe/a/rewound/"]);
    assert_eq!(refs.lines().count(), 2, "the dropped pre refs stay reachable: {refs}");
    // The turn can be taken again after a discard.
    assert!(repo.begin_turn(&a, 1, &Mutation::Under(ws.clone())).unwrap().snapshotted);
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn prune_drops_old_sessions_and_keeps_recent_ones() {
    let (base, ws) = project("prune");
    let store = CheckpointStore::new(base.join("cp"));
    let repo = store.repo(&ws).unwrap();
    repo.begin_turn(&sid("fresh"), 0, &Mutation::Under(ws.clone())).unwrap();
    assert!(repo.prune(60_000).unwrap().is_empty());
    // Everything is "old" against a negative age.
    assert_eq!(repo.prune(-1_000).unwrap(), vec!["fresh".to_string()]);
    assert!(repo.checkpoints(&sid("fresh")).unwrap().is_empty());
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn the_middleware_snapshots_in_pre_mutation_and_warns_once_when_it_cannot() {
    use crate::tools::ToolCtx;
    use marlowe_model::CancelToken;
    let (base, ws) = project("mw");
    let store = Arc::new(CheckpointStore::new(base.join("cp")));
    let (warn, seen) = warn_sink();
    let mw = CheckpointMiddleware::new(store.clone(), warn.clone());
    let s = sid("mw");
    let call = ToolCall { id: "c1".into(), name: "write".into(), args: serde_json::json!({}) };
    let cx = |workspace: Option<PathBuf>, turn| {
        CallCtx::detached(ToolCtx::new(s.clone(), turn, workspace, CancelToken::new(), false))
    };
    mw.pre_mutation(&call, &cx(Some(ws.clone()), 3), &Mutation::Paths(vec![ws.join("notes.txt")])).unwrap();
    assert_eq!(store.repo(&ws).unwrap().checkpoints(&s).unwrap().len(), 1);
    // No workspace: nothing to snapshot, nothing said.
    mw.pre_mutation(&call, &cx(None, 3), &Mutation::Paths(vec![PathBuf::from("x")])).unwrap();
    assert!(seen.lock().unwrap().is_empty());
    // A workspace that does not exist: the edit is still allowed, the user is told once.
    let gone = base.join("not-there");
    for _ in 0..3 {
        mw.pre_mutation(&call, &cx(Some(gone.clone()), 4), &Mutation::Paths(vec![gone.join("f")])).unwrap();
    }
    let warnings = seen.lock().unwrap().clone();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("cannot be rewound"));
    let _ = fs::remove_dir_all(&base);
}

/// A workspace whose `.gitignore` hides `.env`, with a turn that removes that line.
fn gitignore_edit(name: &str, second_turn: bool) {
    let base = tmp(name);
    let ws = base.join("ws");
    put(&ws, ".gitignore", b".env\n");
    put(&ws, ".env", b"SECRET=1\n");
    put(&ws, "a.txt", b"a\n");
    let store = CheckpointStore::new(base.join("cp"));
    let repo = store.repo(&ws).unwrap();
    let s = sid(name);
    let before = fingerprint(&ws);

    repo.begin_turn(&s, 1, &Mutation::Paths(vec![ws.join(".gitignore")])).unwrap();
    fs::write(ws.join(".gitignore"), b"").unwrap();
    repo.end_turn(&s, 1).unwrap();
    if second_turn {
        // The shadow index now holds `.env`, and turn 2's snapshot lists it.
        repo.begin_turn(&s, 2, &Mutation::Paths(vec![ws.join("a.txt")])).unwrap();
        fs::write(ws.join("a.txt"), b"changed\n").unwrap();
        repo.end_turn(&s, 2).unwrap();
    }

    let r = repo.restore_to_turn(&s, 1).unwrap();
    assert!(r.verified, "{r:?}");
    assert_eq!(fingerprint(&ws), before, "the ignored .env survived and the ignore file is back");
    assert_eq!(r.deleted, 0, "{r:?}");
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn a_turn_that_drops_a_gitignore_line_cannot_make_a_rewind_delete_the_ignored_file() {
    gitignore_edit("gi-one", false);
    gitignore_edit("gi-two", true);
}

#[cfg(windows)]
#[test]
fn ignore_rules_follow_the_case_rules_git_detected_on_this_file_system() {
    let base = tmp("case");
    let ws = base.join("ws");
    put(&ws, ".gitignore", b"Build/\n");
    put(&ws, "build/out.bin", b"x");
    put(&ws, "Foo.txt", b"f");
    let store = CheckpointStore::new(base.join("cp"));
    let repo = store.repo(&ws).unwrap();
    let s = sid("case");
    repo.begin_turn(&s, 0, &Mutation::Under(ws.clone())).unwrap();
    let pre = repo.checkpoints(&s).unwrap().remove(0);
    let listing = git_in_shadow(&repo, &["ls-tree", "-r", "--name-only", &pre.tree]);
    assert!(listing.contains("Foo.txt") && !listing.contains("out.bin"), "Build/ ignores build/: {listing}");
    // A case-only rename leaves one entry, not two that would collide on NTFS.
    fs::rename(ws.join("Foo.txt"), ws.join("foo.txt")).unwrap();
    let now = repo.current_tree().unwrap();
    let listing = git_in_shadow(&repo, &["ls-tree", "-r", "--name-only", &now]);
    assert_eq!(listing.lines().filter(|l| l.eq_ignore_ascii_case("foo.txt")).count(), 1, "{listing}");
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn a_workspace_inside_a_repository_honours_the_ignore_files_above_it() {
    let base = tmp("sub");
    let root = base.join("repo");
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q"]);
    put(&root, ".gitignore", b"node_modules/\n");
    put(&root, "other.txt", b"outside the workspace\n");
    let ws = root.join("frontend");
    put(&ws, "src/a.js", b"a\n");
    put(&ws, "node_modules/dep/index.js", b"ignored only at the repository root\n");
    let store = CheckpointStore::new(base.join("cp"));
    let repo = store.repo(&ws).unwrap();
    let s = sid("sub");
    let before = fingerprint(&ws);

    repo.begin_turn(&s, 0, &Mutation::Paths(vec![ws.join("src/a.js")])).unwrap();
    let pre = repo.checkpoints(&s).unwrap().remove(0);
    let listing = git_in_shadow(&repo, &["ls-tree", "-r", "--name-only", &pre.tree]);
    assert!(listing.contains("src/a.js"), "{listing}");
    assert!(!listing.contains("node_modules") && !listing.contains("other.txt"), "{listing}");

    fs::write(ws.join("src/a.js"), b"b\n").unwrap();
    put(&ws, "src/new.js", b"new\n");
    let now = repo.current_tree().unwrap();
    assert_eq!(repo.changed(&pre.tree, &now).unwrap(), vec!["src/a.js".to_string(), "src/new.js".to_string()], "paths are relative to the workspace");
    assert!(repo.diff(&pre.tree, &now).unwrap().contains("--- a/src/a.js"));

    let r = repo.restore_to_turn(&s, 0).unwrap();
    assert!(r.verified && r.kept.is_empty(), "{r:?}");
    assert_eq!(fingerprint(&ws), before);
    assert_eq!(fs::read(root.join("other.txt")).unwrap(), b"outside the workspace\n");
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn a_rewind_leaves_alone_what_changed_outside_the_sessions_turns() {
    let base = tmp("foreign");
    let ws = base.join("ws");
    put(&ws, "a.txt", b"a0\n");
    put(&ws, "b.txt", b"b0\n");
    put(&ws, "c.txt", b"c0\n");
    let store = CheckpointStore::new(base.join("cp"));
    let repo = store.repo(&ws).unwrap();
    let s = sid("foreign");

    // Turn 1 edits a.txt and creates made.txt.
    repo.begin_turn(&s, 1, &Mutation::Paths(vec![ws.join("a.txt"), ws.join("made.txt")])).unwrap();
    fs::write(ws.join("a.txt"), b"a1\n").unwrap();
    put(&ws, "made.txt", b"m\n");
    assert!(repo.end_turn(&s, 1).unwrap());
    // The user, between turns: a new file, an edit of a file no turn touched, and an edit of a.txt.
    fs::write(ws.join("c.txt"), b"c-by-hand\n").unwrap();
    fs::write(ws.join("a.txt"), b"a-by-hand\n").unwrap();
    // Turn 2 edits b.txt.
    repo.begin_turn(&s, 2, &Mutation::Paths(vec![ws.join("b.txt")])).unwrap();
    fs::write(ws.join("b.txt"), b"b1\n").unwrap();
    assert!(repo.end_turn(&s, 2).unwrap());
    // And after the session went quiet: a file made in the editor, and one the turn made, edited.
    put(&ws, "notes-from-the-editor.txt", b"mine\n");
    fs::write(ws.join("made.txt"), b"m-by-hand\n").unwrap();

    let r = repo.restore_to_turn(&s, 1).unwrap();
    assert!(r.verified, "{r:?}");
    let read = |f: &str| fs::read_to_string(ws.join(f)).unwrap();
    assert_eq!(read("b.txt"), "b0\n", "the session's own edit is put back");
    assert_eq!(read("a.txt"), "a-by-hand\n", "edited again outside the turns, so it stays");
    assert_eq!(read("c.txt"), "c-by-hand\n");
    assert_eq!(read("notes-from-the-editor.txt"), "mine\n");
    assert_eq!(read("made.txt"), "m-by-hand\n");
    assert_eq!(r.kept, vec!["a.txt", "c.txt", "made.txt", "notes-from-the-editor.txt"], "{r:?}");
    // The state before is kept for the user to take back.
    let undo = r.undo_ref.unwrap();
    let tree = git_in_shadow(&repo, &["rev-parse", &format!("{undo}^{{tree}}")]);
    assert!(git_in_shadow(&repo, &["ls-tree", "-r", "--name-only", tree.trim()]).contains("notes-from-the-editor.txt"));
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn after_a_conversation_rewind_the_dropped_turns_still_count_as_the_sessions_own() {
    let base = tmp("archived");
    let ws = base.join("ws");
    put(&ws, "a.txt", b"a0\n");
    put(&ws, "b.txt", b"b0\n");
    put(&ws, "ignored.log", b"v0\n");
    put(&ws, ".gitignore", b"*.log\n");
    let store = CheckpointStore::new(base.join("cp"));
    let repo = store.repo(&ws).unwrap();
    let s = sid("archived");
    repo.begin_turn(&s, 0, &Mutation::Paths(vec![ws.join("a.txt")])).unwrap();
    fs::write(ws.join("a.txt"), b"a1\n").unwrap();
    repo.end_turn(&s, 0).unwrap();
    // Turn 1 is dropped from the conversation but its effects stay on disk.
    repo.begin_turn(&s, 1, &Mutation::Paths(vec![ws.join("b.txt"), ws.join("ignored.log")])).unwrap();
    fs::write(ws.join("b.txt"), b"b1\n").unwrap();
    fs::write(ws.join("ignored.log"), b"v1\n").unwrap();
    repo.end_turn(&s, 1).unwrap();
    repo.discard_from(&s, 1, true).unwrap();
    std::thread::sleep(Duration::from_millis(5));
    let began = super::shadow::now_ms();
    // A reused turn 1 changes the ignored file again, then the user edits a file outside the session.
    repo.begin_turn(&s, 1, &Mutation::Paths(vec![ws.join("ignored.log")])).unwrap();
    fs::write(ws.join("ignored.log"), b"v2\n").unwrap();
    repo.end_turn(&s, 1).unwrap();
    put(&ws, "by-hand.txt", b"mine\n");

    // Back to the start of the new turn 1: the ignored file is what the new turn found.
    let r = repo.restore_to_turn_at(&s, 1, Some(began)).unwrap();
    assert!(r.verified, "{r:?}");
    assert_eq!(fs::read_to_string(ws.join("ignored.log")).unwrap(), "v1\n");
    assert_eq!(fs::read_to_string(ws.join("b.txt")).unwrap(), "b1\n");
    // Back to the start of turn 0: the dropped turn 1 is undone too; the user's file is not.
    let r = repo.restore_to_turn_at(&s, 0, Some(0)).unwrap();
    assert!(r.verified, "{r:?}");
    assert_eq!(r.kept, vec!["by-hand.txt"], "{r:?}");
    assert_eq!(fs::read_to_string(ws.join("a.txt")).unwrap(), "a0\n");
    assert_eq!(fs::read_to_string(ws.join("b.txt")).unwrap(), "b0\n");
    assert_eq!(fs::read_to_string(ws.join("ignored.log")).unwrap(), "v0\n");
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn a_turn_without_an_end_is_all_the_sessions_own() {
    // The daemon was killed mid-turn: no `post`, so the turn's changes are the session's.
    let base = tmp("nopost");
    let ws = base.join("ws");
    put(&ws, "a.txt", b"a0\n");
    let store = CheckpointStore::new(base.join("cp"));
    let repo = store.repo(&ws).unwrap();
    let s = sid("nopost");
    assert!(!repo.end_turn(&s, 0).unwrap(), "no pre, no post");
    repo.begin_turn(&s, 0, &Mutation::Under(ws.clone())).unwrap();
    fs::write(ws.join("a.txt"), b"a1\n").unwrap();
    put(&ws, "new.txt", b"n\n");
    let r = repo.restore_to_turn(&s, 0).unwrap();
    assert!(r.verified && r.kept.is_empty(), "{r:?}");
    assert_eq!(fs::read_to_string(ws.join("a.txt")).unwrap(), "a0\n");
    assert!(!ws.join("new.txt").exists());
    let _ = fs::remove_dir_all(&base);
}
