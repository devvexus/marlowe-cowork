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
//!
//! A workflow run works on the user's checkout as it is, uncommitted changes included (a
//! `/code-review` of "the pending changes"): its worktree is seeded with a snapshot of the
//! original working tree (tracked changes, staged or not, and untracked files git does not
//! ignore), kept alive by `refs/marlowe-seed/<run-id>`. In the worktree those changes show as
//! uncommitted, so `git diff` there sees what it would see in the checkout. Diffs are taken against
//! the snapshot, and the merge copies the run's own changes into the original working tree, file by
//! file, refusing when the user changed one of those files since the run started.

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
    /// `CARGO_TARGET_DIR` for every process spawned in this worktree: its own
    /// `<home>\targets\<repo-hash>\<worktree>`, removed with the worktree. Never shared between
    /// worktrees: Cargo keys path packages by their workspace-relative path and judges freshness
    /// by mtime, so one worktree's newer artifacts would pass for another's.
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
    /// A seeded run's changes were copied into the original working tree, uncommitted.
    Applied { files: Vec<String> },
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

/// `<home>\worktrees\<short run id>` ([`short_dir`]).
pub fn worktree_path(home: &Path, run: &RunId) -> PathBuf {
    home.join("worktrees").join(short_dir(run.as_str()))
}

/// A worktree's directory name: a run id's first 10 hex digits and whatever follows the id
/// (`-a3`, `-<task>`). A repository's deepest files sit under it, so every character here counts
/// against Windows' 260-character path limit.
pub fn short_dir(id: &str) -> String {
    let hex = id.chars().take_while(char::is_ascii_hexdigit).count();
    if hex >= 32 {
        format!("{}{}", &id[..10], &id[32..])
    } else {
        id.to_string()
    }
}

/// The ref that keeps a seeded worktree's snapshot commit (and marks the sandbox as seeded).
fn seed_ref(branch: &str) -> String {
    format!("refs/marlowe-seed/{}", branch.strip_prefix("crew/").unwrap_or(branch))
}

/// `<home>\scratch\<run-id>`.
pub fn scratch_path(home: &Path, run: &RunId) -> PathBuf {
    home.join("scratch").join(run.as_str())
}

/// `<home>\targets\<repo-hash>\<worktree>`: the build output directory of one worktree of
/// `origin` (see [`Sandbox::target_dir`]).
pub fn target_dir(home: &Path, origin: &Path, worktree: &str) -> PathBuf {
    let key = origin.display().to_string().replace('\\', "/");
    let key = if cfg!(windows) { key.to_lowercase() } else { key };
    let hash = format!("{:x}", Sha256::digest(key.trim_end_matches('/').as_bytes()));
    home.join("targets").join(&hash[..16]).join(worktree)
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
    // `core.longpaths`: a worktree under the profile is deeper than the checkout it copies.
    cmd.args(["-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false", "-c", "core.longpaths=true"]).arg("-C").arg(dir);
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
        Self::create_with(home, run, workspace, want, false)
    }

    /// [`Sandbox::create`]; with `pending`, a worktree starts from the original working tree as it
    /// is, uncommitted changes included, and merges back into that working tree (workflow runs).
    pub fn create_with(home: &Path, run: &RunId, workspace: Option<&Path>, want: SandboxKind, pending: bool) -> Result<Sandbox, CrewError> {
        if want == SandboxKind::Worktree {
            if let Some(sb) = Self::worktree(home, run, workspace, pending)? {
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
    fn worktree(home: &Path, run: &RunId, workspace: Option<&Path>, pending: bool) -> Result<Option<Sandbox>, CrewError> {
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
            target_dir: Some(target_dir(home, &origin, run.as_str())),
        };
        let seeded = git(&origin, &["rev-parse", "--verify", "--quiet", &seed_ref(&branch)]).ok();
        if root.join(".git").exists() && branch_exists {
            // Resumed: the base is the snapshot it was seeded with, or where the branch left the
            // checkout's history.
            let base = seeded.unwrap_or_else(|| git(&origin, &["merge-base", &branch, "HEAD"]).unwrap_or(head));
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
        if !pending || branch_exists {
            return Ok(Some(sandbox(seeded.unwrap_or(base))));
        }
        let seed = snapshot(&origin, &base, &root.with_extension("seed-index"))?;
        git(&origin, &["update-ref", &seed_ref(&branch), &seed])?;
        if seed != base {
            // The snapshot's files into the worktree, then the index back to HEAD: the changes are
            // uncommitted there, as they are in the checkout.
            git(&root, &["read-tree", "-u", "--reset", &seed])?;
            git(&root, &["reset", "--quiet"])?;
        }
        Ok(Some(sandbox(seed)))
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
    /// `crew/<run-id>-<task>`, cut from this worktree's committed HEAD, sharing its origin and
    /// digest, with its own target dir and `frozen` hub files it may not write. Idempotent: a
    /// resumed run finds the child it made.
    pub fn create_child(&self, home: &Path, run: &RunId, task: &str, frozen: &[String]) -> Result<Sandbox, CrewError> {
        let (origin, _, _) = self.worktree_parts()?;
        let origin = origin.to_path_buf();
        let name = format!("{}-{}", run.as_str(), safe_name(task));
        let root = home.join("worktrees").join(short_dir(&name));
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
        let target = target_dir(home, &origin, &short_dir(&name));
        Ok(Sandbox {
            kind: SandboxKind::Worktree,
            root,
            origin: Some(origin),
            branch: Some(branch),
            base: Some(base),
            repo_digest: self.repo_digest.clone(),
            frozen: frozen.to_vec(),
            target_dir: Some(target),
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

    /// Whether the worktree was seeded with the original working tree ([`Sandbox::create_with`]).
    pub fn is_seeded(&self) -> bool {
        match (self.kind, self.origin.as_deref(), self.branch.as_deref()) {
            (SandboxKind::Worktree, Some(origin), Some(branch)) => git(origin, &["rev-parse", "--verify", "--quiet", &seed_ref(branch)]).is_ok(),
            _ => false,
        }
    }

    /// Daemon only, after the user approved: commits anything uncommitted in the worktree, then
    /// fast-forwards (else merge-commits) `crew/<run-id>` into the origin's current branch.
    /// Refuses when the origin has uncommitted changes to the same files. Never pushes. A seeded
    /// worktree's changes are copied into the origin's working tree instead ([`MergeOutcome::Applied`]).
    pub fn merge(&self) -> Result<MergeOutcome, CrewError> {
        let (origin, branch, base) = self.worktree_parts()?;
        git(&self.root, &["add", "--all"])?;
        if !git(&self.root, &["status", "--porcelain"])?.is_empty() {
            let msg = format!("{branch}: crew changes");
            let mut args = identity_args(&self.root);
            args.extend(["commit", "--quiet", "--no-verify", "-m", &msg]);
            git(&self.root, &args)?;
        }
        if self.is_seeded() {
            return self.apply_to_working_tree(origin, base);
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

    /// Removes the worktree (keeping the branch) and its build output, or the scratch dir.
    pub fn remove(&self) -> Result<(), CrewError> {
        if let (SandboxKind::Worktree, Some(origin)) = (self.kind, self.origin.as_deref()) {
            remove_worktree(origin, &self.root)?;
            if let Some(dir) = &self.target_dir {
                let _ = std::fs::remove_dir_all(dir);
            }
            return Ok(());
        }
        match std::fs::remove_dir_all(&self.root) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// Removes the worktree and deletes its branch, or removes the scratch dir. Child worktrees
    /// of a build (on `<branch>-<task>`) go with it, kept failed-task branches included.
    pub fn discard(&self) -> Result<(), CrewError> {
        self.remove()?;
        if let (SandboxKind::Worktree, Some(origin), Some(branch)) = (self.kind, self.origin.as_deref(), self.branch.as_deref()) {
            if git(origin, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok() {
                git(origin, &["branch", "-D", branch])?;
            }
            let seed = seed_ref(branch);
            if git(origin, &["rev-parse", "--verify", "--quiet", &seed]).is_ok() {
                git(origin, &["update-ref", "-d", &seed])?;
            }
            self.discard_children(origin, branch)?;
        }
        Ok(())
    }

    /// A seeded worktree's merge: every file the run changed since the snapshot `seed` is copied
    /// (or deleted) in the origin's working tree, uncommitted. Refuses, changing nothing, when one
    /// of those files in the origin is no longer what the snapshot had.
    fn apply_to_working_tree(&self, origin: &Path, seed: &str) -> Result<MergeOutcome, CrewError> {
        let listing = git(&self.root, &["diff", "--no-ext-diff", "--no-textconv", "--no-renames", "--name-status", "-z", seed, "HEAD"])?;
        let mut parts = listing.split('\0').filter(|p| !p.is_empty());
        let mut changes: Vec<(char, String)> = Vec::new();
        while let (Some(status), Some(path)) = (parts.next(), parts.next()) {
            changes.push((status.chars().next().unwrap_or('M'), path.to_string()));
        }
        if changes.is_empty() {
            return Ok(MergeOutcome::NothingToMerge);
        }
        // What the origin's file is now, as a blob id (through the same filters git uses).
        let now = |path: &str| -> Option<String> {
            let full = origin.join(path);
            full.is_file().then(|| git(origin, &["hash-object", "--", path]).ok()).flatten()
        };
        let mut clash = Vec::new();
        for (status, path) in &changes {
            let full = normalize(origin, Path::new(path));
            if !within(origin, &full) {
                return Err(CrewError::Git(format!("refusing a path outside the checkout: {path}")));
            }
            let was = if *status == 'A' { None } else { git(&self.root, &["rev-parse", "--verify", "--quiet", &format!("{seed}:{path}")]).ok() };
            if now(path) != was {
                clash.push(path.as_str());
            }
        }
        if !clash.is_empty() {
            return Err(CrewError::Git(format!(
                "the original checkout changed since this run started ({}); nothing was changed",
                clash.iter().take(5).copied().collect::<Vec<_>>().join(", ")
            )));
        }
        for (status, path) in &changes {
            let dest = origin.join(path);
            if *status == 'D' {
                match std::fs::remove_file(&dest) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                    _ => {}
                }
            } else {
                if let Some(dir) = dest.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::copy(self.root.join(path), &dest)?;
            }
        }
        Ok(MergeOutcome::Applied { files: changes.into_iter().map(|(_, p)| p).collect() })
    }

    /// Removes every worktree on a `<branch>-*` branch with its build output, then deletes those
    /// branches. Read from git, so children of a run that stopped mid-wave are found too.
    fn discard_children(&self, origin: &Path, branch: &str) -> Result<(), CrewError> {
        let prefix = format!("refs/heads/{branch}-");
        let listing = git(origin, &["worktree", "list", "--porcelain"])?;
        let mut path: Option<PathBuf> = None;
        for line in listing.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p));
            } else if let (Some(b), Some(p)) = (line.strip_prefix("branch "), path.as_ref()) {
                if b.starts_with(&prefix) {
                    remove_worktree(origin, p)?;
                    let target = self.target_dir.as_deref().and_then(Path::parent).zip(p.file_name());
                    if let Some((targets, name)) = target {
                        let _ = std::fs::remove_dir_all(targets.join(name));
                    }
                }
            }
        }
        let refs = git(origin, &["for-each-ref", "--format=%(refname:short)", &format!("{prefix}*")])?;
        for b in refs.lines().map(str::trim).filter(|b| !b.is_empty()) {
            git(origin, &["branch", "-D", b])?;
        }
        Ok(())
    }
}

/// A commit of `origin`'s working tree as it is (tracked changes, staged or not, and untracked
/// files git does not ignore) on top of `head`; `head` itself when nothing is pending. Built in a
/// scratch index, so the checkout's own index is never touched.
fn snapshot(origin: &Path, head: &str, scratch_index: &Path) -> Result<String, CrewError> {
    let index = PathBuf::from(git(origin, &["rev-parse", "--path-format=absolute", "--git-path", "index"])?);
    let _ = std::fs::remove_file(scratch_index);
    let with_index = |args: &[&str]| -> Result<String, CrewError> {
        let out = git_cmd(origin)
            .env("GIT_INDEX_FILE", scratch_index)
            .args(args)
            .output()
            .map_err(|e| CrewError::Git(format!("cannot run git: {e}")))?;
        if !out.status.success() {
            return Err(CrewError::Git(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim())));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    };
    // A copy of the real index keeps its stat data, so unchanged files are not hashed again.
    let built = (|| {
        if index.is_file() {
            std::fs::copy(&index, scratch_index)?;
        } else {
            with_index(&["read-tree", head])?;
        }
        with_index(&["add", "--all"])?;
        with_index(&["write-tree"])
    })();
    let _ = std::fs::remove_file(scratch_index);
    let tree = built?;
    if tree == git(origin, &["rev-parse", &format!("{head}^{{tree}}")])? {
        return Ok(head.to_string());
    }
    let mut args = identity_args(origin);
    args.extend(["commit-tree", &tree, "-p", head, "-m", "marlowe: the working tree a workflow run started from"]);
    git(origin, &args)

}

/// `git worktree remove --force`, falling back to deleting the directory, then a prune.
fn remove_worktree(origin: &Path, root: &Path) -> Result<(), CrewError> {
    let path = root.display().to_string();
    if git(origin, &["worktree", "remove", "--force", &path]).is_err() && root.exists() {
        std::fs::remove_dir_all(root)?;
    }
    let _ = git(origin, &["worktree", "prune"]);
    Ok(())
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
