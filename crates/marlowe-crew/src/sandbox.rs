//! Where a run may write. A git workspace gets a worktree on `crew/<run-id>` cut from HEAD;
//! anything else gets `<home>\scratch\<run-id>`. File-tool writes inside the sandbox are free and
//! writes outside ask. Crew shells start in the sandbox, and a lexical matcher asks for the risky
//! commands it can see: git writes outside the worktree or to shared refs and config, deletes,
//! copies and redirects outside the sandbox, and `git push`. It is a tripwire, not confinement: a
//! child process can still write wherever the user can. Only the daemon merges, after the user
//! approves, and its own git calls run with hooks, fsmonitor and external diff drivers off.
//!
//! A worktree shares `.git/config` and hooks with the original checkout, so the sandbox records a
//! digest of them at creation and refuses to diff or merge once they have changed.

use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use marlowe_proto::{DiffSummary, RunId, SandboxInfo, SandboxKind};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::CrewError;

/// A run's sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sandbox {
    /// What was actually created (a worktree request on a non-git workspace becomes scratch).
    pub kind: SandboxKind,
    /// The writable root. For `SandboxKind::None` it is the scratch path, used only as a cwd and
    /// never free to write.
    pub root: PathBuf,
    /// The original checkout (worktree) or workspace (scratch), read-only to the crew.
    pub origin: Option<PathBuf>,
    /// `crew/<run-id>` for a worktree.
    pub branch: Option<String>,
    /// The commit the worktree was cut from; diffs are taken against it.
    pub base: Option<String>,
    /// sha256 of the original repository's guarded config entries and hooks when the worktree
    /// was made ([`repo_digest`]). `None` for scratch sandboxes and runs from before it existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_digest: Option<String>,
    /// Hub files (relative, forward slashes) this sandbox may not write: a build_v2 child
    /// worktree after the contract stage. Changes to them go through the lead.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub frozen: Vec<String>,
    /// `CARGO_TARGET_DIR` for every process the run spawns: one `<home>\targets\<repo-hash>` per
    /// repository, shared by the run's worktree and its children (Cargo's lock serialises them).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_dir: Option<PathBuf>,
}

/// What a merge did to the original checkout's current branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// The branch moved forward to `head`.
    FastForward { head: String },
    /// A merge commit was created.
    MergeCommit { commit: String },
    /// The crew branch had no commits beyond the base.
    NothingToMerge,
    /// A research run's report was accepted and copied into the workspace.
    ReportCopied { path: PathBuf },
}

/// Whether a crew shell command may run without asking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellVerdict {
    Free,
    /// Needs approval; the one line shown in the approval modal.
    Ask(String),
}

/// `crew/<run-id>`.
pub fn branch_name(run: &RunId) -> String {
    format!("crew/{run}")
}

/// `<home>\worktrees\<run-id>`.
pub fn worktree_path(home: &Path, run: &RunId) -> PathBuf {
    home.join("worktrees").join(run.as_str())
}

/// `<home>\scratch\<run-id>`.
pub fn scratch_path(home: &Path, run: &RunId) -> PathBuf {
    home.join("scratch").join(run.as_str())
}

/// `<home>\targets\<repo-hash>`: the build output directory every crew process working on
/// `origin` shares, so child worktrees do not each build from cold.
pub fn target_dir(home: &Path, origin: &Path) -> PathBuf {
    let key = origin.display().to_string().replace('\\', "/");
    let key = if cfg!(windows) { key.to_lowercase() } else { key };
    let hash = format!("{:x}", Sha256::digest(key.trim_end_matches('/').as_bytes()));
    home.join("targets").join(&hash[..16])
}

/// A branch- and directory-safe form of `name`: lower-case ASCII letters, digits, `-` and `_`,
/// at most 40 characters, never empty.
pub fn safe_name(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
        if out.len() >= 40 {
            break;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "task".into()
    } else {
        out
    }
}

/// The URL every crew push is rewritten to. Not a scheme git can reach.
pub const NO_PUSH_URL: &str = "invalid://crew-cannot-push/";

/// Environment for every process a crew run spawns, on top of the scrubbed environment: each
/// named remote's `pushurl` list is reset to [`NO_PUSH_URL`], every URL (empty prefix) and common
/// prefix is rewritten to it for pushes, `credential.helper` is empty, and git never prompts. Passed as
/// `GIT_CONFIG_COUNT` / `GIT_CONFIG_KEY_n` / `GIT_CONFIG_VALUE_n`, which override every config file.
pub fn no_push_git_env(remotes: &[String]) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = remotes
        .iter()
        .filter(|r| !r.trim().is_empty())
        .flat_map(|r| {
            let key = format!("remote.{}.pushurl", r.trim());
            // An empty value clears pushurls from the repo's own config (git >= 2.46).
            [(key.clone(), String::new()), (key, NO_PUSH_URL.to_string())]
        })
        .collect();
    // The empty prefix matches every URL, including bare paths given on the command line.
    for prefix in ["", "https://", "http://", "ssh://", "git://", "git@", "file://"] {
        pairs.push((format!("url.{NO_PUSH_URL}.pushInsteadOf"), prefix.to_string()));
    }
    pairs.push(("credential.helper".into(), String::new()));
    let mut env = vec![
        ("GIT_CONFIG_COUNT".to_string(), pairs.len().to_string()),
        ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
    ];
    for (i, (k, v)) in pairs.into_iter().enumerate() {
        env.push((format!("GIT_CONFIG_KEY_{i}"), k));
        env.push((format!("GIT_CONFIG_VALUE_{i}"), v));
    }
    env
}


/// True when `path` is inside a git work tree (`git rev-parse --is-inside-work-tree`).
pub fn is_git_repo(path: &Path) -> bool {
    path.is_dir() && git(path, &["rev-parse", "--is-inside-work-tree"]).is_ok_and(|o| o == "true")
}

/// A daemon-side `git -C dir` command. The repository may have been touched by a crew, so
/// nothing it configures runs: hooks point at the null device, fsmonitor is off, API keys are
/// scrubbed, every push is rewritten to [`NO_PUSH_URL`], and there is no console window. Diff
/// commands also pass `--no-ext-diff --no-textconv`.
pub(crate) fn git_cmd(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.args(["-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false"]).arg("-C").arg(dir);
    for var in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_EXTERNAL_DIFF", "GIT_SSH_COMMAND"] {
        cmd.env_remove(var);
    }
    for var in marlowe_brains::scrubbed_env() {
        cmd.env_remove(var);
    }
    cmd.envs(no_push_git_env(&[])).stdin(Stdio::null());
    no_window(&mut cmd);
    cmd
}

/// Repository config keys a crew may change without tripping the digest: identity and
/// per-branch metadata (editors write `branch.*` entries on their own).
fn unguarded_key(entry: &str) -> bool {
    entry.starts_with("branch.") || entry.starts_with("user.")
}

/// A digest of what in the original repository would run code or redirect pushes for its owner:
/// its own config (every entry but [`unguarded_key`] ones) and every file in its hooks folders
/// (`<common>/hooks` and a configured `core.hooksPath`). `None` when `origin` is not a repository.
pub fn repo_digest(origin: &Path) -> Option<String> {
    let common = PathBuf::from(git(origin, &["rev-parse", "--path-format=absolute", "--git-common-dir"]).ok()?);
    let file = common.join("config").display().to_string();
    // `--file` reads that file only; the `-c` overrides of `git_cmd` stay out of the listing.
    let entries = git(origin, &["config", "--file", &file, "--list"]).unwrap_or_default();
    let mut h = Sha256::new();
    let mut hooks_path = None;
    for line in entries.lines().filter(|l| !unguarded_key(l)) {
        if let Some(v) = line.strip_prefix("core.hookspath=") {
            hooks_path = Some(normalize(origin, Path::new(v)));
        }
        h.update(line.as_bytes());
        h.update(b"\n");
    }
    for dir in std::iter::once(common.join("hooks")).chain(hooks_path) {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_file()).collect())
            .unwrap_or_default();
        files.sort();
        for f in files {
            h.update(f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default().as_bytes());
            h.update(b"\0");
            h.update(std::fs::read(&f).unwrap_or_default());
            h.update(b"\0");
        }
    }
    Some(format!("{:x}", h.finalize()))
}

/// Runs git in `dir`; stdout with the trailing newline cut, or the stderr as the error.
pub(crate) fn git(dir: &Path, args: &[&str]) -> Result<String, CrewError> {
    let out = git_cmd(dir).args(args).output().map_err(|e| CrewError::Git(format!("cannot run git: {e}")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let err = if err.is_empty() { String::from_utf8_lossy(&out.stdout).trim().to_string() } else { err };
        return Err(CrewError::Git(format!("git {}: {}", args.join(" "), err)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

#[cfg(windows)]
pub(crate) fn no_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
pub(crate) fn no_window(_cmd: &mut Command) {}

/// `-c user.name/-c user.email` when the repo has no identity, so daemon-side commits never fail.
fn identity_args(dir: &Path) -> Vec<&'static str> {
    if git(dir, &["config", "user.email"]).is_ok_and(|e| !e.is_empty()) {
        Vec::new()
    } else {
        vec!["-c", "user.name=Marlowe crew", "-c", "user.email=crew@marlowe.invalid"]
    }
}

/// Resolves `.` and `..` lexically; relative paths are taken against `base`.
pub(crate) fn normalize(base: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() || path.has_root() { path.to_path_buf() } else { base.join(path) };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                // Never climbs above the root or the drive prefix.
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether `path` is `root` or below it. Case-insensitive and separator-agnostic on Windows.
pub(crate) fn within(root: &Path, path: &Path) -> bool {
    fn key(p: &Path) -> Vec<String> {
        p.components()
            .filter(|c| !matches!(c, Component::CurDir | Component::RootDir))
            .map(|c| {
                let s = c.as_os_str().to_string_lossy().replace('\\', "/");
                if cfg!(windows) {
                    s.to_lowercase()
                } else {
                    s
                }
            })
            .collect()
    }
    let (r, p) = (key(root), key(path));
    !r.is_empty() && p.len() >= r.len() && p[..r.len()] == r[..]
}

impl Sandbox {
    /// Creates the sandbox for `run`. `want: Worktree` on a git `workspace` runs
    /// `git worktree add <home>\worktrees\<run-id> -b crew/<run-id>` from HEAD; otherwise (or on
    /// `Scratch`) creates `<home>\scratch\<run-id>`. `None` creates the scratch dir as a cwd only.
    /// Idempotent: a resumed run finds its existing worktree or scratch dir.
    pub fn create(home: &Path, run: &RunId, workspace: Option<&Path>, want: SandboxKind) -> Result<Sandbox, CrewError> {
        if want == SandboxKind::Worktree {
            if let Some(sb) = Self::worktree(home, run, workspace)? {
                return Ok(sb);
            }
        }
        let root = scratch_path(home, run);
        std::fs::create_dir_all(&root)?;
        let kind = if want == SandboxKind::None { SandboxKind::None } else { SandboxKind::Scratch };
        Ok(Sandbox {
            kind,
            root,
            origin: workspace.map(Path::to_path_buf),
            branch: None,
            base: None,
            repo_digest: None,
            frozen: Vec::new(),
            target_dir: None,
        })
    }

    /// The worktree for `run`, or `None` when the workspace is not a git checkout with a commit.
    fn worktree(home: &Path, run: &RunId, workspace: Option<&Path>) -> Result<Option<Sandbox>, CrewError> {
        let Some(ws) = workspace.filter(|w| is_git_repo(w)) else { return Ok(None) };
        let origin = PathBuf::from(git(ws, &["rev-parse", "--show-toplevel"])?);
        let Ok(head) = git(&origin, &["rev-parse", "--verify", "HEAD"]) else { return Ok(None) };
        let root = worktree_path(home, run);
        let branch = branch_name(run);
        let branch_ref = format!("refs/heads/{branch}");
        let branch_exists = git(&origin, &["rev-parse", "--verify", "--quiet", &branch_ref]).is_ok();
        let sandbox = |base: String| Sandbox {
            kind: SandboxKind::Worktree,
            root: root.clone(),
            origin: Some(origin.clone()),
            branch: Some(branch.clone()),
            base: Some(base),
            repo_digest: repo_digest(&origin),
            frozen: Vec::new(),
            target_dir: Some(target_dir(home, &origin)),
        };
        if root.join(".git").exists() && branch_exists {
            // Resumed: the base is where the branch left the checkout's history.
            let base = git(&origin, &["merge-base", &branch, "HEAD"]).unwrap_or(head);
            return Ok(Some(sandbox(base)));
        }
        if let Some(parent) = root.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let path = root.display().to_string();
        let base = if branch_exists {
            git(&origin, &["worktree", "add", &path, &branch])?;
            git(&origin, &["merge-base", &branch, "HEAD"]).unwrap_or(head)
        } else {
            git(&origin, &["worktree", "add", "-b", &branch, &path, &head])?;
            head
        };
        Ok(Some(sandbox(base)))
    }

    /// The wire view.
    pub fn info(&self) -> SandboxInfo {
        SandboxInfo {
            kind: self.kind,
            root: self.root.display().to_string(),
            origin: self.origin.as_ref().map(|p| p.display().to_string()),
            branch: self.branch.clone(),
        }
    }

    /// Whether writes to `path` are free: inside `root` (after normalising `..`), and the
    /// sandbox is not `None`.
    pub fn allows_write(&self, path: &Path) -> bool {
        let abs = normalize(&self.root, path);
        self.kind != SandboxKind::None && within(&self.root, &abs) && self.frozen_path(&abs).is_none()
    }

    /// The frozen hub file `path` names, if it is one (relative to the root, forward slashes).
    pub fn frozen_path(&self, path: &Path) -> Option<String> {
        if self.frozen.is_empty() {
            return None;
        }
        let abs = normalize(&self.root, path);
        if !within(&self.root, &abs) {
            return None;
        }
        let rel = abs.strip_prefix(&self.root).ok().map(|p| p.to_string_lossy().replace('\\', "/")).or_else(|| {
            // A case or separator difference on Windows: count components instead.
            let n = self.root.components().count();
            let parts: Vec<String> = abs.components().skip(n).map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
            Some(parts.join("/"))
        })?;
        let same = |a: &str, b: &str| if cfg!(windows) { a.eq_ignore_ascii_case(b) } else { a == b };
        self.frozen.iter().find(|f| same(f.trim_start_matches("./"), &rel)).cloned()
    }

    /// Environment for the run's child processes ([`no_push_git_env`] with the origin's remotes).
    pub fn child_env(&self) -> Vec<(String, String)> {
        let remotes: Vec<String> = self
            .origin
            .as_deref()
            .filter(|o| is_git_repo(o))
            .and_then(|o| git(o, &["remote"]).ok())
            .map(|out| out.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect())
            .unwrap_or_default();
        let mut env = no_push_git_env(&remotes);
        if let Some(dir) = &self.target_dir {
            env.push(("CARGO_TARGET_DIR".into(), dir.display().to_string()));
        }
        env
    }

    /// A child worktree for one build_v2 task: `<home>\worktrees\<run-id>-<task>` on
    /// `crew/<run-id>-<task>`, cut from this worktree's committed HEAD, sharing its origin, digest
    /// and target dir, with `frozen` hub files it may not write. Idempotent: a resumed run finds
    /// the child it made.
    pub fn create_child(&self, home: &Path, run: &RunId, task: &str, frozen: &[String]) -> Result<Sandbox, CrewError> {
        let (origin, _, _) = self.worktree_parts()?;
        let origin = origin.to_path_buf();
        let name = format!("{}-{}", run.as_str(), safe_name(task));
        let root = home.join("worktrees").join(&name);
        let branch = format!("crew/{name}");
        let branch_ref = format!("refs/heads/{branch}");
        let branch_exists = git(&origin, &["rev-parse", "--verify", "--quiet", &branch_ref]).is_ok();
        let head = git(&self.root, &["rev-parse", "--verify", "HEAD"])?;
        let base = if root.join(".git").exists() && branch_exists {
            git(&origin, &["merge-base", &branch, &head]).unwrap_or(head)
        } else {
            if let Some(parent) = root.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let path = root.display().to_string();
            if branch_exists {
                git(&origin, &["worktree", "add", &path, &branch])?;
                git(&origin, &["merge-base", &branch, &head]).unwrap_or(head)
            } else {
                git(&origin, &["worktree", "add", "-b", &branch, &path, &head])?;
                head
            }
        };
        Ok(Sandbox {
            kind: SandboxKind::Worktree,
            root,
            origin: Some(origin),
            branch: Some(branch),
            base: Some(base),
            repo_digest: self.repo_digest.clone(),
            frozen: frozen.to_vec(),
            target_dir: self.target_dir.clone(),
        })
    }

    /// The worktree's parts, once the original repository's config and hooks are known to be as
    /// they were when the worktree was made.
    fn worktree_parts(&self) -> Result<(&Path, &str, &str), CrewError> {
        let parts = match (self.kind, self.origin.as_deref(), self.branch.as_deref(), self.base.as_deref()) {
            (SandboxKind::Worktree, Some(origin), Some(branch), Some(base)) => (origin, branch, base),
            _ => return Err(CrewError::Git("this run has no worktree".into())),
        };
        if let Some(before) = &self.repo_digest {
            if repo_digest(parts.0).as_ref() != Some(before) {
                return Err(CrewError::Git(format!(
                    "the git config or hooks of {} changed while this run had a worktree on it (a worktree \
                     shares them with the checkout); check `git config --local --list` and the hooks folder, \
                     then discard the run or merge its branch yourself",
                    parts.0.display()
                )));
            }
        }
        Ok(parts)
    }

    /// Changed files and line counts of `crew/<run-id>` (committed and uncommitted) against `base`.
    pub fn diff_summary(&self) -> Result<DiffSummary, CrewError> {
        let (_, branch, base) = self.worktree_parts()?;
        git(&self.root, &["add", "--all", "--intent-to-add"])?;
        let numstat = git(&self.root, &["diff", "--no-ext-diff", "--no-textconv", "--numstat", base])?;
        let mut d = DiffSummary { branch: branch.to_string(), files: Vec::new(), insertions: 0, deletions: 0 };
        for line in numstat.lines() {
            let mut cols = line.splitn(3, '\t');
            let (Some(ins), Some(del), Some(file)) = (cols.next(), cols.next(), cols.next()) else { continue };
            // Binary files report `-`.
            d.insertions += ins.parse::<u32>().unwrap_or(0);
            d.deletions += del.parse::<u32>().unwrap_or(0);
            d.files.push(file.to_string());
        }
        Ok(d)
    }

    /// The full diff text against `base`, for the Reviewer.
    pub fn diff_text(&self) -> Result<String, CrewError> {
        let (_, _, base) = self.worktree_parts()?;
        git(&self.root, &["add", "--all", "--intent-to-add"])?;
        git(&self.root, &["diff", "--no-ext-diff", "--no-textconv", base])
    }

    /// Daemon only, after the user approved: commits anything uncommitted in the worktree, then
    /// fast-forwards (else merge-commits) `crew/<run-id>` into the origin's current branch.
    /// Refuses when the origin has uncommitted changes to the same files. Never pushes.
    pub fn merge(&self) -> Result<MergeOutcome, CrewError> {
        let (origin, branch, _) = self.worktree_parts()?;
        git(&self.root, &["add", "--all"])?;
        if !git(&self.root, &["status", "--porcelain"])?.is_empty() {
            let msg = format!("{branch}: crew changes");
            let mut args = identity_args(&self.root);
            args.extend(["commit", "--quiet", "--no-verify", "-m", &msg]);
            git(&self.root, &args)?;
        }
        let current = git(origin, &["symbolic-ref", "--quiet", "--short", "HEAD"])
            .map_err(|_| CrewError::Git("the original checkout is on a detached HEAD; check out a branch first".into()))?;
        let ahead: u32 = git(origin, &["rev-list", "--count", &format!("HEAD..{branch}")])?.parse().unwrap_or(0);
        if ahead == 0 {
            return Ok(MergeOutcome::NothingToMerge);
        }
        let changed = git(origin, &["diff", "--no-ext-diff", "--name-only", &format!("HEAD...{branch}")])?;
        let dirty = dirty_paths(&git(origin, &["status", "--porcelain"])?);
        let clash: Vec<&str> = changed.lines().filter(|f| dirty.iter().any(|d| d == f)).take(5).collect();
        if !clash.is_empty() {
            return Err(CrewError::Git(format!(
                "uncommitted changes in the original checkout touch the same files ({}); commit or stash them first",
                clash.join(", ")
            )));
        }
        if git(origin, &["merge", "--ff-only", "--quiet", branch]).is_ok() {
            return Ok(MergeOutcome::FastForward { head: git(origin, &["rev-parse", "HEAD"])? });
        }
        let msg = format!("Merge {branch} into {current}");
        let mut args = identity_args(origin);
        args.extend(["merge", "--no-ff", "--no-edit", "--quiet", "-m", &msg, branch]);
        match git(origin, &args) {
            Ok(_) => Ok(MergeOutcome::MergeCommit { commit: git(origin, &["rev-parse", "HEAD"])? }),
            Err(e) => {
                let _ = git(origin, &["merge", "--abort"]);
                Err(CrewError::Git(format!("{branch} does not merge cleanly into {current}; nothing was changed ({e})")))
            }
        }
    }

    /// Removes the worktree (keeping the branch) or the scratch dir.
    pub fn remove(&self) -> Result<(), CrewError> {
        if let (SandboxKind::Worktree, Some(origin)) = (self.kind, self.origin.as_deref()) {
            let path = self.root.display().to_string();
            if git(origin, &["worktree", "remove", "--force", &path]).is_err() && self.root.exists() {
                std::fs::remove_dir_all(&self.root)?;
            }
            let _ = git(origin, &["worktree", "prune"]);
            return Ok(());
        }
        match std::fs::remove_dir_all(&self.root) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// Removes the worktree and deletes its branch, or removes the scratch dir.
    pub fn discard(&self) -> Result<(), CrewError> {
        self.remove()?;
        if let (SandboxKind::Worktree, Some(origin), Some(branch)) = (self.kind, self.origin.as_deref(), self.branch.as_deref()) {
            if git(origin, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok() {
                git(origin, &["branch", "-D", branch])?;
            }
        }
        Ok(())
    }
}

/// Paths named by `git status --porcelain` (both sides of a rename).
fn dirty_paths(porcelain: &str) -> Vec<String> {
    porcelain
        .lines()
        .filter(|l| l.len() > 3)
        .flat_map(|l| l[3..].split(" -> ").map(|p| p.trim_matches('"').to_string()).collect::<Vec<_>>())
        .collect()
}

/// Daemon only: stages everything in `dir` and commits it as `msg` (hooks off, an identity when
/// the repository has none). `Ok(false)` when there was nothing to commit.
pub(crate) fn commit_all(dir: &Path, msg: &str) -> Result<bool, CrewError> {
    git(dir, &["add", "--all"])?;
    if git(dir, &["status", "--porcelain"])?.is_empty() {
        return Ok(false);
    }
    let mut args = identity_args(dir);
    args.extend(["commit", "--quiet", "--no-verify", "-m", msg]);
    git(dir, &args)?;
    Ok(true)
}

/// [`git`] with an identity when the repository has none (merge commits made by the daemon).
pub(crate) fn git_as_crew(dir: &Path, args: &[&str]) -> Result<String, CrewError> {
    let mut all = identity_args(dir);
    all.extend(args.iter().copied());
    git(dir, &all)
}
