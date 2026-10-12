//! The mechanical git of a build: commits, merges in topo order, conflict lists, taking one side
//! of a lockfile, resets for the serial fallback. All of it is the daemon's own git (hooks,
//! fsmonitor and external diff drivers off), never an agent's.

use std::path::Path;

use crate::sandbox::{commit_all, git, git_as_crew};
use crate::CrewError;

/// How a merge ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Merged {
    /// Committed (or nothing to merge).
    Clean,
    /// Stopped with these files unmerged; the merge is still in progress.
    Conflicts(Vec<String>),
}

pub fn head(dir: &Path) -> Result<String, CrewError> {
    git(dir, &["rev-parse", "--verify", "HEAD"])
}

/// Commits everything in `dir`; `Ok(false)` when clean.
pub fn commit(dir: &Path, msg: &str) -> Result<bool, CrewError> {
    commit_all(dir, msg)
}

/// Files changed between `base` and HEAD (committed work only; commit first).
pub fn changed_since(dir: &Path, base: &str) -> Result<Vec<String>, CrewError> {
    let out = git(dir, &["diff", "--no-ext-diff", "--no-textconv", "--name-only", base, "HEAD"])?;
    Ok(out.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect())
}

/// Puts `files` back as they were at `base` (deleting the ones `base` did not have) and commits.
pub fn restore(dir: &Path, base: &str, files: &[String], msg: &str) -> Result<(), CrewError> {
    for f in files {
        if git(dir, &["cat-file", "-e", &format!("{base}:{f}")]).is_ok() {
            git(dir, &["checkout", base, "--", f])?;
        } else {
            let _ = git(dir, &["rm", "-f", "--quiet", "--", f]);
            let _ = std::fs::remove_file(dir.join(f));
        }
    }
    commit_all(dir, msg)?;
    Ok(())
}

/// `git merge --no-ff` of `branch` into the checkout at `dir`.
pub fn merge(dir: &Path, branch: &str, msg: &str) -> Result<Merged, CrewError> {
    match git_as_crew(dir, &["merge", "--no-ff", "--no-edit", "--quiet", "-m", msg, branch]) {
        Ok(_) => Ok(Merged::Clean),
        Err(e) => {
            let unmerged = unmerged(dir)?;
            if unmerged.is_empty() {
                let _ = git(dir, &["merge", "--abort"]);
                Err(e)
            } else {
                Ok(Merged::Conflicts(unmerged))
            }
        }
    }
}

/// Files still unmerged in an in-progress merge.
pub fn unmerged(dir: &Path) -> Result<Vec<String>, CrewError> {
    let out = git(dir, &["diff", "--name-only", "--diff-filter=U"])?;
    Ok(out.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect())
}

/// Resolves `file` to the checkout's own side (lockfiles: regenerated afterwards, never merged).
pub fn take_ours(dir: &Path, file: &str) -> Result<(), CrewError> {
    git(dir, &["checkout", "--ours", "--", file])?;
    git(dir, &["add", "--", file])?;
    Ok(())
}

/// Files among `files` that still hold conflict markers.
pub fn with_markers(dir: &Path, files: &[String]) -> Vec<String> {
    files
        .iter()
        .filter(|f| {
            std::fs::read_to_string(dir.join(f)).is_ok_and(|t| {
                t.lines().any(|l| l.starts_with("<<<<<<< ") || l.starts_with(">>>>>>> ") || l == "=======")
            })
        })
        .cloned()
        .collect()
}

/// Concludes an in-progress merge with everything staged.
pub fn conclude(dir: &Path) -> Result<(), CrewError> {
    git(dir, &["add", "--all"])?;
    git_as_crew(dir, &["commit", "--no-edit", "--quiet", "--no-verify"])?;
    Ok(())
}

pub fn abort(dir: &Path) {
    let _ = git(dir, &["merge", "--abort"]);
}

/// Moves the checkout at `dir` back to `sha`, discarding everything after it.
pub fn reset_hard(dir: &Path, sha: &str) -> Result<(), CrewError> {
    abort(dir);
    git(dir, &["reset", "--hard", "--quiet", sha])?;
    git(dir, &["clean", "-fd", "--quiet"])?;
    Ok(())
}

/// Whether `branch` is already merged into HEAD at `dir`.
pub fn is_merged(dir: &Path, branch: &str) -> bool {
    git(dir, &["merge-base", "--is-ancestor", branch, "HEAD"]).is_ok()
}
