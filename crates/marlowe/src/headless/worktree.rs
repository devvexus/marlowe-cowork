//! `-w NAME`: the run works in a git worktree of the repository, created on first use and reused
//! after. Git runs through `marlowe_os::child` (scrubbed environment, Job Object).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use marlowe_os::child::{self, ChildSpec, RunLimits};

pub(crate) fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.args(args);
    let spec = ChildSpec::new().with_cwd(dir).with_env([("GIT_TERMINAL_PROMPT".to_string(), "0".to_string())]);
    let out = child::run(cmd, &spec, RunLimits { timeout: Duration::from_secs(60), max_output_bytes: 256 * 1024 }, &|| false)
        .map_err(|e| format!("git {}: {e}", args.first().copied().unwrap_or("")))?;
    if out.code() != 0 {
        let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(format!("git {} failed: {}", args.join(" "), if why.is_empty() { format!("exit {}", out.code()) } else { why }));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A name that is safe as a directory and a branch component.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(['.', '-'])
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.contains("..")
}

/// The worktree named `name` of the repository containing `from`; created on the branch
/// `marlowe/<name>` (off HEAD) when it does not exist.
pub fn ensure(from: &Path, name: &str) -> Result<PathBuf, String> {
    if !valid_name(name) {
        return Err(format!("--worktree {name:?}: use letters, digits, '-', '_' and '.'"));
    }
    let top = git(from, &["rev-parse", "--show-toplevel"]).map_err(|_| format!("--worktree needs a git repository; {} is not inside one", from.display()))?;
    let dir = PathBuf::from(&top).join(".marlowe").join("worktrees").join(name);
    if dir.join(".git").exists() {
        return Ok(dir);
    }
    let branch = format!("marlowe/{name}");
    let dir_str = dir.display().to_string();
    let exists = git(Path::new(&top), &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok();
    if exists {
        git(Path::new(&top), &["worktree", "add", &dir_str, &branch])?;
    } else {
        git(Path::new(&top), &["worktree", "add", "-b", &branch, &dir_str])?;
    }
    // Keep `git status` in the main checkout quiet about the worktrees.
    if let Ok(common) = git(Path::new(&top), &["rev-parse", "--git-common-dir"]) {
        let common = PathBuf::from(&common);
        let common = if common.is_absolute() { common } else { PathBuf::from(&top).join(common) };
        let exclude = common.join("info").join("exclude");
        let line = "/.marlowe/worktrees/";
        let have = std::fs::read_to_string(&exclude).unwrap_or_default();
        if !have.lines().any(|l| l.trim() == line) {
            let _ = std::fs::create_dir_all(common.join("info"));
            let sep = if have.is_empty() || have.ends_with('\n') { "" } else { "\n" };
            let _ = std::fs::write(&exclude, format!("{have}{sep}{line}\n"));
        }
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("marlowe-hl-wt-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "t"],
            vec!["config", "commit.gpgsign", "false"],
        ] {
            git(&d, &args).unwrap();
        }
        std::fs::write(d.join("a.txt"), "one\n").unwrap();
        git(&d, &["add", "."]).unwrap();
        git(&d, &["commit", "-q", "-m", "first"]).unwrap();
        d
    }

    #[test]
    fn names_are_checked() {
        assert!(valid_name("feature-1"));
        assert!(valid_name("fix_2.x"));
        for bad in ["", ".hidden", "-x", "a b", "a/b", "a..b", "a\\b"] {
            assert!(!valid_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_worktree_is_created_once_reused_and_hidden_from_status() {
        let r = repo("a");
        let w = ensure(&r, "try1").unwrap();
        assert!(w.join("a.txt").is_file(), "{}", w.display());
        assert_eq!(git(&w, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap(), "marlowe/try1");
        // Reuse.
        assert_eq!(ensure(&r, "try1").unwrap(), w);
        assert_eq!(git(&r, &["status", "--porcelain"]).unwrap(), "", "the worktrees directory is excluded");
        // The branch survives a removed directory.
        git(&r, &["worktree", "remove", "--force", &w.display().to_string()]).unwrap();
        let again = ensure(&r, "try1").unwrap();
        assert!(again.join("a.txt").is_file());
        assert!(ensure(&r, "bad name").is_err());
    }

    #[test]
    fn outside_a_repository_it_says_so() {
        let d = std::env::temp_dir().join(format!("marlowe-hl-wt-{}-norepo", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let e = ensure(&d, "x").unwrap_err();
        assert!(e.contains("git repository"), "{e}");
    }
}
