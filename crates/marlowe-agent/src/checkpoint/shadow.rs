//! The shadow repository: a bare git directory under `<home>\checkpoints` whose work tree is the
//! session's workspace. The user's own repository is never opened, written or linked (no
//! alternates: a `gc` there could drop objects the shadow still reads); its `info/exclude` is
//! only read. Git's own ignore rules (`.gitignore`, `info/exclude`, the global excludes) decide
//! what a snapshot holds; line-ending, filter and encoding conversion are switched off so a
//! restore is byte for byte.
//!
//! The work tree is the workspace, or the user repository's root when the workspace lies inside
//! one (so `.gitignore` files above the workspace apply); git then runs in the workspace, which
//! limits every snapshot, diff and restore to it. The root is recorded at creation in `top.txt`.
//!
//! Refs, per session `S` and turn `T`:
//! - `refs/marlowe/S/T/pre`: the workspace as it was when `T` first changed a file;
//! - `refs/marlowe/S/T/post`: the workspace when `T` ended; with the next `pre` it tells what
//!   changed outside the session's turns, which a rewind leaves alone;
//! - `refs/marlowe/S/undo/<ms>`: the workspace as it was just before a rewind put an older state
//!   back, so a rewind is itself undoable (until pruned);
//! - `refs/marlowe/S/rewound/<ms>/T/pre`: the `pre` refs of turns a conversation rewind dropped.
//!
//! Every snapshot is a flat commit (no parents) whose subject line is a JSON record
//! (`kind, session, turn, at_ms`), so the ledger is the refs themselves and survives any crash.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use marlowe_model::CancelToken;
use marlowe_proto::SessionId;
use serde_json::json;

use super::git::{bare, Git};
use super::paths::{canonical, ignore_line, lexical_clean, ref_part, relative_to, repo_stem, strip_verbatim};
use super::preimage::{remove_file, PreimageReport, Preimages};
use super::CheckpointError;
use crate::tools::Mutation;

/// Untracked files bigger than this stay out of snapshots (datasets, checkpoints, build blobs
/// nobody ignored). A rewind cannot restore them.
pub const MAX_SNAPSHOT_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// The shadow repository grows past this and the daemon warns; pruning is by age.
pub const SIZE_WARN_KIB: u64 = 2 * 1024 * 1024;

/// A workspace with more untracked, non-ignored files than this (a home directory, a drive root)
/// is not snapshotted: `add -A` over it would take minutes and fill the disk.
pub const MAX_SNAPSHOT_FILES: usize = 200_000;

/// ... or more bytes than this.
pub const MAX_SNAPSHOT_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Diffs longer than this are cut (a `/diff` of a vendored dependency drop).
pub const MAX_DIFF_BYTES: usize = 2 * 1024 * 1024;

const CONFIG: &str = "[core]
\trepositoryformatversion = 0
\tbare = true
\tlogallrefupdates = false
\tautocrlf = false
\tsafecrlf = false
\tlongpaths = true
\tfsmonitor = false
\tquotepath = false
\tuntrackedCache = false
[gc]
\tauto = 0
[commit]
\tgpgsign = false
[user]
\tname = Marlowe checkpoints
\temail = checkpoints@marlowe.invalid
[advice]
\taddIgnoredFile = false
\taddEmbeddedRepo = false
";

const ATTRIBUTES: &str = "* -text -filter -ident -working-tree-encoding\n";

/// One `pre` snapshot of a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnCheckpoint {
    pub turn: u32,
    pub at_ms: i64,
    pub commit: String,
    pub tree: String,
}

/// What a snapshot call did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnState {
    /// A `pre` snapshot was taken by this call.
    pub snapshotted: bool,
    /// Files saved as pre-images by this call.
    pub preimages: usize,
    /// Files the snapshot could not hold (too large, or git could not read them).
    pub warnings: Vec<String>,
}

/// What a rewind of the working tree did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreReport {
    /// The turn whose `pre` state was put back (the earliest turn at or after the one asked for).
    pub from_turn: Option<u32>,
    pub restored: usize,
    pub deleted: usize,
    /// Paths left as they are because they changed outside this session's turns (the user's
    /// editor, another session) or were never theirs to put back.
    pub kept: Vec<String>,
    /// The working tree equals the target state after the restore, apart from `kept`.
    pub verified: bool,
    /// The ref holding the state just before the rewind.
    pub undo_ref: Option<String>,
    pub preimages: PreimageReport,
}

/// One session's shadow repository access for one workspace.
pub struct ShadowRepo {
    git_dir: PathBuf,
    workspace: PathBuf,
    /// The git work tree: `workspace`, or the user repository root above it.
    top: PathBuf,
    /// The workspace as the session spelled it (a junction or symlink may differ from the
    /// canonical form); tool paths are matched against both.
    given: PathBuf,
    preimages: Preimages,
    ops: Mutex<HashSet<(String, u32)>>,
    /// When a snapshot last found the workspace too large: asking again would walk it again.
    too_large: Mutex<Option<std::time::Instant>>,
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl ShadowRepo {
    /// Opens the shadow repository for `workspace` under `root`, creating it on first use.
    pub fn open(root: &Path, workspace: &Path) -> Result<Self, CheckpointError> {
        let repo = Self::locate(root, workspace)?;
        repo.init()?;
        Ok(repo)
    }

    /// The repository for `workspace` if one exists; never creates one.
    pub fn open_existing(root: &Path, workspace: &Path) -> Result<Option<Self>, CheckpointError> {
        let repo = Self::locate(root, workspace)?;
        Ok(repo.git_dir.join("HEAD").is_file().then_some(repo))
    }

    fn locate(root: &Path, workspace: &Path) -> Result<Self, CheckpointError> {
        let given = lexical_clean(&strip_verbatim(workspace));
        let workspace = canonical(workspace);
        if !workspace.is_dir() {
            return Err(CheckpointError::Workspace(format!("{} is not a directory", workspace.display())));
        }
        let stem = repo_stem(&workspace);
        let git_dir = root.join(format!("{stem}.git"));
        // A repository keeps the root it was created with: its trees are prefixed by the path
        // from there to the workspace, and a `git init` later must not change that.
        let recorded = fs::read_to_string(git_dir.join("top.txt"))
            .ok()
            .map(|t| PathBuf::from(t.trim()))
            .filter(|t| relative_to(t, &workspace).is_some());
        let top = if git_dir.join("HEAD").is_file() && !git_dir.join("top.txt").is_file() {
            workspace.clone() // created before the root was recorded
        } else {
            recorded.or_else(|| find_user_repo(&workspace).map(|(top, _)| top)).unwrap_or_else(|| workspace.clone())
        };
        Ok(Self {
            git_dir,
            preimages: Preimages::new(root.join(format!("{stem}.pre"))),
            workspace,
            top,
            given,
            ops: Mutex::new(HashSet::new()),
            too_large: Mutex::new(None),
        })
    }

    /// The repository at `git_dir` (`<stem>.git` under `root`) for a workspace that is gone:
    /// deleted, moved or renamed. Only its history can be read, so it is good for pruning.
    pub(crate) fn orphan(root: &Path, git_dir: PathBuf, workspace: PathBuf) -> Option<Self> {
        if !git_dir.join("HEAD").is_file() {
            return None;
        }
        let stem = git_dir.file_stem()?.to_string_lossy().into_owned();
        Some(Self {
            preimages: Preimages::new(root.join(format!("{stem}.pre"))),
            top: workspace.clone(),
            given: workspace.clone(),
            git_dir,
            workspace,
            ops: Mutex::new(HashSet::new()),
            too_large: Mutex::new(None),
        })
    }

    /// Prunes a repository whose workspace is gone like [`ShadowRepo::prune`], and deletes the
    /// repository and its pre-images once nothing is left to keep. Returns whether it was deleted.
    pub(crate) fn prune_orphan(&self, max_age_ms: i64) -> Result<bool, CheckpointError> {
        self.prune(max_age_ms)?;
        let _g = self.lock();
        let refs = self.git().must(&["for-each-ref", "--format=%(refname)", "refs/marlowe/"], &[])?;
        let kept = refs.text().lines().any(|l| !l.trim().is_empty()) || !self.preimages.sessions().is_empty();
        if kept {
            return Ok(false);
        }
        let _ = fs::remove_dir_all(&self.git_dir);
        self.preimages.remove_all();
        Ok(true)
    }

    #[cfg(test)]
    pub(crate) fn mark_too_large(&self) {
        *self.too_large.lock().unwrap() = Some(std::time::Instant::now());
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    fn git(&self) -> Git<'_> {
        self.git_c(None)
    }

    /// A git handle that stops its commands when `cancel` fires.
    fn git_c<'a>(&'a self, cancel: Option<&'a CancelToken>) -> Git<'a> {
        // A workspace that is gone leaves git running next to its own directory.
        let cwd = if self.workspace.is_dir() { &self.workspace } else { &self.git_dir };
        Git { git_dir: &self.git_dir, work_tree: &self.top, cwd, cancel }
    }

    fn init(&self) -> Result<(), CheckpointError> {
        if self.git_dir.join("HEAD").is_file() {
            return Ok(());
        }
        let parent = self.git_dir.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        bare(parent, &["init", "--bare", "--quiet", &self.git_dir.to_string_lossy()])?;
        // Keep what `git init` detected for this file system (`core.ignorecase`, `symlinks`,
        // `filemode`): ignore matching and case-only renames must behave as in the user's git.
        let detected = fs::read_to_string(self.git_dir.join("config")).unwrap_or_default();
        fs::write(self.git_dir.join("config"), format!("{detected}\n{CONFIG}"))?;
        fs::write(self.git_dir.join("top.txt"), self.top.to_string_lossy().as_bytes())?;
        fs::create_dir_all(self.git_dir.join("info"))?;
        fs::write(self.git_dir.join("info").join("attributes"), ATTRIBUTES)?;
        fs::write(self.git_dir.join("workspace.txt"), self.workspace.to_string_lossy().as_bytes())?;
        Ok(())
    }

    /// `path` relative to the workspace (canonical spelling, else the session's), `/` separated.
    fn rel(&self, path: &Path) -> Option<String> {
        relative_to(&self.workspace, path).or_else(|| relative_to(&self.given, path))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashSet<(String, u32)>> {
        self.ops.lock().unwrap_or_else(|p| p.into_inner())
    }

    // ---- snapshots ------------------------------------------------------------------------

    /// The user repository's `info/exclude`, found by walking up from the workspace.
    fn user_exclude(&self) -> Option<PathBuf> {
        find_user_repo(&self.workspace).map(|(_, exclude)| exclude)
    }

    fn write_exclude(&self, big: &[String]) -> Result<(), CheckpointError> {
        let mut text = String::from("# generated for each snapshot: the repository's info/exclude, then oversized files\n");
        if let Some(user) = self.user_exclude() {
            if let Ok(t) = fs::read_to_string(user) {
                text.push_str(&t);
                text.push('\n');
            }
        }
        // The listing is relative to the workspace, but git anchors `info/exclude` patterns at the
        // work tree top, which is the user's repository root when the workspace sits inside one.
        let prefix = relative_to(&self.top, &self.workspace).filter(|p| !p.is_empty());
        for rel in big {
            let anchored = prefix.as_ref().map_or_else(|| rel.clone(), |p| format!("{p}/{rel}"));
            text.push_str(&ignore_line(&anchored));
            text.push('\n');
        }
        fs::create_dir_all(self.git_dir.join("info"))?;
        fs::write(self.git_dir.join("info").join("exclude"), text)?;
        Ok(())
    }

    /// Stages the whole workspace into the shadow index and returns the tree id, plus what could
    /// not be held. Caller holds the lock.
    fn snapshot_tree(&self, warnings: &mut Vec<String>, cancel: Option<&CancelToken>) -> Result<String, CheckpointError> {
        const TOO_LARGE_FOR: std::time::Duration = std::time::Duration::from_secs(300);
        let refused = || CheckpointError::TooLarge(self.workspace.display().to_string());
        if self.too_large.lock().unwrap_or_else(|p| p.into_inner()).is_some_and(|at| at.elapsed() < TOO_LARGE_FOR) {
            return Err(refused());
        }
        let g = self.git_c(cancel);
        self.write_exclude(&[])?;
        // Untracked, non-ignored files too big to keep are excluded for this snapshot. The
        // listing is read as it is produced, so a huge tree is refused as soon as it is known to
        // be one, and an interrupt stops the walk.
        let mut big = Vec::new();
        let (mut files, mut bytes) = (0usize, 0u64);
        let mut over = false;
        g.stream_nul(&["ls-files", "-o", "--exclude-standard", "-z"], &mut |item| {
            let rel = String::from_utf8_lossy(item).into_owned();
            files += 1;
            if let Ok(m) = fs::metadata(self.workspace.join(&rel)) {
                if m.is_file() && m.len() > MAX_SNAPSHOT_FILE_BYTES {
                    warnings.push(format!("{rel} is over {} MiB and is not checkpointed", MAX_SNAPSHOT_FILE_BYTES >> 20));
                    big.push(rel);
                } else if m.is_file() {
                    bytes += m.len();
                }
            }
            over = files > MAX_SNAPSHOT_FILES || bytes > MAX_SNAPSHOT_BYTES;
            !over
        })?;
        if over {
            *self.too_large.lock().unwrap_or_else(|p| p.into_inner()) = Some(std::time::Instant::now());
            return Err(refused());
        }
        *self.too_large.lock().unwrap_or_else(|p| p.into_inner()) = None;
        if !big.is_empty() {
            self.write_exclude(&big)?;
        }
        self.drop_ignored_entries(cancel)?;
        let add = g.run(&["add", "-A", "--ignore-errors", "--", "."], &[])?;
        if !add.ok() {
            let first = add.stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
            warnings.push(format!("some files could not be read for the checkpoint: {first}"));
        }
        let tree = g.must(&["write-tree"], &[])?.text().trim().to_string();
        Ok(tree)
    }

    /// The shadow index outlives its snapshots, and git keeps tracking a file after it becomes
    /// ignored: take those out, so a tree is always "what the ignore rules of now let in".
    fn drop_ignored_entries(&self, cancel: Option<&CancelToken>) -> Result<(), CheckpointError> {
        let g = self.git_c(cancel);
        let listed = g.run(&["ls-files", "-c", "-i", "--exclude-standard", "-z"], &[])?;
        if listed.ok() && !listed.stdout.is_empty() {
            let out = g.run_stdin(&["update-index", "--force-remove", "-z", "--stdin"], &listed.stdout)?;
            if !out.ok() {
                return Err(CheckpointError::Git { args: "update-index".into(), message: out.stderr.trim().to_string() });
            }
        }
        Ok(())
    }

    fn commit(&self, tree: &str, record: serde_json::Value) -> Result<String, CheckpointError> {
        let subject = record.to_string();
        Ok(self.git().must(&["commit-tree", tree, "-m", &subject], &[])?.text().trim().to_string())
    }

    fn ref_exists(&self, name: &str) -> bool {
        self.git().run(&["rev-parse", "--verify", "-q", &format!("{name}^{{commit}}")], &[]).is_ok_and(|o| o.ok())
    }

    fn pre_ref(session: &SessionId, turn: u32) -> String {
        format!("refs/marlowe/{}/{turn}/pre", ref_part(session.as_str()))
    }

    fn post_ref(session: &SessionId, turn: u32) -> String {
        format!("refs/marlowe/{}/{turn}/post", ref_part(session.as_str()))
    }

    /// Records the workspace as `turn` ended, for a turn that took a `pre` snapshot. A turn that
    /// never reaches its end (the daemon was killed) has none.
    pub fn end_turn(&self, session: &SessionId, turn: u32) -> Result<bool, CheckpointError> {
        let _g = self.lock();
        // Saved pre-images record how their files were left, snapshot or not.
        self.preimages.close_turn(&ref_part(session.as_str()), turn);
        if !self.ref_exists(&Self::pre_ref(session, turn)) {
            return Ok(false);
        }
        let tree = self.snapshot_tree(&mut Vec::new(), None)?;
        let commit = self.commit(&tree, json!({"kind": "post", "session": session.as_str(), "turn": turn, "at_ms": now_ms()}))?;
        self.git().must(&["update-ref", &Self::post_ref(session, turn), &commit], &[])?;
        Ok(true)
    }

    /// Where `turn` left the workspace, if it ended (a running turn, or one whose daemon was
    /// killed, has no end). Edits made after the turn are not in this tree.
    pub fn post_tree(&self, session: &SessionId, turn: u32) -> Option<String> {
        self.post_trees(session).remove(&turn)
    }

    /// [`post_tree`](Self::post_tree) for every turn of the session that ended.
    pub fn post_trees(&self, session: &SessionId) -> HashMap<u32, String> {
        let _g = self.lock();
        self.posts_locked(session).unwrap_or_default()
    }

    /// The `post` trees of a session by turn.
    fn posts_locked(&self, session: &SessionId) -> Result<HashMap<u32, String>, CheckpointError> {
        let prefix = format!("refs/marlowe/{}/", ref_part(session.as_str()));
        let out = self.git().must(&["for-each-ref", "--format=%(refname)%09%(tree)", &prefix], &[])?;
        Ok(out
            .text()
            .lines()
            .filter_map(|line| {
                let (name, tree) = line.split_once('\t')?;
                let turn: u32 = name.strip_prefix(&prefix)?.strip_suffix("/post")?.parse().ok()?;
                Some((turn, tree.to_string()))
            })
            .collect())
    }

    /// Makes sure `turn` of `session` has its `pre` snapshot and that every path in `mutation`
    /// the tree cannot hold has a saved pre-image, before a tool changes anything. Idempotent per
    /// turn: the first call snapshots, later ones only save pre-images for new paths.
    pub fn begin_turn(&self, session: &SessionId, turn: u32, mutation: &Mutation) -> Result<TurnState, CheckpointError> {
        self.begin_turn_cancellable(session, turn, mutation, None)
    }

    /// [`ShadowRepo::begin_turn`] that gives up with [`CheckpointError::Cancelled`] when `cancel`
    /// fires, so an interrupt does not wait for a first snapshot of a large tree.
    pub fn begin_turn_cancellable(
        &self,
        session: &SessionId,
        turn: u32,
        mutation: &Mutation,
        cancel: Option<&CancelToken>,
    ) -> Result<TurnState, CheckpointError> {
        let mut snapped = self.lock();
        let mut state = TurnState::default();
        let (tree_change, outside) = self.classify(mutation)?;
        for path in &outside {
            if self.preimages.save(&ref_part(session.as_str()), turn, path)? {
                state.preimages += 1;
            }
        }
        let key = (session.as_str().to_string(), turn);
        if tree_change && !snapped.contains(&key) {
            let name = Self::pre_ref(session, turn);
            if !self.ref_exists(&name) {
                let tree = self.snapshot_tree(&mut state.warnings, cancel)?;
                let commit = self.commit(
                    &tree,
                    json!({"kind": "pre", "session": session.as_str(), "turn": turn, "at_ms": now_ms()}),
                )?;
                self.git().must(&["update-ref", &name, &commit], &[])?;
                state.snapshotted = true;
            }
            snapped.insert(key);
        }
        Ok(state)
    }

    /// Splits a mutation into "changes the tree" and the absolute paths that need a pre-image.
    fn classify(&self, mutation: &Mutation) -> Result<(bool, Vec<PathBuf>), CheckpointError> {
        match mutation {
            Mutation::None => Ok((false, Vec::new())),
            Mutation::Under(dir) => Ok((self.rel(dir).is_some(), Vec::new())),
            Mutation::Paths(paths) => {
                let mut inside: Vec<(PathBuf, String)> = Vec::new();
                let mut outside = Vec::new();
                for p in paths {
                    let abs = if p.is_absolute() { p.clone() } else { self.workspace.join(p) };
                    match self.rel(&abs) {
                        // `.git` is never in a snapshot, nor is anything inside a nested repository.
                        Some(rel) if rel.split('/').any(|c| c == ".git") || self.in_nested_repo(&rel) => outside.push(abs),
                        Some(rel) if !rel.is_empty() => inside.push((abs, rel)),
                        _ => outside.push(abs),
                    }
                }
                let mut tree_change = false;
                if !inside.is_empty() {
                    let ignored = self.ignored(&inside.iter().map(|(_, r)| r.as_str()).collect::<Vec<_>>())?;
                    for (abs, rel) in inside {
                        if ignored.contains(&rel) {
                            outside.push(abs);
                        } else {
                            tree_change = true;
                        }
                    }
                }
                Ok((tree_change, outside))
            }
        }
    }

    /// Whether `rel` lies inside a repository nested in the workspace (a submodule, a clone): its
    /// files are not in the tree, so they get pre-images.
    fn in_nested_repo(&self, rel: &str) -> bool {
        let parts: Vec<&str> = rel.split('/').collect();
        let mut dir = self.workspace.clone();
        for c in &parts[..parts.len().saturating_sub(1)] {
            dir.push(c);
            if dir.join(".git").exists() {
                return true;
            }
        }
        false
    }

    /// Which of `rels` (workspace-relative, `/` separated) the ignore rules exclude.
    fn ignored(&self, rels: &[&str]) -> Result<HashSet<String>, CheckpointError> {
        self.write_exclude(&[])?;
        let mut input = Vec::new();
        for r in rels {
            input.extend_from_slice(r.as_bytes());
            input.push(0);
        }
        let out = self.git().run_stdin(&["check-ignore", "-z", "--no-index", "--stdin"], &input)?;
        // Exit 1 means none matched; anything above is a real failure.
        if out.code > 1 {
            return Err(CheckpointError::Git { args: "check-ignore".into(), message: out.stderr.trim().to_string() });
        }
        Ok(out.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()).map(|p| String::from_utf8_lossy(p).into_owned()).collect())
    }

    // ---- the ledger ------------------------------------------------------------------------

    /// The session's `pre` snapshots, by turn.
    pub fn checkpoints(&self, session: &SessionId) -> Result<Vec<TurnCheckpoint>, CheckpointError> {
        let _g = self.lock();
        self.checkpoints_locked(session)
    }

    fn checkpoints_locked(&self, session: &SessionId) -> Result<Vec<TurnCheckpoint>, CheckpointError> {
        let prefix = format!("refs/marlowe/{}/", ref_part(session.as_str()));
        let out = self.git().must(
            &["for-each-ref", "--format=%(refname)%09%(objectname)%09%(tree)%09%(contents:subject)", &prefix],
            &[],
        )?;
        let mut list: Vec<TurnCheckpoint> = out
            .text()
            .lines()
            .filter_map(|line| {
                let mut f = line.splitn(4, '\t');
                let (name, commit, tree, subject) = (f.next()?, f.next()?, f.next()?, f.next()?);
                let turn: u32 = name.strip_prefix(&prefix)?.strip_suffix("/pre")?.parse().ok()?;
                let at_ms = serde_json::from_str::<serde_json::Value>(subject).ok()?.get("at_ms")?.as_i64()?;
                Some(TurnCheckpoint { turn, at_ms, commit: commit.to_string(), tree: tree.to_string() })
            })
            .collect();
        list.sort_by_key(|c| c.turn);
        Ok(list)
    }

    /// Turns that saved pre-images only (no tree change), ascending.
    pub fn preimage_turns(&self, session: &SessionId) -> Vec<u32> {
        self.preimages.turns(&ref_part(session.as_str()))
    }

    /// The tree id of the workspace right now (stages it into the shadow index).
    pub fn current_tree(&self) -> Result<String, CheckpointError> {
        let _g = self.lock();
        self.snapshot_tree(&mut Vec::new(), None)
    }

    /// A unified diff between two tree-ish ids, cut at [`MAX_DIFF_BYTES`].
    pub fn diff(&self, from: &str, to: &str) -> Result<String, CheckpointError> {
        let _g = self.lock();
        let out = self.git().must(
            &[
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--no-renames",
                "--relative",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                from,
                to,
            ],
            &[],
        )?;
        let mut text = out.text();
        if text.len() > MAX_DIFF_BYTES || out.truncated {
            let mut cut = MAX_DIFF_BYTES.min(text.len());
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            text.push_str("\n... diff cut here; it is larger than the viewer shows ...\n");
        }
        Ok(text)
    }

    /// Paths that differ between two tree-ish ids.
    pub fn changed(&self, from: &str, to: &str) -> Result<Vec<String>, CheckpointError> {
        let _g = self.lock();
        self.changed_locked(from, to)
    }

    fn changed_locked(&self, from: &str, to: &str) -> Result<Vec<String>, CheckpointError> {
        let out = self.git().must(&["diff-tree", "-r", "--name-only", "--no-renames", "--relative", "-z", from, to], &[])?;
        Ok(out.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()).map(|p| String::from_utf8_lossy(p).into_owned()).collect())
    }

    /// Repository size in KiB (loose and packed objects).
    pub fn size_kib(&self) -> u64 {
        let _g = self.lock();
        let Ok(out) = self.git().run(&["count-objects", "-v"], &[]) else { return 0 };
        out.text()
            .lines()
            .filter_map(|l| {
                let (k, v) = l.split_once(':')?;
                matches!(k.trim(), "size" | "size-pack" | "size-garbage").then(|| v.trim().parse::<u64>().ok()).flatten()
            })
            .sum()
    }

    // ---- rewind ----------------------------------------------------------------------------

    /// Puts the workspace back to how it was when `turn` began: the earliest `pre` snapshot of
    /// `turn` or later is restored (files that differ are rewritten, files that did not exist
    /// then are deleted), then the pre-images of those turns. The state just before is kept as an
    /// `undo` ref first. A session with nothing at or after `turn` changes nothing.
    ///
    /// Only what this session's turns changed is put back: a path that differs from the target
    /// because of an edit outside them (the user's editor, another session, between or after the
    /// turns) is left as it is and listed in [`RestoreReport::kept`]. The target's ignore files
    /// are restored first, so a turn that edited `.gitignore` cannot make the restore delete
    /// files the target state never held.
    pub fn restore_to_turn(&self, session: &SessionId, turn: u32) -> Result<RestoreReport, CheckpointError> {
        self.restore_to_turn_at(session, turn, None)
    }

    /// [`restore_to_turn`](Self::restore_to_turn) given the time turn `turn` began, which bounds
    /// the pre-images that count (a turn number reused after a conversation rewind).
    pub fn restore_to_turn_at(&self, session: &SessionId, turn: u32, started_ms: Option<i64>) -> Result<RestoreReport, CheckpointError> {
        let mut guard = self.lock();
        let mut report = RestoreReport::default();
        let checkpoints = self.checkpoints_locked(session)?;
        let mut kept: BTreeSet<String> = BTreeSet::new();
        if let Some(at) = checkpoints.iter().position(|c| c.turn >= turn) {
            let target = &checkpoints[at];
            let mut warnings = Vec::new();
            let mut current = self.snapshot_tree(&mut warnings, None)?;
            let undo = format!("refs/marlowe/{}/undo/{}", ref_part(session.as_str()), now_ms());
            let commit = self.commit(&current, json!({"kind": "undo", "session": session.as_str(), "to_turn": turn, "at_ms": now_ms()}))?;
            self.git().must(&["update-ref", &undo, &commit], &[])?;
            report.undo_ref = Some(undo);
            report.from_turn = Some(target.turn);
            if current != target.tree {
                let ours = self.session_effects(session, &checkpoints[at..], &current)?;
                // The ignore files first, then a fresh look under the target's rules.
                let (write, delete) = self.plan(&current, &target.tree)?;
                let ignore_files = |v: &[String]| v.iter().filter(|p| is_ignore_file(p)).cloned().collect::<Vec<_>>();
                let (iw, id) = (ignore_files(&write), ignore_files(&delete));
                if !iw.is_empty() || !id.is_empty() {
                    self.apply(&target.tree, iw, id, &ours, &mut kept, &mut report)?;
                    current = self.snapshot_tree(&mut warnings, None)?;
                }
                if current != target.tree {
                    let (write, delete) = self.plan(&current, &target.tree)?;
                    self.apply(&target.tree, write, delete, &ours, &mut kept, &mut report)?;
                }
            }
            let now = self.snapshot_tree(&mut warnings, None)?;
            report.verified = now == target.tree || self.changed_locked(&now, &target.tree)?.iter().all(|p| kept.contains(p));
            // The turns being undone have no end any more.
            for c in checkpoints.iter().filter(|c| c.turn >= target.turn) {
                let _ = self.git().run(&["update-ref", "-d", &Self::post_ref(session, c.turn)], &[]);
            }
        } else {
            report.verified = true;
        }
        report.kept = kept.into_iter().collect();
        let since = started_ms.or_else(|| self.turn_start(&checkpoints, session, turn));
        report.preimages = self.preimages.restore_from(&ref_part(session.as_str()), turn, since);
        report.kept.extend(report.preimages.kept.iter().cloned());
        // The turns are being replayed: their snapshots must be retaken.
        guard.retain(|(s, t)| s != session.as_str() || *t < turn);
        Ok(report)
    }

    /// When `turn` first did something the ledger saw, if it did.
    fn turn_start(&self, checkpoints: &[TurnCheckpoint], session: &SessionId, turn: u32) -> Option<i64> {
        let pre = checkpoints.iter().find(|c| c.turn == turn).map(|c| c.at_ms);
        pre.into_iter().chain(self.preimages.first_save_ms(&ref_part(session.as_str()), turn)).min()
    }

    /// The paths this session's turns `cps` changed, minus any that also changed outside them:
    /// what a rewind may put back. A turn's span runs from its `pre` to its `post` (else the next
    /// `pre`, else now); the stretch from a `post` to the next `pre`, and from the last `post` to
    /// now, belongs to somebody else.
    fn session_effects(&self, session: &SessionId, cps: &[TurnCheckpoint], current: &str) -> Result<HashSet<String>, CheckpointError> {
        let posts = self.posts_locked(session)?;
        let mut touched: HashSet<String> = HashSet::new();
        let mut outside: HashSet<String> = HashSet::new();
        for (i, c) in cps.iter().enumerate() {
            let next = cps.get(i + 1).map(|n| n.tree.as_str());
            let post = posts.get(&c.turn).map(String::as_str);
            let end = post.or(next).unwrap_or(current);
            touched.extend(self.changed_locked(&c.tree, end)?);
            if let Some(post) = post {
                outside.extend(self.changed_locked(post, next.unwrap_or(current))?);
            }
        }
        Ok(touched.difference(&outside).cloned().collect())
    }

    /// Writes and deletes the paths of a plan that are `ours`; the rest are kept.
    fn apply(
        &self,
        tree: &str,
        write: Vec<String>,
        delete: Vec<String>,
        ours: &HashSet<String>,
        kept: &mut BTreeSet<String>,
        report: &mut RestoreReport,
    ) -> Result<(), CheckpointError> {
        let (write, left): (Vec<String>, Vec<String>) = write.into_iter().partition(|p| ours.contains(p));
        let (delete, left_d): (Vec<String>, Vec<String>) = delete.into_iter().partition(|p| ours.contains(p));
        kept.extend(left);
        kept.extend(left_d);
        for rel in &delete {
            let p = self.workspace.join(rel);
            if remove_file(&p) {
                report.deleted += 1;
            }
            self.prune_empty_dirs(&p);
        }
        if !write.is_empty() {
            self.checkout(tree, &write)?;
            report.restored += write.len();
        }
        Ok(())
    }

    /// Files to rewrite and files to delete to turn tree `current` into `target`.
    fn plan(&self, current: &str, target: &str) -> Result<(Vec<String>, Vec<String>), CheckpointError> {
        let out = self.git().must(&["diff-tree", "-r", "-z", "--raw", "--no-renames", "--no-abbrev", "--relative", current, target], &[])?;
        let mut write = Vec::new();
        let mut delete = Vec::new();
        let mut fields = out.stdout.split(|b| *b == 0).filter(|f| !f.is_empty());
        while let Some(meta) = fields.next() {
            let Some(path) = fields.next() else { break };
            let meta = String::from_utf8_lossy(meta);
            let path = String::from_utf8_lossy(path).into_owned();
            // ":<src mode> <dst mode> <src sha> <dst sha> <status>"
            let mut m = meta.trim_start_matches(':').split(' ');
            let (src_mode, dst_mode) = (m.next().unwrap_or(""), m.next().unwrap_or(""));
            if src_mode == "160000" || dst_mode == "160000" {
                continue; // an embedded repository: not ours to restore
            }
            if dst_mode != "000000" {
                if src_mode != "000000" && is_symlink(src_mode) != is_symlink(dst_mode) {
                    delete.push(path.clone());
                }
                write.push(path);
            } else {
                delete.push(path);
            }
        }
        Ok((write, delete))
    }

    fn checkout(&self, tree: &str, rels: &[String]) -> Result<(), CheckpointError> {
        let list = self.git_dir.join("restore.list");
        let mut bytes = Vec::new();
        for r in rels {
            bytes.extend_from_slice(r.as_bytes());
            bytes.push(0);
        }
        fs::write(&list, bytes)?;
        let source = format!("--source={tree}");
        let from = format!("--pathspec-from-file={}", list.to_string_lossy());
        let r = self.git().must(&["restore", &source, "--worktree", &from, "--pathspec-file-nul"], &[]);
        let _ = fs::remove_file(&list);
        r.map(|_| ())
    }

    fn prune_empty_dirs(&self, file: &Path) {
        let mut dir = file.parent();
        while let Some(d) = dir {
            if self.rel(d).is_none_or(|r| r.is_empty()) || fs::remove_dir(d).is_err() {
                break;
            }
            dir = d.parent();
        }
    }

    /// Drops the refs and pre-images of `from..`: they were replayed or the conversation moved
    /// past them. With `archive` the `pre` and `post` refs are kept under `rewound/` and the
    /// pre-images retired (a conversation rewind that left the code alone keeps the older states
    /// reachable); the last kept turn then ends where the dropped ones did.
    pub fn discard_from(&self, session: &SessionId, from: u32, archive: bool) -> Result<(), CheckpointError> {
        let mut guard = self.lock();
        let sid = ref_part(session.as_str());
        let stamp = now_ms();
        let all = self.checkpoints_locked(session)?;
        let posts = self.posts_locked(session)?;
        for c in all.iter().filter(|c| c.turn >= from) {
            if archive {
                self.git().must(&["update-ref", &format!("refs/marlowe/{sid}/rewound/{stamp}/{}/pre", c.turn), &c.commit], &[])?;
                if let Some(post) = self.rev_of(&Self::post_ref(session, c.turn)) {
                    self.git().must(&["update-ref", &format!("refs/marlowe/{sid}/rewound/{stamp}/{}/post", c.turn), &post], &[])?;
                }
            }
            self.git().must(&["update-ref", "-d", &Self::pre_ref(session, c.turn)], &[])?;
            let _ = self.git().run(&["update-ref", "-d", &Self::post_ref(session, c.turn)], &[]);
        }
        for turn in posts.keys().filter(|t| **t >= from && !all.iter().any(|c| c.turn == **t)) {
            let _ = self.git().run(&["update-ref", "-d", &Self::post_ref(session, *turn)], &[]);
        }
        if archive {
            // The code stays as the dropped turns left it: that is where the last kept turn ends.
            let last_dropped = all.iter().filter(|c| c.turn >= from).map(|c| c.turn).max();
            let last_kept = all.iter().filter(|c| c.turn < from).map(|c| c.turn).max();
            if let (Some(dropped), Some(kept)) = (last_dropped, last_kept) {
                match self.rev_of(&format!("refs/marlowe/{sid}/rewound/{stamp}/{dropped}/post")) {
                    Some(post) => self.git().must(&["update-ref", &Self::post_ref(session, kept), &post], &[])?,
                    None => self.git().must(&["update-ref", "-d", &Self::post_ref(session, kept)], &[])?,
                };
            }
            self.preimages.retire_from(&sid, from, stamp);
        } else {
            self.preimages.discard_from(&sid, from);
        }
        guard.retain(|(s, t)| s != session.as_str() || *t < from);
        Ok(())
    }

    fn rev_of(&self, name: &str) -> Option<String> {
        let out = self.git().run(&["rev-parse", "--verify", "-q", &format!("{name}^{{commit}}")], &[]).ok()?;
        out.ok().then(|| out.text().trim().to_string())
    }

    /// Copies the ledger of turns before `before` to another session (a fork).
    pub fn fork(&self, from: &SessionId, to: &SessionId, before: u32) -> Result<(), CheckpointError> {
        let _g = self.lock();
        for c in self.checkpoints_locked(from)?.into_iter().filter(|c| c.turn < before) {
            self.git().must(&["update-ref", &Self::pre_ref(to, c.turn), &c.commit], &[])?;
            if let Some(post) = self.rev_of(&Self::post_ref(from, c.turn)) {
                self.git().must(&["update-ref", &Self::post_ref(to, c.turn), &post], &[])?;
            }
        }
        self.preimages.copy_before(&ref_part(from.as_str()), &ref_part(to.as_str()), before)
    }

    /// Deletes every checkpoint of sessions whose newest snapshot is older than `max_age_ms` and
    /// collects the unreachable objects. Returns the sessions pruned.
    pub fn prune(&self, max_age_ms: i64) -> Result<Vec<String>, CheckpointError> {
        let _g = self.lock();
        let out = self.git().must(&["for-each-ref", "--format=%(refname)%09%(contents:subject)", "refs/marlowe/"], &[])?;
        let mut newest: std::collections::BTreeMap<String, (i64, Vec<String>)> = Default::default();
        for line in out.text().lines() {
            let Some((name, subject)) = line.split_once('\t') else { continue };
            let Some(sid) = name.strip_prefix("refs/marlowe/").and_then(|r| r.split('/').next()) else { continue };
            let at = serde_json::from_str::<serde_json::Value>(subject).ok().and_then(|v| v.get("at_ms")?.as_i64()).unwrap_or(0);
            let e = newest.entry(sid.to_string()).or_insert((0, Vec::new()));
            e.0 = e.0.max(at);
            e.1.push(name.to_string());
        }
        for sid in self.preimages.sessions() {
            if let Some(ms) = self.preimages.newest_ms(&sid) {
                let e = newest.entry(sid).or_insert((0, Vec::new()));
                e.0 = e.0.max(ms);
            }
        }
        let cutoff = now_ms() - max_age_ms;
        let mut pruned = Vec::new();
        for (sid, (at, refs)) in newest {
            if at >= cutoff {
                continue;
            }
            for r in refs {
                self.git().must(&["update-ref", "-d", &r], &[])?;
            }
            self.preimages.discard_session(&sid);
            pruned.push(sid);
        }
        if !pruned.is_empty() {
            let _ = self.git().run(&["gc", "--prune=now", "--quiet"], &[]);
        }
        Ok(pruned)
    }
}

fn is_symlink(mode: &str) -> bool {
    mode == "120000"
}

fn is_ignore_file(rel: &str) -> bool {
    rel.rsplit('/').next() == Some(".gitignore")
}

/// The user's repository above (or at) `dir`: its root and its `info/exclude`.
fn find_user_repo(dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut at = Some(dir);
    while let Some(d) = at {
        let dot = d.join(".git");
        if dot.is_dir() {
            return Some((d.to_path_buf(), dot.join("info").join("exclude")));
        }
        if dot.is_file() {
            // A linked worktree or submodule: `gitdir: <path>`; excludes live in the common dir.
            let text = fs::read_to_string(&dot).ok()?;
            let gitdir = PathBuf::from(text.trim().strip_prefix("gitdir:")?.trim());
            let gitdir = if gitdir.is_absolute() { gitdir } else { d.join(gitdir) };
            let common = fs::read_to_string(gitdir.join("commondir")).ok().map(|c| gitdir.join(c.trim())).unwrap_or(gitdir);
            return Some((d.to_path_buf(), common.join("info").join("exclude")));
        }
        at = d.parent();
    }
    None
}
