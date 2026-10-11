//! `git{op}`: the repository operations a coding agent needs, as structured calls (design 3.1).
//!
//! Reads (status, diff, log, show, blame, a branch list, stash list/show, and a fetch before any
//! untrusted read) are free. Everything that writes asks in default mode and is free inside a crew
//! worktree, except a push, deleting a branch, and anything that stages or commits a secret-shaped
//! file. No force, no hook skip, no refspec: [`policy`] refuses them before the approval gate and
//! again here. Rebase, merge, cherry-pick, reset, tag and worktree are bash's, under the rules.

use std::path::{Path, PathBuf};

use marlowe_agent::{CallCtx, Consequence, Exposure, Mutation, SandboxView, ToolCallReq, ToolCtx, ToolOutcome};
use serde_json::Value;

use super::run::{Env, Mode, Out, MAX_RESULT_BYTES, READ_TIMEOUT, WRITE_TIMEOUT};
use super::{policy, watch};
use crate::{first_line, int_arg, str_arg, text, BuiltinTools};

/// The operations, in the order the schema lists them.
pub(crate) const OPS: &[&str] =
    &["status", "diff", "log", "show", "blame", "branch", "switch", "add", "commit", "stash", "push", "fetch"];

pub(crate) fn flag(args: &Value, key: &str) -> bool {
    match args.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s.eq_ignore_ascii_case("true"),
        _ => false,
    }
}

/// A list argument: an array of strings, or one string.
pub(crate) fn list(args: &Value, key: &str) -> Vec<String> {
    match args.get(key) {
        Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str()).map(str::to_string).filter(|s| !s.is_empty()).collect(),
        Some(Value::String(s)) if !s.is_empty() => vec![s.clone()],
        _ => Vec::new(),
    }
}

fn refused(why: impl std::fmt::Display) -> ToolOutcome {
    ToolOutcome::error(format!("refused: {why}"))
}

/// A running call: where, how, and how to tell it was interrupted.
pub(crate) struct Ctx<'a> {
    pub env: &'a Env,
    pub dir: &'a Path,
    pub cancel: &'a dyn Fn() -> bool,
}

impl Ctx<'_> {
    pub fn read(&self, args: &[&str]) -> Out {
        self.env.git(self.dir, args, READ_TIMEOUT, Mode::Read, self.cancel)
    }

    pub fn write(&self, args: &[&str]) -> Out {
        self.env.git(self.dir, args, WRITE_TIMEOUT, Mode::Write, self.cancel)
    }

    /// A command that talks to a remote (push, fetch).
    pub fn net(&self, args: &[&str]) -> Out {
        self.env.git(self.dir, args, WRITE_TIMEOUT, Mode::Net, self.cancel)
    }

    /// Trimmed stdout of a read-only command that succeeded.
    pub fn out(&self, args: &[&str]) -> Option<String> {
        let o = self.read(args);
        o.ok().then(|| o.out().to_string())
    }

    pub fn branch(&self) -> Option<String> {
        self.out(&["branch", "--show-current"]).filter(|b| !b.is_empty())
    }

    /// The directory of the work tree's top.
    pub fn toplevel(&self) -> Option<PathBuf> {
        self.out(&["rev-parse", "--show-toplevel"]).map(PathBuf::from)
    }
}

pub(crate) fn git_call(tools: &BuiltinTools, args: &Value, cx: &CallCtx) -> ToolOutcome {
    if let Some(why) = policy::deny_structured("git", args) {
        return refused(why);
    }
    let Some(op) = str_arg(args, "op") else {
        return ToolOutcome::error(format!("`op` is required: one of {}", OPS.join(", ")));
    };
    let env = Env::of(tools);
    let dir = match env.dir(cx.workspace.as_deref(), str_arg(args, "cwd")) {
        Ok(d) => d,
        Err(e) => return ToolOutcome::error(e),
    };
    let cancel = || cx.cancel.is_cancelled();
    let c = Ctx { env: &env, dir: &dir, cancel: &cancel };
    match op {
        "status" => status(&c),
        "diff" => diff(&c, args),
        "log" => log(&c, args),
        "show" => show(&c, args),
        "blame" => blame(&c, args),
        "branch" => branch(&c, args),
        "switch" => switch(&c, args),
        "add" => add(&c, args),
        "commit" => commit(&c, args),
        "stash" => stash(&c, args),
        "push" => push(&c, args),
        "fetch" => fetch(&c, args),
        other => ToolOutcome::error(format!("unknown git op `{other}`; use one of {}", OPS.join(", "))),
    }
}

fn bounded(s: &str) -> String {
    text::head_and_tail(s, MAX_RESULT_BYTES)
}

fn fail(o: &Out, what: &str) -> ToolOutcome {
    ToolOutcome::error(o.failure(what))
}

/// "In the middle of a rebase/merge/..." from the git directory's marker files.
fn in_progress(c: &Ctx) -> Option<&'static str> {
    let git_dir = c.out(&["rev-parse", "--absolute-git-dir"])?;
    let g = Path::new(&git_dir);
    [
        ("rebase-merge", "a rebase is in progress; continue it with bash (`git rebase --continue`) or abort it"),
        ("rebase-apply", "a rebase or am is in progress"),
        ("MERGE_HEAD", "a merge is in progress; resolve the conflicts and commit, or `git merge --abort`"),
        ("CHERRY_PICK_HEAD", "a cherry-pick is in progress"),
        ("REVERT_HEAD", "a revert is in progress"),
        ("BISECT_LOG", "a bisect is in progress"),
    ]
    .into_iter()
    .find(|(f, _)| g.join(f).exists())
    .map(|(_, note)| note)
}

fn status(c: &Ctx) -> ToolOutcome {
    let o = c.read(&["status", "--short", "--branch", "--untracked-files=normal"]);
    if !o.ok() {
        return fail(&o, "git status");
    }
    let changed = o.out().lines().filter(|l| !l.starts_with("##")).count();
    let mut out = o.out().to_string();
    if changed == 0 {
        out.push_str("\n(working tree clean)");
    }
    if let Some(note) = in_progress(c) {
        out.push_str(&format!("\n({note})"));
    }
    let summary = if changed == 0 { "clean".to_string() } else { format!("{changed} changed") };
    ToolOutcome::ok(bounded(&out), summary)
}

fn paths_tail(args: &Value) -> Vec<String> {
    let paths = list(args, "paths");
    if paths.is_empty() {
        return paths;
    }
    let mut v = vec!["--".to_string()];
    v.extend(paths);
    v
}

fn plus_minus(diff: &str) -> (usize, usize) {
    let add = diff.lines().filter(|l| l.starts_with('+') && !l.starts_with("+++")).count();
    let del = diff.lines().filter(|l| l.starts_with('-') && !l.starts_with("---")).count();
    (add, del)
}

fn diff(c: &Ctx, args: &Value) -> ToolOutcome {
    let mut a: Vec<String> = ["diff", "--no-ext-diff", "--no-textconv", "--no-color"].map(String::from).to_vec();
    if flag(args, "staged") {
        a.push("--cached".into());
    }
    let shape = if flag(args, "stat") {
        a.push("--stat".into());
        true
    } else if flag(args, "names_only") {
        a.push("--name-status".into());
        true
    } else {
        false
    };
    if let Some(n) = int_arg(args, "context").filter(|n| (0..=100).contains(n)) {
        a.push(format!("-U{n}"));
    }
    if let Some(base) = str_arg(args, "base").filter(|b| !b.is_empty()) {
        a.push(base.to_string());
    }
    a.extend(paths_tail(args));
    let refs: Vec<&str> = a.iter().map(String::as_str).collect();
    let o = c.read(&refs);
    if !o.ok() {
        return fail(&o, "git diff");
    }
    if o.out().is_empty() {
        let what = if flag(args, "staged") { "nothing is staged" } else { "no changes" };
        return ToolOutcome::ok(format!("({what}; untracked files are not part of a diff, see git status)"), "no changes");
    }
    let summary = if shape {
        format!("{} lines", o.out().lines().count())
    } else {
        let (add, del) = plus_minus(o.out());
        format!("+{add} −{del}")
    };
    ToolOutcome::ok(bounded(o.out()), summary)
}

fn log(c: &Ctx, args: &Value) -> ToolOutcome {
    let limit = int_arg(args, "limit").unwrap_or(20).clamp(1, 200);
    let mut a: Vec<String> = vec![
        "log".into(),
        "--no-color".into(),
        "--date=short".into(),
        "--format=%h %ad %an  %s%d".into(),
        format!("-n{limit}"),
    ];
    for (key, opt) in [("author", "--author="), ("grep", "--grep="), ("since", "--since=")] {
        if let Some(v) = str_arg(args, key).filter(|v| !v.is_empty()) {
            a.push(format!("{opt}{v}"));
        }
    }
    if let Some(rev) = str_arg(args, "rev").filter(|r| !r.is_empty()) {
        a.push(rev.to_string());
    }
    a.extend(paths_tail(args));
    let refs: Vec<&str> = a.iter().map(String::as_str).collect();
    let o = c.read(&refs);
    if !o.ok() {
        return fail(&o, "git log");
    }
    if o.out().is_empty() {
        return ToolOutcome::ok("(no commits)", "0 commits");
    }
    let n = o.out().lines().count();
    ToolOutcome::ok(bounded(o.out()), format!("{n} commit{}", if n == 1 { "" } else { "s" }))
}

fn show(c: &Ctx, args: &Value) -> ToolOutcome {
    let rev = str_arg(args, "rev").filter(|r| !r.is_empty()).unwrap_or("HEAD");
    let mut a: Vec<String> = ["show", "--no-ext-diff", "--no-textconv", "--no-color", "--date=short"].map(String::from).to_vec();
    a.push("--stat".into());
    if !flag(args, "stat") {
        a.push("--patch".into());
    }
    a.push(rev.to_string());
    a.extend(paths_tail(args));
    let refs: Vec<&str> = a.iter().map(String::as_str).collect();
    let o = c.read(&refs);
    if !o.ok() {
        return fail(&o, "git show");
    }
    let subject = o.out().lines().find(|l| l.starts_with("    ")).map(|l| first_line(l, 60)).unwrap_or_default();
    ToolOutcome::ok(bounded(o.out()), subject)
}

fn blame(c: &Ctx, args: &Value) -> ToolOutcome {
    let Some(path) = list(args, "paths").into_iter().next().or_else(|| str_arg(args, "path").map(String::from)) else {
        return ToolOutcome::error("blame needs `paths` (the file to blame)");
    };
    let mut a: Vec<String> = ["blame", "--date=short", "-w"].map(String::from).to_vec();
    if let Some(start) = int_arg(args, "start").filter(|n| *n >= 1) {
        let end = int_arg(args, "end").filter(|e| *e >= start).unwrap_or(start + 40);
        a.push(format!("-L{start},{end}"));
    }
    if let Some(rev) = str_arg(args, "rev").filter(|r| !r.is_empty()) {
        a.push(rev.to_string());
    }
    a.push("--".into());
    a.push(path);
    let refs: Vec<&str> = a.iter().map(String::as_str).collect();
    let o = c.read(&refs);
    if !o.ok() {
        return fail(&o, "git blame");
    }
    ToolOutcome::ok(bounded(o.out()), format!("{} lines", o.out().lines().count()))
}

fn branch(c: &Ctx, args: &Value) -> ToolOutcome {
    let name = str_arg(args, "name").filter(|n| !n.is_empty());
    let Some(name) = name else {
        let mut a = vec!["branch", "--list", "-vv", "--no-color"];
        if flag(args, "all") {
            a.push("--all");
        }
        let o = c.read(&a);
        if !o.ok() {
            return fail(&o, "git branch");
        }
        return ToolOutcome::ok(bounded(o.out()), format!("{} branches", o.out().lines().count()));
    };
    if flag(args, "delete") {
        // `-d` only: git refuses a branch that is not merged.
        let o = c.write(&["branch", "-d", name]);
        return if o.ok() { ToolOutcome::ok(o.all(), format!("deleted {name}")) } else { fail(&o, "git branch -d") };
    }
    let from = str_arg(args, "from").filter(|f| !f.is_empty());
    let mut a = vec!["branch", name];
    a.extend(from);
    let o = c.write(&a);
    if !o.ok() {
        return fail(&o, "git branch");
    }
    let at = c.out(&["rev-parse", "--short", name]).unwrap_or_default();
    ToolOutcome::ok(format!("created branch {name} at {at}"), format!("created {name}"))
}

fn switch(c: &Ctx, args: &Value) -> ToolOutcome {
    let Some(name) = str_arg(args, "name").filter(|n| !n.is_empty()) else {
        return ToolOutcome::error("switch needs `name` (the branch)");
    };
    let mut a = vec!["switch"];
    if flag(args, "create") {
        a.push("-c");
    }
    a.push(name);
    if let Some(from) = str_arg(args, "from").filter(|f| !f.is_empty()) {
        a.push(from);
    }
    let o = c.write(&a);
    if !o.ok() {
        return fail(&o, "git switch");
    }
    let said = o.all();
    ToolOutcome::ok(if said.is_empty() { format!("switched to {name}") } else { said }, format!("on {name}"))
}

fn add(c: &Ctx, args: &Value) -> ToolOutcome {
    let paths = list(args, "paths");
    let mut a: Vec<String> = vec!["add".into()];
    if flag(args, "all") {
        a.push("-A".into());
    } else if flag(args, "update") {
        a.push("-u".into());
    } else if paths.is_empty() {
        return ToolOutcome::error("add needs `paths` (or `all`: true to stage every change, `update`: true for tracked files only)");
    } else {
        a.push("--".into());
        a.extend(paths);
    }
    let refs: Vec<&str> = a.iter().map(String::as_str).collect();
    let o = c.write(&refs);
    if !o.ok() {
        return fail(&o, "git add");
    }
    let staged = c.out(&["diff", "--cached", "--stat", "--no-color"]).unwrap_or_default();
    let n = staged.lines().filter(|l| l.contains('|')).count();
    let body = if staged.is_empty() { "(nothing is staged)".to_string() } else { bounded(&staged) };
    ToolOutcome::ok(format!("{}{body}", if o.all().is_empty() { String::new() } else { format!("{}\n", o.all()) }), format!("{n} staged"))
}

fn commit(c: &Ctx, args: &Value) -> ToolOutcome {
    let message = str_arg(args, "message").map(str::trim).unwrap_or("");
    let amend = flag(args, "amend");
    if message.is_empty() && !amend {
        return ToolOutcome::error("commit needs `message`");
    }
    if message.len() > 20_000 {
        return ToolOutcome::error("the commit message is over 20,000 characters");
    }
    let mut a: Vec<String> = vec!["commit".into()];
    if !message.is_empty() {
        a.push("-m".into());
        a.push(message.to_string());
    }
    if amend {
        a.push("--amend".into());
        if message.is_empty() {
            a.push("--no-edit".into());
        }
    }
    if flag(args, "all") {
        a.push("-a".into());
    }
    a.extend(paths_tail(args));
    let refs: Vec<&str> = a.iter().map(String::as_str).collect();
    if let Some(why) = policy::deny_git(&refs) {
        return refused(why);
    }
    let o = c.write(&refs);
    if !o.ok() {
        return ToolOutcome::error(format!(
            "{}\n(hooks run on every commit and are never skipped; fix what they report and commit again)",
            o.failure("git commit")
        ));
    }
    let line = c.out(&["log", "-1", "--format=%h %s"]).unwrap_or_default();
    let stat = c.out(&["show", "--stat", "--format=", "--no-color", "HEAD"]).unwrap_or_default();
    let branch = c.branch().unwrap_or_else(|| "detached HEAD".into());
    let short = line.split(' ').next().unwrap_or("").to_string();
    ToolOutcome::ok(format!("committed on {branch}: {line}\n{}", bounded(&stat)), format!("committed {short}"))
}

fn stash(c: &Ctx, args: &Value) -> ToolOutcome {
    let action = str_arg(args, "action").filter(|a| !a.is_empty()).unwrap_or("push");
    let rev = str_arg(args, "rev").filter(|r| !r.is_empty());
    let mut a: Vec<String> = vec!["stash".into()];
    match action {
        "push" | "save" => {
            a.push("push".into());
            if flag(args, "include_untracked") {
                a.push("-u".into());
            }
            if let Some(m) = str_arg(args, "message").filter(|m| !m.is_empty()) {
                a.push("-m".into());
                a.push(m.to_string());
            }
            a.extend(paths_tail(args));
        }
        "list" => a.push("list".into()),
        "show" => {
            a.extend(["show".into(), "--stat".into(), "--no-color".into()]);
            a.extend(rev.map(String::from));
        }
        "pop" | "apply" | "drop" => {
            a.push(action.into());
            a.extend(rev.map(String::from));
        }
        other => return ToolOutcome::error(format!("unknown stash action `{other}`; use push, list, show, pop, apply or drop")),
    }
    let refs: Vec<&str> = a.iter().map(String::as_str).collect();
    let o = if matches!(action, "list" | "show") { c.read(&refs) } else { c.write(&refs) };
    if !o.ok() {
        return fail(&o, "git stash");
    }
    let said = o.all();
    ToolOutcome::ok(if said.is_empty() { "(nothing)".into() } else { bounded(&said) }, format!("stash {action}"))
}

/// The remote a branch pushes to: the argument, the branch's configured remote, else `origin`.
pub(crate) fn remote_for(c: &Ctx, args: &Value, branch: &str) -> String {
    str_arg(args, "remote")
        .filter(|r| !r.is_empty())
        .map(String::from)
        .or_else(|| c.out(&["config", "--get", &format!("branch.{branch}.remote")]).filter(|r| !r.is_empty() && r != "."))
        .unwrap_or_else(|| "origin".into())
}

fn push(c: &Ctx, args: &Value) -> ToolOutcome {
    let Some(branch) = str_arg(args, "branch").filter(|b| !b.is_empty()).map(String::from).or_else(|| c.branch()) else {
        return ToolOutcome::error("HEAD is detached: name the `branch` to push, or switch to one");
    };
    let remote = remote_for(c, args, &branch);
    if c.out(&["remote", "get-url", &remote]).is_none() {
        return ToolOutcome::error(format!("no remote named `{remote}` (git remote -v lists them)"));
    }
    if let Some(expected) = str_arg(args, "expect_sha").filter(|s| !s.is_empty()) {
        let now = c.out(&["rev-parse", &format!("refs/heads/{branch}")]).unwrap_or_default();
        if now != expected {
            return ToolOutcome::error(format!(
                "`{branch}` moved after the push was approved (approved {}, now {}); nothing was pushed. Ask again to push what is there now",
                expected.chars().take(7).collect::<String>(),
                now.chars().take(7).collect::<String>()
            ));
        }
    }
    let has_upstream = c.out(&["rev-parse", "--abbrev-ref", &format!("{branch}@{{upstream}}")]).is_some();
    let spec = format!("refs/heads/{branch}:refs/heads/{branch}");
    let mut a = vec!["push"];
    if flag(args, "set_upstream") || !has_upstream {
        a.push("--set-upstream");
    }
    a.push(&remote);
    a.push(&spec);
    if let Some(why) = policy::deny_git(&a) {
        return refused(why);
    }
    let o = c.net(&a);
    if !o.ok() {
        let mut msg = o.failure("git push");
        if o.all().contains("rejected") || o.all().contains("non-fast-forward") {
            msg.push_str("\n(the remote has commits you do not: fetch, then rebase or merge, then push again; force pushes are not available)");
        }
        return ToolOutcome::error(msg);
    }
    register_ci(c, args, &branch, &remote);
    let said = o.all();
    ToolOutcome::ok(if said.is_empty() { format!("pushed {branch} to {remote}") } else { bounded(&said) }, format!("pushed {branch}"))
}

/// Records a pushed branch for the Sentinel's CI probe. Best effort.
pub(crate) fn register_ci(c: &Ctx, _args: &Value, branch: &str, remote: &str) {
    let Some(home) = c.env.state.home() else { return };
    let (Some(repo), Some(sha)) = (c.toplevel(), c.out(&["rev-parse", &format!("refs/heads/{branch}")])) else { return };
    let slug = c.out(&["remote", "get-url", remote]).and_then(|u| watch::github_slug(&u));
    let _ = watch::register(
        home,
        &watch::Watch { repo: repo.to_string_lossy().into_owned(), branch: branch.into(), sha, remote: remote.into(), slug },
    );
}

fn fetch(c: &Ctx, args: &Value) -> ToolOutcome {
    let mut a = vec!["fetch"];
    let remote = str_arg(args, "remote").filter(|r| !r.is_empty());
    a.extend(remote);
    let o = c.net(&a);
    if !o.ok() {
        return fail(&o, "git fetch");
    }
    let said = o.all();
    ToolOutcome::ok(if said.is_empty() { "(already up to date)".into() } else { bounded(&said) }, "fetched")
}

// ---------------------------------------------------------------- consequence

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Read,
    /// Changes the local repository.
    Local,
    BranchDelete,
    Push,
    Fetch,
}

fn classify(args: &Value) -> Class {
    match str_arg(args, "op").unwrap_or("") {
        "status" | "diff" | "log" | "show" | "blame" => Class::Read,
        "branch" => match (str_arg(args, "name").is_some_and(|n| !n.is_empty()), flag(args, "delete")) {
            (false, _) => Class::Read,
            (true, true) => Class::BranchDelete,
            (true, false) => Class::Local,
        },
        "stash" if matches!(str_arg(args, "action"), Some("list" | "show")) => Class::Read,
        "stash" | "switch" | "add" | "commit" => Class::Local,
        "push" => Class::Push,
        "fetch" => Class::Fetch,
        // Not an op: the call reports that, with nothing to ask about.
        _ => Class::Read,
    }
}

/// The directory a consequence check looks at.
fn check_dir(env: &Env, call: &ToolCallReq) -> Option<PathBuf> {
    env.dir(None, str_arg(&call.args, "cwd")).ok()
}

/// Secret-shaped things about to be staged or committed: `(label, why)`.
fn secrets_for(c: &Ctx, args: &Value, class_op: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut note = |name: &str, what: &str| {
        let s = format!("{name} ({what})");
        if !found.contains(&s) {
            found.push(s);
        }
    };
    match class_op {
        "add" => {
            let explicit = list(args, "paths");
            if !explicit.is_empty() {
                for p in &explicit {
                    if let Some(what) = policy::secret_path(p) {
                        note(p, what);
                    }
                }
            } else if let Some(porcelain) = c.out(&["status", "--porcelain", "--untracked-files=all"]) {
                for line in porcelain.lines() {
                    let name = line.get(3..).unwrap_or("").trim().trim_matches('"');
                    if let Some(what) = policy::secret_path(name) {
                        note(name, what);
                    }
                }
            }
        }
        _ => {
            let base: &[&str] = if flag(args, "all") { &["diff", "HEAD"] } else { &["diff", "--cached"] };
            let mut names = base.to_vec();
            names.extend(["--name-only", "--no-color"]);
            if let Some(list) = c.out(&names) {
                for name in list.lines() {
                    if let Some(what) = policy::secret_path(name) {
                        note(name, what);
                    }
                }
            }
            let mut body = base.to_vec();
            body.extend(["-U0", "--no-color", "--no-ext-diff", "--no-textconv"]);
            if let Some(d) = c.out(&body) {
                let d = &d[..text::floor_boundary(&d, 600_000)];
                for (file, what) in policy::secret_content(d) {
                    note(&file, what);
                }
            }
        }
    }
    found
}

fn with_secrets(base: String, secrets: &[String]) -> String {
    if secrets.is_empty() {
        base
    } else {
        format!("{base}; WARNING secret-shaped: {}", secrets.join(", "))
    }
}

/// What the approval modal says for a call that asks.
fn ask_summary(c: &Ctx, call: &ToolCallReq, class: Class) -> (String, Vec<String>) {
    let args = &call.args;
    let op = str_arg(args, "op").unwrap_or("");
    match (op, class) {
        ("commit", _) => {
            let msg = first_line(str_arg(args, "message").unwrap_or("(amend)"), 70);
            let branch = c.branch().unwrap_or_else(|| "detached HEAD".into());
            let stat = if flag(args, "all") { c.out(&["diff", "HEAD", "--shortstat"]) } else { c.out(&["diff", "--cached", "--shortstat"]) }
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .map(|s| format!(" ({s})"))
                .unwrap_or_default();
            let amend = if flag(args, "amend") { " (amend)" } else { "" };
            let secrets = secrets_for(c, args, "commit");
            let protected = if policy::is_protected(&branch) { " (a protected branch: consider a feature branch first)" } else { "" };
            (format!("git commit{amend} on {branch}{protected}: \"{msg}\"{stat}"), secrets)
        }
        ("add", _) => {
            let what = if flag(args, "all") {
                "everything".to_string()
            } else if flag(args, "update") {
                "tracked changes".to_string()
            } else {
                first_line(&list(args, "paths").join(" "), 80)
            };
            (format!("git add {what}"), secrets_for(c, args, "add"))
        }
        ("push", _) => {
            let branch = str_arg(args, "branch").map(String::from).or_else(|| c.branch()).unwrap_or_else(|| "HEAD".into());
            let remote = remote_for(c, args, &branch);
            let url = c.out(&["remote", "get-url", &remote]).map(|u| redact_url(&u)).unwrap_or_default();
            let tracked = format!("refs/remotes/{remote}/{branch}");
            let count = if c.out(&["rev-parse", "--verify", "--quiet", &tracked]).is_some() {
                c.out(&["rev-list", "--count", &format!("{tracked}..refs/heads/{branch}")]).map(|n| (n, ""))
            } else {
                c.out(&["rev-list", "--count", &format!("refs/heads/{branch}"), "--not", &format!("--remotes={remote}")]).map(|n| (n, " (new branch)"))
            };
            let ahead = count
                .map(|(n, new)| format!(": {n} commit{}{new}", if n == "1" { "" } else { "s" }))
                .unwrap_or_default();
            let protected = if policy::is_protected(&branch) { format!(" — WARNING: {branch} is a protected branch") } else { String::new() };
            (format!("git push {branch} to {remote} ({url}){ahead}{protected}"), Vec::new())
        }
        (_, Class::BranchDelete) => (format!("git branch -d {}", str_arg(args, "name").unwrap_or("?")), Vec::new()),
        (_, Class::Fetch) => (format!("git fetch {}(after untrusted content)", str_arg(args, "remote").map(|r| format!("{r} ")).unwrap_or_default()), Vec::new()),
        ("branch", _) => (format!("git branch {}", str_arg(args, "name").unwrap_or("?")), Vec::new()),
        ("switch", _) => (format!("git switch {}{}", if flag(args, "create") { "-c " } else { "" }, str_arg(args, "name").unwrap_or("?")), Vec::new()),
        ("stash", _) => (format!("git stash {}", str_arg(args, "action").unwrap_or("push")), Vec::new()),
        _ => (format!("git {op}"), Vec::new()),
    }
}

/// A remote URL without credentials in it.
pub(crate) fn redact_url(url: &str) -> String {
    match (url.find("://"), url.rfind('@')) {
        (Some(i), Some(at)) if at > i => format!("{}{}", &url[..i + 3], &url[at + 1..]),
        _ => url.to_string(),
    }
}

pub(crate) fn git_consequence(tools: &BuiltinTools, call: &ToolCallReq, exposure: &Exposure) -> Consequence {
    consequence(tools, call, exposure, None)
}

/// In a crew sandbox local writes are free (it is the run's own worktree); a push, a branch
/// delete and secrets still ask.
pub(crate) fn git_crew_consequence(
    tools: &BuiltinTools,
    call: &ToolCallReq,
    exposure: &Exposure,
    sandbox: &dyn SandboxView,
) -> Consequence {
    consequence(tools, call, exposure, Some(sandbox))
}

fn consequence(tools: &BuiltinTools, call: &ToolCallReq, exposure: &Exposure, sandbox: Option<&dyn SandboxView>) -> Consequence {
    if policy::deny_structured("git", &call.args).is_some() {
        // Refused by the call itself, with the reason; asking would only invite a yes.
        return Consequence::Free;
    }
    let class = classify(&call.args);
    if class == Class::Read {
        return Consequence::Free;
    }
    if class == Class::Fetch && !exposure.tainted {
        return Consequence::Free;
    }
    let env = Env::of(tools);
    let Some(dir) = check_dir(&env, call) else { return Consequence::Free };
    let cancel = || false;
    let c = Ctx { env: &env, dir: &dir, cancel: &cancel };
    let (summary, secrets) = ask_summary(&c, call, class);
    if sandbox.is_some() && matches!(class, Class::Local) {
        // The run's own worktree: free unless it stages a secret.
        if secrets.is_empty() {
            return Consequence::Free;
        }
        return Consequence::Ask { summary: with_secrets(summary, &secrets) };
    }
    Consequence::Ask { summary: with_secrets(summary, &secrets) }
}

/// Switching branches and stashing change the working tree; the rest change only `.git`.
pub(crate) fn git_mutation(tools: &BuiltinTools, call: &ToolCallReq, ctx: &ToolCtx) -> Mutation {
    let changes_files = match str_arg(&call.args, "op").unwrap_or("") {
        "switch" => true,
        "stash" => !matches!(str_arg(&call.args, "action"), Some("list" | "show")),
        _ => false,
    };
    if !changes_files {
        return Mutation::None;
    }
    match Env::of(tools).dir(ctx.workspace.as_deref(), str_arg(&call.args, "cwd")) {
        Ok(dir) => Mutation::Under(dir),
        Err(_) => Mutation::None,
    }
}

pub(crate) fn describe(call: &ToolCallReq) -> String {
    let a = &call.args;
    let op = str_arg(a, "op").unwrap_or("");
    let detail = match op {
        "commit" => first_line(str_arg(a, "message").unwrap_or(""), 50),
        "switch" | "branch" => str_arg(a, "name").unwrap_or("").to_string(),
        "add" => first_line(&list(a, "paths").join(" "), 50),
        "show" | "log" | "diff" => str_arg(a, "rev").or_else(|| str_arg(a, "base")).unwrap_or("").to_string(),
        "push" | "fetch" => str_arg(a, "branch").or_else(|| str_arg(a, "remote")).unwrap_or("").to_string(),
        _ => String::new(),
    };
    if detail.is_empty() {
        format!("git {op}")
    } else {
        format!("git {op} {detail}")
    }
}
