//! Git, read-only: `git status --porcelain`, `git log -n`, `git rev-parse`, `git rev-list
//! --count`. Never fetch or pull. Every command runs with a timeout and `GIT_OPTIONAL_LOCKS=0`
//! so status never takes the index lock or refreshes the index.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use marlowe_proto::GitInfo;

/// The repository root containing `dir` (walks up looking for `.git`), without running git.
pub fn repo_root(dir: &Path) -> Option<PathBuf> {
    dir.ancestors().find(|d| d.join(".git").exists()).map(Path::to_path_buf)
}

/// Runs one read-only git command in `dir`. `None` on failure, non-zero exit or timeout (the
/// git child is then killed: it is ours, never a user process).
fn git(dir: &Path, args: &[&str], timeout: Duration) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .args(["-c", "core.fsmonitor=false", "-c", "color.ui=never"])
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().ok()?;
    // Drain stdout on a helper thread so a large status cannot fill the pipe and stall git.
    let mut out = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out.read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(15)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let buf = reader.join().ok()?;
    status.filter(|s| s.success())?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Branch, dirty count, HEAD and its subject/time; `new_commits` counts commits after
/// `last_seen_head` (0 when it is `None`, equal, or unknown to the repo). `None` when git is
/// missing, times out, or `dir` is not a repository.
pub fn probe(dir: &Path, last_seen_head: Option<&str>, timeout: Duration) -> Option<GitInfo> {
    let branch = git(dir, &["rev-parse", "--abbrev-ref", "HEAD"], timeout)
        .map(|b| b.trim().to_string())
        .or_else(|| {
            // A repository with no commits yet: rev-parse fails, symbolic-ref still names it.
            git(dir, &["symbolic-ref", "--short", "HEAD"], timeout).map(|b| b.trim().to_string())
        })?;
    let branch = if branch == "HEAD" { "(detached)".to_string() } else { branch };
    let status = git(dir, &["status", "--porcelain", "--ignore-submodules=dirty"], timeout)?;
    let dirty = status.lines().filter(|l| !l.trim().is_empty()).count() as u32;
    let mut info = GitInfo { branch, dirty, ..GitInfo::default() };
    if let Some(log) = git(dir, &["log", "-n", "1", "--format=%h%x1f%s%x1f%ct"], timeout) {
        let mut parts = log.trim_end_matches(['\r', '\n']).splitn(3, '\u{1f}');
        info.head = parts.next().map(str::to_string).filter(|h| !h.is_empty());
        info.subject = parts.next().map(str::to_string);
        info.commit_ms = parts.next().and_then(|t| t.trim().parse::<i64>().ok()).map(|s| s * 1000);
    }
    if let (Some(seen), Some(head)) = (last_seen_head, info.head.as_deref()) {
        if !seen.is_empty() && seen != head {
            let range = format!("{seen}..HEAD");
            info.new_commits = git(dir, &["rev-list", "--count", &range], timeout).and_then(|n| n.trim().parse().ok()).unwrap_or(0);
        }
    }
    Some(info)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Runs git for a test fixture (writes only inside the temp repo).
    pub(crate) fn run(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "git {args:?}");
    }

    pub(crate) fn git_available() -> bool {
        Command::new("git").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
    }

    #[test]
    fn probes_a_repo_made_here() {
        if !git_available() {
            eprintln!("git not installed; skipping");
            return;
        }
        let dir = std::env::temp_dir().join(format!("sentinel-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let t = Duration::from_secs(10);
        assert!(probe(&dir, None, t).is_none(), "not a repo yet");
        run(&dir, &["init", "-q"]);
        let empty = probe(&dir, None, t).unwrap();
        assert_eq!((empty.branch.as_str(), empty.head.as_deref()), ("main", None));
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        run(&dir, &["add", "a.txt"]);
        run(&dir, &["commit", "-q", "-m", "first commit"]);
        let first = probe(&dir, None, t).unwrap();
        assert_eq!((first.dirty, first.subject.as_deref(), first.new_commits), (0, Some("first commit"), 0));
        assert!(first.commit_ms.unwrap() > 1_600_000_000_000);
        std::fs::write(dir.join("b.txt"), "b").unwrap();
        std::fs::write(dir.join("a.txt"), "a2").unwrap();
        run(&dir, &["commit", "-q", "-am", "second"]);
        run(&dir, &["commit", "-q", "--allow-empty", "-m", "third"]);
        let now = probe(&dir, first.head.as_deref(), t).unwrap();
        assert_eq!((now.dirty, now.new_commits, now.subject.as_deref()), (1, 2, Some("third")));
        assert_eq!(repo_root(&dir.join("sub")), Some(dir.clone()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
