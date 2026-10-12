//! The [`WorktreeProvider`] the conductor uses (`agent{isolation:"worktree"}`, `enter_worktree`):
//! real git worktrees under `<home>\worktrees\agents\<repo-hash>-<name>` on `marlowe/<name>`,
//! made with the same daemon-side git as the crew's sandboxes (hooks, fsmonitor and external diff drivers off, keys
//! scrubbed, pushes rewritten to nowhere). Each worktree leaves a small record beside it so
//! [`WorktreeProvider::attach`] finds it again after a restart without re-creating anything.

use std::path::{Path, PathBuf};

use marlowe_os::workspace::{WorktreeError, WorktreeHandle, WorktreeProvider, WorktreeRequest};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::sandbox::{git, is_git_repo, safe_name};

/// What `attach` and `remove` need to find a worktree again.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    id: String,
    /// The repository (or worktree) it was cut from; git runs there to remove it.
    repo: PathBuf,
    root: PathBuf,
    branch: String,
    base: String,
}

/// Git worktrees for the conductor and its subagents.
#[derive(Debug, Clone)]
pub struct GitWorktrees {
    dir: PathBuf,
}

impl GitWorktrees {
    /// Worktrees live in `<home>\worktrees\agents`.
    pub fn new(home: &Path) -> GitWorktrees {
        GitWorktrees { dir: home.join("worktrees").join("agents") }
    }

    fn record_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    fn read(&self, id: &str) -> Result<Record, WorktreeError> {
        if !valid_id(id) {
            return Err(WorktreeError::NotFound(id.to_string()));
        }
        let text = std::fs::read_to_string(self.record_path(id)).map_err(|_| WorktreeError::NotFound(id.to_string()))?;
        serde_json::from_str(&text).map_err(|e| WorktreeError::Failed(format!("the record of worktree {id} is unreadable: {e}")))
    }

    fn handle(r: &Record) -> WorktreeHandle {
        WorktreeHandle { id: r.id.clone(), root: r.root.clone(), branch: r.branch.clone(), base: r.base.clone() }
    }
}

fn failed(e: impl std::fmt::Display) -> WorktreeError {
    WorktreeError::Failed(e.to_string())
}

/// Longest name kept whole; a longer one is cut and given a hash of the whole, so two long names
/// with a common start stay apart.
const NAME_CHARS: usize = 40;

/// The branch-safe form of a requested name: [`safe_name`]'s, unique per distinct name.
fn worktree_name(name: &str) -> String {
    let mut full = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            full.push(c.to_ascii_lowercase());
        } else if !full.ends_with('-') {
            full.push('-');
        }
    }
    let full = full.trim_matches('-');
    if full.len() <= NAME_CHARS {
        return safe_name(name);
    }
    let hash = format!("{:x}", Sha256::digest(name.trim().as_bytes()));
    format!("{}-{}", full[..NAME_CHARS - 9].trim_end_matches('-'), &hash[..8])
}

/// Whether `id` can name a record file (ids are made here; a caller's is checked, not trusted).
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The repository `dir` belongs to (its common git dir, so a worktree and its checkout agree),
/// as a short hash for directory names.
fn repo_key(dir: &Path) -> Option<String> {
    let common = git(dir, &["rev-parse", "--path-format=absolute", "--git-common-dir"]).ok()?;
    let key = common.replace('\\', "/");
    let key = if cfg!(windows) { key.to_lowercase() } else { key };
    let hash = format!("{:x}", Sha256::digest(key.trim_end_matches('/').as_bytes()));
    Some(hash[..8].to_string())
}

impl WorktreeProvider for GitWorktrees {
    /// A new worktree on `marlowe/<name>` from `base` (default: the repository's HEAD), or the
    /// existing one of that name in the same repository. A request with a `parent` branches from
    /// that worktree's HEAD. The id (and directory) is `<repo-hash>-<name>`: worktrees of every
    /// repository share one folder, so the same name in two repositories is two worktrees.
    fn create(&self, req: &WorktreeRequest) -> Result<WorktreeHandle, WorktreeError> {
        // A child is cut from its parent worktree's HEAD but belongs to the parent's repository:
        // git runs there to remove it, after the parent may already be gone.
        let (repo, from) = match &req.parent {
            Some(parent) => {
                let p = self.read(parent)?;
                (p.repo, p.root)
            }
            None => (req.repo.clone(), req.repo.clone()),
        };
        if !is_git_repo(&repo) || !is_git_repo(&from) {
            return Err(WorktreeError::Unavailable(format!("{} is not a git repository", from.display())));
        }
        let key = repo_key(&repo).ok_or_else(|| WorktreeError::Unavailable(format!("{} is not a git repository", repo.display())))?;
        let name = worktree_name(&req.name);
        let id = format!("{key}-{name}");
        if let Ok(r) = self.read(&id) {
            if r.root.join(".git").exists() {
                if repo_key(&r.repo).as_deref() != Some(key.as_str()) {
                    return Err(WorktreeError::Failed(format!(
                        "worktree {id} belongs to {}, not {}",
                        r.repo.display(),
                        repo.display()
                    )));
                }
                return Ok(Self::handle(&r));
            }
        }
        let start = req.base.clone().unwrap_or_else(|| "HEAD".into());
        let base = git(&from, &["rev-parse", "--verify", &format!("{start}^{{commit}}")])
            .map_err(|_| WorktreeError::Failed(format!("`{start}` is not a commit in {}", from.display())))?;
        let root = self.dir.join(&id);
        let branch = format!("marlowe/{name}");
        std::fs::create_dir_all(&self.dir).map_err(failed)?;
        let path = root.display().to_string();
        let exists = git(&repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok();
        if exists {
            git(&repo, &["worktree", "add", &path, &branch]).map_err(failed)?;
        } else {
            git(&repo, &["worktree", "add", "-b", &branch, &path, &base]).map_err(failed)?;
        }
        let record = Record { id: id.clone(), repo, root, branch, base };
        let json = serde_json::to_string_pretty(&record).map_err(failed)?;
        std::fs::write(self.record_path(&id), json).map_err(failed)?;
        Ok(Self::handle(&record))
    }

    fn attach(&self, id: &str) -> Result<WorktreeHandle, WorktreeError> {
        let r = self.read(id)?;
        if !r.root.join(".git").exists() {
            return Err(WorktreeError::NotFound(id.to_string()));
        }
        Ok(Self::handle(&r))
    }

    fn remove(&self, handle: &WorktreeHandle, keep_branch: bool) -> Result<(), WorktreeError> {
        let r = self.read(&handle.id)?;
        let path = r.root.display().to_string();
        if git(&r.repo, &["worktree", "remove", "--force", &path]).is_err() && r.root.exists() {
            std::fs::remove_dir_all(&r.root).map_err(failed)?;
        }
        let _ = git(&r.repo, &["worktree", "prune"]);
        if !keep_branch && git(&r.repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{}", r.branch)]).is_ok() {
            git(&r.repo, &["branch", "-D", &r.branch]).map_err(failed)?;
        }
        let _ = std::fs::remove_file(self.record_path(&r.id));
        Ok(())
    }
}
