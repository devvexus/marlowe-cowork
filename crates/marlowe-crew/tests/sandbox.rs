//! Sandbox hardening: what a worktree shares with the original checkout, the daemon's own git
//! calls, and the shell policy's reach. Temp git repos only; no network, no models.

use std::path::{Path, PathBuf};
use std::process::Command;

use marlowe_crew::{MergeOutcome, RunId, Sandbox, SandboxKind, ShellVerdict};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("marlowe-crew-sb-{name}-{}", RunId::new_random().short()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// `<base>/work`: one commit on `main`, an identity, LF endings.
fn repo(base: &Path) -> PathBuf {
    let work = base.join("work");
    git(base, &["init", "-q", "-b", "main", work.to_str().unwrap()]);
    git(&work, &["config", "user.name", "Test"]);
    git(&work, &["config", "user.email", "test@example.invalid"]);
    git(&work, &["config", "core.autocrlf", "false"]);
    std::fs::write(work.join("README.md"), "hello\n").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "first"]);
    work
}

fn remove(dir: &Path) {
    for _ in 0..50 {
        if std::fs::remove_dir_all(dir).is_ok() || !dir.exists() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn asks(v: ShellVerdict) -> bool {
    matches!(v, ShellVerdict::Ask(_))
}

#[test]
fn a_change_to_the_shared_git_config_or_hooks_stops_diff_and_merge() {
    let base = temp_dir("shared");
    let work = repo(&base);
    let sb = Sandbox::create(&base.join("home"), &RunId::new_random(), Some(&work), SandboxKind::Worktree).unwrap();
    assert!(sb.repo_digest.is_some());
    std::fs::write(sb.root.join("new.txt"), "x\n").unwrap();
    assert_eq!(sb.diff_summary().unwrap().files, ["new.txt"]);

    // Editor metadata and identity are not guarded.
    git(&sb.root, &["config", "branch.main.vscode-merge-base", "origin/main"]);
    git(&sb.root, &["config", "user.name", "Someone"]);
    assert!(sb.diff_summary().is_ok());

    // Set from the worktree, it lands in the checkout's own config.
    git(&sb.root, &["config", "alias.st", "!echo planted"]);
    assert_eq!(git(&work, &["config", "--local", "alias.st"]), "!echo planted");
    let err = sb.diff_summary().unwrap_err().to_string();
    assert!(err.contains("changed while this run had a worktree"), "{err}");
    assert!(sb.diff_text().is_err() && sb.merge().is_err());
    git(&work, &["config", "--unset", "alias.st"]);
    assert!(sb.diff_summary().is_ok(), "back as it was");

    // A planted hook.
    let hook = work.join(".git").join("hooks").join("post-merge");
    std::fs::write(&hook, "#!/bin/sh\necho planted\n").unwrap();
    assert!(sb.merge().is_err());
    std::fs::remove_file(&hook).unwrap();
    assert!(matches!(sb.merge().unwrap(), MergeOutcome::FastForward { .. }));
    sb.discard().unwrap();
    remove(&base);
}

#[test]
fn the_daemons_git_calls_run_no_repository_hooks() {
    let base = temp_dir("hooks");
    let work = repo(&base);
    let marker = base.join("hook-ran");
    let script = format!("#!/bin/sh\necho ran >> '{}'\n", marker.display().to_string().replace('\\', "/"));
    let hooks = work.join(".git").join("hooks");
    for name in ["post-checkout", "post-commit", "post-merge", "pre-commit"] {
        std::fs::write(hooks.join(name), &script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(hooks.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    // The hooks work for an ordinary git call.
    git(&work, &["commit", "-q", "--allow-empty", "-m", "probe"]);
    assert!(marker.exists(), "the test hook did not run under plain git");
    std::fs::remove_file(&marker).unwrap();

    // Worktree add, commit, merge and discard by the daemon: none of them runs a hook.
    let sb = Sandbox::create(&base.join("home"), &RunId::new_random(), Some(&work), SandboxKind::Worktree).unwrap();
    std::fs::write(sb.root.join("crew.txt"), "crew\n").unwrap();
    assert_eq!(sb.diff_summary().unwrap().files, ["crew.txt"]);
    assert!(matches!(sb.merge().unwrap(), MergeOutcome::FastForward { .. }));
    sb.discard().unwrap();
    assert!(work.join("crew.txt").exists());
    assert!(!marker.exists(), "a repository hook ran during a daemon git call");
    remove(&base);
}

#[test]
fn shells_ask_for_writes_to_the_checkout_shared_git_state_and_paths_outside() {
    let base = temp_dir("policy");
    let root = base.join("worktrees").join("r1");
    let origin = base.join("proj");
    std::fs::create_dir_all(&root).unwrap();
    let sb = Sandbox {
        kind: SandboxKind::Worktree,
        root: root.clone(),
        origin: Some(origin.clone()),
        branch: Some("crew/r1".into()),
        base: Some("abc".into()),
        repo_digest: None,
    };
    let o = origin.display().to_string().replace('\\', "/");
    for free in [
        "git config user.email".to_string(),
        "git config --get core.autocrlf".into(),
        "git branch -a".into(),
        "git stash list".into(),
        "git remote -v".into(),
        "git worktree list".into(),
        format!("git -C {o} status"),
        format!("git -C {o} log --oneline -5"),
        "echo hi > out.txt".into(),
        "cargo test 2>&1".into(),
        "echo x > nul".into(),
        format!("cp {o}/README.md notes.md"),
        "python -c \"print(1 > 0)\"".into(),
    ] {
        assert!(!asks(sb.classify_shell(&free, false)), "{free} should run free");
    }
    for ask in [
        "git config core.hooksPath .hooks".to_string(),
        "git config --global user.name x".into(),
        "git config set alias.x y".into(),
        format!("git -C {o} checkout -- ."),
        format!("git -C {o} clean -fdx"),
        format!("git -C {o} stash"),
        format!("git --git-dir={o}/.git update-ref refs/heads/main HEAD"),
        "git update-ref refs/heads/main HEAD".into(),
        "git branch -D main".into(),
        "git branch -m main old".into(),
        "git tag -d v1".into(),
        "git stash clear".into(),
        "git remote set-url origin https://example.com/x.git".into(),
        "git worktree remove ../other".into(),
        "git checkout -B main".into(),
        format!("rm {o}/file.txt"),
        "del ../other/file.txt".into(),
        "echo x > ../escape.txt".into(),
        format!("echo x >> {o}/notes.md"),
        format!("cp notes.md {o}/notes.md"),
        "mv ../other/a.txt .".into(),
        format!("Set-Content -Path {o}/a.txt -Value x"),
        "set GIT_CONFIG_COUNT=0 && git status".into(),
        "env -u GIT_CONFIG_COUNT sh -c x".into(),
    ] {
        assert!(asks(sb.classify_shell(&ask, false)), "{ask} should ask");
    }

    // Code and scripts that would push.
    assert!(asks(sb.classify_code("import subprocess\nsubprocess.run(['git', 'push', 'origin'])")));
    assert!(asks(sb.classify_code("import os\nos.environ.pop('GIT_CONFIG_COUNT')")));
    assert!(!asks(sb.classify_code("print(sum(range(10)))")));
    std::fs::write(root.join("ship.sh"), "#!/bin/sh\ngit add -A\ngit push --force\n").unwrap();
    std::fs::write(root.join("build.sh"), "#!/bin/sh\ncargo build\n").unwrap();
    assert!(asks(sb.classify_shell("bash ship.sh", false)));
    assert!(!asks(sb.classify_shell("bash build.sh", false)));
    remove(&base);
}
