use serde_json::json;

use super::*;

fn denied(cmd: &str) -> bool {
    deny_command(cmd).is_some()
}

#[test]
fn every_spelling_of_force_is_denied() {
    for cmd in [
        "git push -f",
        "git push --force",
        "git push --force-with-lease",
        "git push --force-with-lease=main:abc123 origin main",
        "git push --force-if-includes origin main",
        "git push origin main --force",
        "git push origin +main",
        "git push origin +HEAD:main",
        "git push origin HEAD:+main",
        "git push --mirror",
        "git push --mirror origin",
        // git reads any unambiguous prefix of a long option.
        "git push --force-w origin main",
        "git push --force-with origin main",
        "git push --force-if origin main",
        "git push --forc origin main",
        "git push --mirr --dry-run",
        "git push --mir origin",
        "git push --m origin",
        "git push --pru origin 'refs/heads/*'",
        "git push --force>out",
        "git push --force-w>out origin main",
        "git push -f>out",
        "git push origin :main>out",
        "git remote add --mirror=push backup ../x.git",
        "git remote add --mirr=push backup ../x.git",
        // Force through config.
        "git -c remote.origin.mirror=true push",
        "git -c remote.origin.mirror push",
        "git -c remote.origin.push=+refs/heads/*:refs/heads/* push",
        "git --config-env=remote.origin.push=PUSHSPEC push",
        "git config remote.origin.push +refs/heads/*:refs/heads/*",
        "git config --add remote.origin.push +HEAD",
        "git config remote.origin.mirror true",
        "git config set remote.backup.mirror yes",
        "GIT_CONFIG_PARAMETERS=\"'remote.origin.mirror'='true'\" git push",
        "GIT_CONFIG_KEY_0=remote.origin.push GIT_CONFIG_VALUE_0=+HEAD git push",
        "git push --prune origin 'refs/heads/*'",
        "git push -fu origin feat",
        "git push -uf origin feat",
        "git push -vf",
        "git   push    -f",
        "git -C repo push -f",
        "git -c user.name=x push --force",
        "git --no-pager push -f",
        "git.exe push -f",
        "\"C:\\Program Files\\Git\\cmd\\git.exe\" push -f",
        "/usr/bin/git push --force",
        "GIT_TRACE=1 git push -f",
        "env GIT_TRACE=1 git push -f",
        "sudo git push -f",
        "sudo -u deploy git push -f",
        "timeout 30 git push -f",
        "nohup git push -f &",
        "cd repo && git push -f",
        "cd repo; git push --force-with-lease",
        "git fetch || git push -f",
        "echo ok | git push -f",
        "git status\ngit push --force",
        "(git push -f)",
        "{ git push -f; }",
        "if git push -f; then echo hi; fi",
        "bash -c 'git push -f'",
        "bash -lc \"git push --force origin main\"",
        "sh -c \"cd x && git push -f\"",
        "powershell -NoProfile -Command \"git push -f\"",
        "pwsh -Command 'git push --force-with-lease'",
        "cmd /c git push -f",
        "& git push -f",
        "& 'git' push -f",
        "eval git push -f",
        "echo $(git push -f)",
        "echo \"$(git push -f)\"",
        "x=`git push -f`",
        "xargs git push -f",
    ] {
        assert!(denied(cmd), "should deny: {cmd}");
    }
}

#[test]
fn hook_skipping_is_denied() {
    for cmd in [
        "git commit --no-verify -m x",
        "git commit -n -m x",
        "git commit -nm x",
        "git commit -anm x",
        "git commit -m x --no-verify",
        "git push --no-verify",
        "git merge --no-verify topic",
        "git rebase --no-verify main",
        "git -c core.hooksPath=/dev/null commit -m x",
        "git -c core.hookspath=NUL commit -m x",
        "git config core.hooksPath /dev/null",
        "bash -c 'git commit -n -m wip'",
        // Any one-shot hooksPath, whatever it points at.
        "git -c core.hooksPath=.nohooks commit --allow-empty -m d",
        "git -c core.hooksPath=.husky commit -m x",
        "git --config-env=core.hooksPath=HOOKS commit -m x",
        "git --config-env core.hookspath=HOOKS commit -m x",
        // Through the environment.
        "GIT_CONFIG_PARAMETERS=\"'core.hooksPath'='x'\" git commit -m x",
        "env GIT_CONFIG_PARAMETERS=\"'core.hooksPath'='x'\" git commit -m x",
        "GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.hooksPath GIT_CONFIG_VALUE_0=/tmp/none git commit -m x",
        // Abbreviated long options and a redirect glued to an option.
        "git commit -m b --no-verif",
        "git commit -m b --no-ver=",
        "git commit -m b --no-v",
        "git commit --no-verify>log -m b",
        "git commit --no-verif>log -m b",
    ] {
        assert!(denied(cmd), "should deny: {cmd}");
    }
}

#[test]
fn deleting_protected_branches_on_a_remote_is_denied() {
    for cmd in [
        "git push origin --delete main",
        "git push --delete origin master",
        "git push -d origin develop",
        "git push origin :main",
        "git push origin :refs/heads/release/1.0",
        "git push origin --delete release/2.1",
        "git push origin --delete origin/main",
        "git push origin --delete",
    ] {
        assert!(denied(cmd), "should deny: {cmd}");
    }
    // A feature branch may be deleted (the rules still ask).
    for cmd in ["git push origin --delete feat/x", "git push origin :feat/x", "git push -d origin tmp"] {
        assert!(!denied(cmd), "should leave to the rules: {cmd}");
    }
}

#[test]
fn ordinary_git_is_not_denied() {
    for cmd in [
        "git push",
        "git push -u origin feat/x",
        "git push origin HEAD",
        "git push --dry-run origin main",
        "git push -n origin main",
        "git push --tags",
        "git push -o ci.skip origin feat",
        "git commit -m 'fix -f handling'",
        "git commit -m \"use --force carefully\"",
        "git commit -am wip",
        "git commit -m x -S",
        "git log -n 5",
        "git diff -f",
        "git rebase -i main",
        "git rebase --onto main a b",
        "git cherry-pick abc123",
        "git cherry-pick -n abc123",
        "git merge --no-ff topic",
        "git reset --hard HEAD~1",
        "git tag -a v1 -m 'release'",
        "git worktree add ../wt -b feat",
        "git stash push -u",
        "git config user.name Matthew",
        "git config core.hooksPath .husky",
        "git config remote.origin.push refs/heads/main",
        "git config remote.origin.mirror false",
        "git config remote.origin.mirror",
        "git config --get remote.origin.push",
        "git -c remote.origin.mirror=false push",
        "git -c user.name=x -c push.default=simple push",
        "git push --follow-tags",
        "git push --dry-run origin main",
        "git push --delete origin feat/x",
        "git push --set-upstream origin feat",
        "git push --set-upstream-to=x",
        "git log --no-verbose",
        "git commit -m 'mention --no-verify'",
        "git commit -m x # fix: handle -n flag",
        "git push origin main # not -f",
        "git -C repo status",
        "echo git push -f",
        "echo 'git push --force' > notes.txt",
        "printf '%s' 'git push -f'",
        "grep -r 'git push -f' docs",
        "ls && pwd",
        "gitk --all",
        "legit push -f",
        "bash -c 'echo hi'",
        "",
    ] {
        assert!(!denied(cmd), "should not deny: {cmd}");
    }
}

#[test]
fn a_line_that_does_not_parse_is_still_denied() {
    for cmd in [
        "git push -f 'unterminated",
        "git push --force origin main  # don't worry",
        "git push origin main -f \"open",
        "echo \"a`\"b\"; git push --force",
        "echo \"it's\"; git push --force-with-lease origin main",
        "echo don't; git commit --no-verify -m x",
        "echo 'x; git commit -nm y",
        "bash -c \"git push -f",
        "echo \"$(git push -f",
        "cmd /c echo hi #& git push -f",
        "git push --for\"ce\" origin main 'x",
    ] {
        assert!(denied(cmd), "should deny: {cmd}");
    }
    // What would not be denied if it parsed is not denied when it does not.
    for cmd in ["echo \"open", "echo don't; git status", "git push origin main  # don't worry", "git log 'x"] {
        assert!(!denied(cmd), "should not deny: {cmd}");
    }
}

#[test]
fn structured_calls_refuse_force_and_hook_skips_and_option_injection() {
    for (tool, args) in [
        ("git", json!({"op": "push", "force": true})),
        ("git", json!({"op": "push", "force_with_lease": true})),
        ("git", json!({"op": "push", "force-with-lease": "origin"})),
        ("git", json!({"op": "push", "mirror": true})),
        ("git", json!({"op": "push", "branch": "+main"})),
        ("git", json!({"op": "push", "branch": "main:+main"})),
        ("git", json!({"op": "push", "branch": ":main"})),
        ("git", json!({"op": "push", "branch": "--force"})),
        ("git", json!({"op": "push", "remote": "-f"})),
        ("git", json!({"op": "commit", "message": "x", "no_verify": true})),
        ("git", json!({"op": "commit", "message": "x", "skip_hooks": true})),
        ("git", json!({"op": "branch", "name": "main", "delete": true})),
        ("git", json!({"op": "diff", "base": "--output=x"})),
        ("git", json!({"op": "switch", "name": "-f"})),
        ("gh", json!({"op": "pr_create", "title": "t", "force": true})),
        ("gh", json!({"op": "pr_create", "title": "t", "head": "--web"})),
    ] {
        assert!(deny_structured(tool, &args).is_some(), "should deny {tool} {args}");
    }
    for (tool, args) in [
        ("git", json!({"op": "push"})),
        ("git", json!({"op": "push", "branch": "feat/x", "set_upstream": true})),
        ("git", json!({"op": "commit", "message": "fix: -f is parsed", "force": false})),
        ("git", json!({"op": "diff", "base": "main...HEAD"})),
        ("git", json!({"op": "branch", "name": "feat/a", "delete": true})),
        ("gh", json!({"op": "pr_create", "title": "--force in a title is fine", "head": "feat/x"})),
    ] {
        assert_eq!(deny_structured(tool, &args), None, "{tool} {args}");
    }
}

#[test]
fn protected_branches() {
    for b in ["main", "master", "develop", "trunk", "Main", "origin/main", "refs/heads/master", "release/1.2", "releases/2"] {
        assert!(is_protected(b), "{b}");
    }
    for b in ["feat/main-menu", "mainline", "fix/master-key", "releasenotes"] {
        assert!(!is_protected(b), "{b}");
    }
}

#[test]
fn secret_shaped_paths() {
    for p in [".env", "app/.env.local", "prod.env", "keys/server.pem", "id_rsa", "~/.ssh/id_ed25519", "cert.p12", "credentials.json", ".npmrc", "infra/terraform.tfstate", "config\\secrets.yaml"] {
        assert!(secret_path(p).is_some(), "{p}");
    }
    for p in [".env.example", ".env.sample", "id_rsa.pub", "src/main.rs", "README.md", "docs/environment.md", "keyboard.rs", "tests/fixtures/pem_reader.rs"] {
        assert!(secret_path(p).is_none(), "{p}");
    }
}

#[test]
fn secret_shaped_content_in_a_diff() {
    let diff = "+++ b/src/a.rs\n+let x = 1;\n+++ b/deploy.sh\n+export TOKEN=ghp_abcdefghijklmnopqrstuvwxyz0123456789\n+++ b/k.txt\n+-----BEGIN OPENSSH PRIVATE KEY-----\n";
    let hits = secret_content(diff);
    assert_eq!(hits.iter().map(|(f, _)| f.as_str()).collect::<Vec<_>>(), ["deploy.sh", "k.txt"]);
    assert!(secret_content("+++ b/a.rs\n+let akia = \"AKIA\";\n-ghp_abcdefghijklmnopqrstuvwxyz0123456789\n").is_empty());
}

#[test]
fn pushes_are_recognised_in_command_lines() {
    for cmd in ["git push", "git push -u origin feat", "cd r && git push origin HEAD", "bash -c 'git push'", "git -C r push", "sudo git push", "(git push)"] {
        assert!(pushes(cmd), "{cmd}");
    }
    for cmd in ["git push --dry-run", "git push -n", "git status", "echo git push", "git commit -m 'git push'", "git pull", "git push-to-checkout x"] {
        assert!(!pushes(cmd), "{cmd}");
    }
}
