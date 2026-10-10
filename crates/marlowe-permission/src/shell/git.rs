//! `git` invocations: hard denies (force pushes, `--no-verify`, deleting protected branches),
//! fixed approvals (every push, config injection, global config writes), and, in a crew sandbox,
//! writes outside the worktree or to what it shares with the checkout (design 2.8 G7, G9).

use std::path::{Path, PathBuf};

use super::net::host_of_url;
use super::paths::resolve;
use super::{Analysis, Dialect, SandboxKind, Scope, ShellCtx, Word};

/// `git -c key=value` keys whose value runs a program or redirects where git talks to.
const RISKY_CONFIG: &[&str] = &[
    "core.sshcommand", "core.hookspath", "core.fsmonitor", "core.editor", "core.askpass", "credential.helper",
    "sequence.editor", "diff.external", "gpg.program", "core.gitproxy", "http.proxy", "https.proxy",
    "protocol.allow", "uploadpack.packobjectshook", "include.path", "core.worktree",
];

/// Facts about one `git` command that the caller folds into its [`super::Simple`].
pub(crate) struct GitFacts {
    pub read_only: bool,
    /// Shell code git hands to `sh -c` (`rebase -x`, `submodule foreach`, filter-branch's
    /// filters, `difftool -x`, `--upload-pack`): the caller walks it as bash.
    pub code: Vec<Word>,
    /// Commands git runs as given (`bisect run <cmd...>`): the caller walks each.
    pub argv: Vec<Vec<Word>>,
}

/// Examines `git <args>` run in `cwd`, recording denies, asks and hosts in `a`.
pub(crate) fn examine(dialect: Dialect, args: &[Word], cwd: Option<&Path>, ctx: &ShellCtx<'_>, a: &mut Analysis) -> GitFacts {
    let mut dir: Option<PathBuf> = cwd.map(Path::to_path_buf);
    let mut elsewhere = false;
    // Any `-c` override: it may change what a read-only subcommand runs (a pager, a filter).
    let mut overrides = false;
    let mut i = 0;
    while i < args.len() {
        let w = &args[i];
        let t = w.text.as_str();
        if w.dynamic && t.starts_with('-') {
            a.ask("git with options chosen at run time");
        }
        match t {
            "-C" => {
                dir = match (dir.as_deref(), args.get(i + 1)) {
                    (Some(c), Some(d)) if !d.dynamic => resolve(dialect, c, &d.text),
                    _ => None,
                };
                i += 2;
            }
            "-c" => {
                overrides = true;
                if let Some(kv) = args.get(i + 1) {
                    config_injection(&kv.text, kv.dynamic, a);
                }
                i += 2;
            }
            "--git-dir" | "--work-tree" | "--namespace" | "--exec-path" | "--super-prefix" => {
                elsewhere = true;
                if t == "--exec-path" {
                    a.ask("git --exec-path (runs other git programs)");
                }
                i += 2;
            }
            _ if t.starts_with("--config-env") => {
                a.ask("git --config-env");
                i += 1;
            }
            _ if t.starts_with('-') => {
                elsewhere |= t.starts_with("--git-dir=") || t.starts_with("--work-tree=");
                if t.starts_with("--exec-path=") {
                    a.ask("git --exec-path (runs other git programs)");
                }
                if let Some(v) = t.strip_prefix("-c") {
                    if !v.is_empty() {
                        overrides = true;
                        config_injection(v, w.dynamic, a);
                    }
                }
                i += 1;
            }
            _ => break,
        }
    }
    let Some(sub_word) = args.get(i) else {
        return GitFacts { read_only: true, code: Vec::new(), argv: Vec::new() };
    };
    if sub_word.dynamic {
        a.ask("git subcommand chosen at run time");
        return GitFacts { read_only: false, code: Vec::new(), argv: Vec::new() };
    }
    let sub = sub_word.text.to_ascii_lowercase();
    let flags: Vec<&str> = args[i + 1..].iter().map(|w| w.text.as_str()).collect();
    let pos: Vec<&str> = flags.iter().copied().filter(|f| !f.starts_with('-')).collect();
    let short = |c: char| flags.iter().any(|f| f.starts_with('-') && !f.starts_with("--") && f[1..].contains(c));

    // Never skip hooks (G7), in any subcommand. `--no-ver` is as far as git abbreviates it
    // (`--no-verbose` shares `--no-ve`); a shorter ambiguous spelling is git's own error.
    if long_prefix(&flags, "--no-verify", 8) || (sub == "commit" && short('n')) {
        a.deny("git --no-verify (skips the repository's hooks)");
    }

    match sub.as_str() {
        "push" => push(&flags, &pos, ctx, a),
        "send-pack" | "http-push" => a.ask("git push"),
        "clone" | "fetch" | "pull" | "ls-remote" | "submodule" | "remote" => {
            for p in &pos {
                if let Some(h) = host_of_url(p) {
                    if !super::net::is_local(&h) {
                        a.host(h);
                    }
                }
            }
        }
        "config" if config_writes(&flags) && (long_prefix(&flags, "--global", 4) || long_prefix(&flags, "--system", 4)) => {
            a.ask("git config --global/--system (outside the workspace)");
        }
        "config" if config_writes(&flags) => {
            if let Some(key) = pos.first() {
                config_injection(&format!("{key}=x"), false, a);
            }
        }
        _ => {}
    }
    if sub == "clone" && pos.len() >= 2 {
        if let Some(target) = pos.last().and_then(|p| dir.as_deref().and_then(|d| resolve(dialect, d, p))) {
            if !ctx.writable(&target) {
                a.ask("git clone outside the workspace");
            }
        }
    }

    let (code, argv) = embedded(&sub, &args[i + 1..]);
    let read_only = !overrides && code.is_empty() && argv.is_empty() && read_only(&sub, &flags, &pos);
    if let Scope::Sandbox { root, kind, .. } = ctx.scope {
        sandbox_rules(&sub, &flags, &pos, root, kind, dir.as_deref(), elsewhere, read_only, a);
    } else if !read_only && (elsewhere || dir.as_deref().is_none_or(|d| !ctx.writable(d))) {
        a.ask("git writes outside the workspace");
    }
    GitFacts { read_only, code, argv }
}

/// The shell code and commands a git subcommand runs from its arguments.
fn embedded(sub: &str, rest: &[Word]) -> (Vec<Word>, Vec<Vec<Word>>) {
    let mut code = Vec::new();
    let mut argv = Vec::new();
    // `--name value`, `--name=value` or, for a short option, `-xvalue` / `-ix value` (a
    // cluster ending in the letter takes the next word). `stops` are the other short options
    // that take a value: the rest of a cluster after one of them is that value.
    let values = |longs: &[(&str, usize)], short: Option<(char, &str)>| -> Vec<Word> {
        let mut out = Vec::new();
        let mut j = 0;
        while j < rest.len() {
            let w = &rest[j];
            let t = w.text.as_str();
            if t == "--" {
                break;
            }
            let next = || rest.get(j + 1).cloned();
            let (name, inline) = match t.split_once('=') {
                Some((n, v)) if n.starts_with("--") => (n, Some(v)),
                _ => (t, None),
            };
            if name.starts_with("--") && longs.iter().any(|(full, min)| name.len() >= *min && full.starts_with(name)) {
                match inline {
                    Some(v) => out.push(Word { text: v.to_string(), dynamic: w.dynamic, quoted: w.quoted }),
                    None => out.extend(next()),
                }
            } else if let (Some((c, stops)), true) = (short, t.starts_with('-') && !t.starts_with("--")) {
                let cluster = t[1..].char_indices().take_while(|(_, ch)| !stops.contains(*ch) || *ch == c);
                if let Some(at) = cluster.map(|(k, _)| k).find(|&k| t[1 + k..].starts_with(c)) {
                    let attached = &t[1 + at + c.len_utf8()..];
                    if attached.is_empty() {
                        out.extend(next());
                    } else {
                        out.push(Word { text: attached.to_string(), dynamic: w.dynamic, quoted: w.quoted });
                    }
                }
            }
            j += 1;
        }
        out
    };
    match sub {
        "rebase" => code.extend(values(&[("--exec", 4)], Some(('x', "sXCS")))),
        "difftool" | "mergetool" => code.extend(values(&[("--extcmd", 5), ("--tool-cmd", 7)], Some(('x', "tO")))),
        "filter-branch" => code.extend(values(
            &[
                ("--setup", 7),
                ("--env-filter", 12),
                ("--tree-filter", 13),
                ("--index-filter", 14),
                ("--parent-filter", 15),
                ("--msg-filter", 12),
                ("--commit-filter", 15),
                ("--tag-name-filter", 17),
            ],
            None,
        )),
        "clone" | "fetch" | "pull" | "ls-remote" | "archive" => {
            code.extend(values(&[("--upload-pack", 5), ("--exec", 4)], (sub == "clone").then_some(('u', "obcj"))))
        }
        "push" | "send-pack" => code.extend(values(&[("--receive-pack", 6), ("--exec", 4)], None)),
        "bisect" => {
            if let Some(at) = rest.iter().position(|w| !w.text.starts_with('-')) {
                if rest[at].text == "run" && at + 1 < rest.len() {
                    argv.push(rest[at + 1..].to_vec());
                }
            }
        }
        "submodule" => {
            if let Some(at) = rest.iter().position(|w| w.text == "foreach") {
                let cmd: Vec<&Word> = rest[at + 1..].iter().skip_while(|w| matches!(w.text.as_str(), "--recursive" | "-q" | "--quiet")).collect();
                if !cmd.is_empty() {
                    // `foreach` runs its arguments joined, as shell code.
                    let text = cmd.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
                    code.push(Word { text, dynamic: cmd.iter().any(|w| w.dynamic), quoted: false });
                }
            }
        }
        _ => {}
    }
    (code, argv)
}

fn push(flags: &[&str], pos: &[&str], ctx: &ShellCtx<'_>, a: &mut Analysis) {
    let short = |c: char| flags.iter().any(|f| f.starts_with('-') && !f.starts_with("--") && f[1..].contains(c));
    // Git takes any unambiguous prefix of a long option (`--force-w`, `--mirr`, `--del`).
    // `--for` is ambiguous between the force options (git refuses it), so denying it costs
    // nothing; `--fo` also matches `--follow-tags`.
    if short('f') || ["--force", "--force-with-lease", "--force-if-includes"].iter().any(|full| long_prefix(flags, full, 5)) {
        a.deny("git push --force (rewrites remote history)");
    }
    if long_prefix(flags, "--mirror", 3) {
        a.deny("git push --mirror");
    }
    // Refspecs after the remote: `+main`, `+HEAD:main` force; `:main` deletes.
    let refspecs: &[&str] = if pos.len() > 1 { &pos[1..] } else { &[] };
    if refspecs.iter().any(|r| r.starts_with('+')) {
        a.deny("git push +refspec (a force push)");
    }
    let protected = |b: &str| {
        let b = b.trim_start_matches("refs/heads/");
        matches!(b, "main" | "master" | "trunk" | "develop" | "production" | "prod")
            || b.starts_with("release")
            || ctx.protected_branches.iter().any(|p| p == b)
    };
    let deletes: Vec<&str> = if long_prefix(flags, "--delete", 4) || short('d') {
        refspecs.to_vec()
    } else {
        refspecs.iter().filter_map(|r| r.strip_prefix(':')).collect()
    };
    if deletes.iter().any(|b| protected(b)) {
        a.deny("git push deleting a protected branch");
    }
    a.ask("git push");
}

/// Whether a flag (before `--`) is `full` as git reads long options: the whole name or a prefix
/// of at least `min` characters, with any `=value` stripped.
fn long_prefix(flags: &[&str], full: &str, min: usize) -> bool {
    flags.iter().take_while(|f| **f != "--").any(|f| {
        let name = f.split_once('=').map_or(*f, |(n, _)| n);
        name.starts_with("--") && name.len() >= min && full.starts_with(name)
    })
}

/// `git -c key=value`: asks when the key runs programs or redirects git.
fn config_injection(kv: &str, dynamic: bool, a: &mut Analysis) {
    let (key, value) = kv.split_once('=').unwrap_or((kv, ""));
    let key = key.to_ascii_lowercase();
    if dynamic {
        a.ask("git -c with a value chosen at run time");
        return;
    }
    let risky = RISKY_CONFIG.contains(&key.as_str())
        || key.starts_with("alias.")
        || key.starts_with("url.")
        || key.starts_with("remote.")
        || key.starts_with("credential.")
        || key.starts_with("filter.")
        || key.starts_with("diff.") && key.ends_with(".textconv")
        || key.starts_with("merge.") && key.ends_with(".driver")
        || key.starts_with("includeif.");
    // A pager that is a bare program name is harmless (`core.pager=cat`); one with arguments runs
    // a shell command.
    let pager = (key == "core.pager" || key.starts_with("pager.")) && value.contains([' ', ';', '|', '&', '$', '`']);
    if risky || pager {
        a.ask(format!("git -c {key} (changes what git runs or where it connects)"));
    }
}

/// Git subcommands (with their arguments) that only read.
pub(crate) fn read_only(sub: &str, flags: &[&str], pos: &[&str]) -> bool {
    // A long option as git accepts it, abbreviated to any unambiguous prefix (`--ou` for
    // `--output`); `min` is the shortest prefix taken to mean it.
    let long = |full: &str, min: usize| {
        flags.iter().any(|f| {
            let name = f.split_once('=').map_or(*f, |(n, _)| n);
            name.len() >= min && name.starts_with("--") && full.starts_with(name)
        })
    };
    if long("--output", 4) || long("--ext-diff", 5) || sub == "grep" && (long("--open-files-in-pager", 4) || flags.iter().any(|f| f.starts_with('-') && !f.starts_with("--") && f[1..].contains('O'))) {
        return false;
    }
    let short_any = |set: &[char]| flags.iter().any(|f| f.starts_with('-') && !f.starts_with("--") && f[1..].chars().any(|c| set.contains(&c)));
    match sub {
        "status" | "log" | "diff" | "show" | "rev-parse" | "rev-list" | "ls-files" | "ls-tree" | "cat-file" | "blame"
        | "grep" | "describe" | "shortlog" | "show-ref" | "for-each-ref" | "merge-base" | "name-rev" | "version" | "help"
        | "count-objects" | "var" | "check-ignore" | "check-attr" | "whatchanged" | "cherry" | "range-diff" | "annotate"
        | "show-branch" | "verify-commit" | "verify-tag" | "diff-tree" | "diff-files" | "diff-index" => {
            !flags.iter().any(|f| f.starts_with("--output") || *f == "--ext-diff")
        }
        "config" => !config_writes(flags),
        "branch" | "tag" => {
            (pos.is_empty() || flags.iter().any(|f| matches!(*f, "--list" | "-l" | "--contains" | "--merged" | "--no-merged" | "--points-at")))
                && !short_any(&['d', 'D', 'm', 'M', 'f', 'c', 'C', 'u', 'a', 's'])
                && !flags.iter().any(|f| matches!(*f, "--delete" | "--move" | "--force" | "--copy" | "--set-upstream-to" | "--unset-upstream" | "--edit-description" | "--annotate" | "--sign"))
                || (sub == "branch" && flags.iter().any(|f| matches!(*f, "-a" | "--all" | "-r" | "--remotes" | "-v" | "-vv" | "--show-current")) && pos.is_empty())
        }
        "remote" => pos.first().is_none_or(|p| matches!(*p, "show" | "get-url")),
        "stash" => pos.first().is_some_and(|p| matches!(*p, "list" | "show")),
        "worktree" => pos.first() == Some(&"list"),
        "reflog" => pos.first().is_none_or(|p| *p == "show"),
        "notes" => pos.first().is_none_or(|p| matches!(*p, "list" | "show")),
        _ => false,
    }
}

/// Whether `git config <flags>` writes (any scope).
pub(crate) fn config_writes(flags: &[&str]) -> bool {
    const WRITE: [&str; 8] = ["--unset", "--unset-all", "--add", "--replace-all", "--rename-section", "--remove-section", "-e", "--edit"];
    const READ: [&str; 8] = ["--get", "--get-all", "--get-regexp", "--get-urlmatch", "--list", "-l", "--get-color", "--get-colorbool"];
    const VALUE: [&str; 6] = ["--file", "-f", "--blob", "--type", "--default", "--comment"];
    if flags.iter().any(|f| WRITE.contains(f)) {
        return true;
    }
    if flags.iter().any(|f| READ.contains(f)) {
        return false;
    }
    let mut pos = Vec::new();
    let mut i = 0;
    while i < flags.len() {
        let f = flags[i];
        if VALUE.contains(&f) {
            i += 2;
            continue;
        }
        if !f.starts_with('-') {
            pos.push(f);
        }
        i += 1;
    }
    match pos.first() {
        Some(&"get") | Some(&"list") => false,
        Some(&"set") | Some(&"unset") | Some(&"rename-section") | Some(&"remove-section") | Some(&"edit") => true,
        _ => pos.len() >= 2,
    }
}

/// The crew's worktree rules: git writes outside the sandbox ask, and inside it so do changes to
/// what the worktree shares with the original checkout. Shared config writes are denied.
#[allow(clippy::too_many_arguments)]
fn sandbox_rules(
    sub: &str,
    flags: &[&str],
    pos: &[&str],
    root: &Path,
    kind: SandboxKind,
    dir: Option<&Path>,
    elsewhere: bool,
    read_only: bool,
    a: &mut Analysis,
) {
    let has = |f: &str| long_prefix(flags, f, 5.min(f.len()));
    let short = |c: char| flags.iter().any(|f| f.starts_with('-') && !f.starts_with("--") && f[1..].contains(c));
    let inside = !elsewhere && kind != SandboxKind::None && dir.is_some_and(|d| super::within(root, d));
    if !inside && !read_only {
        a.ask("git writes outside the sandbox");
        return;
    }
    let why = match sub {
        "branch" if short('f') || has("--force") => Some("git branch -f"),
        "branch" if ['d', 'D', 'm', 'M', 'C'].into_iter().any(short) || has("--delete") || has("--move") => {
            Some("git branch delete or rename (branches are shared with the checkout)")
        }
        "reset" if has("--hard") && kind != SandboxKind::Worktree => Some("git reset --hard outside a worktree"),
        "config" if config_writes(flags) => {
            a.deny("git config write from a crew sandbox (shared with the original checkout)");
            None
        }
        "update-ref" | "gc" | "prune" | "repack" | "pack-refs" | "replace" | "filter-branch" | "filter-repo" => {
            Some("git ref or object maintenance (shared with the checkout)")
        }
        "symbolic-ref" if pos.len() >= 2 || short('d') || has("--delete") => Some("git symbolic-ref"),
        "tag" if short('d') || short('f') || has("--delete") || has("--force") => Some("git tag -d/-f"),
        "reflog" if pos.first().is_some_and(|p| matches!(*p, "expire" | "delete")) => Some("git reflog expire/delete"),
        "worktree" if pos.first().is_some_and(|p| *p != "list") => Some("git worktree (shared with the checkout)"),
        "remote" if pos.first().is_some_and(|p| !matches!(*p, "show" | "get-url")) => Some("git remote change (shared with the checkout)"),
        "stash" if pos.first().is_some_and(|p| matches!(*p, "drop" | "clear")) => Some("git stash drop/clear (shared)"),
        "checkout" | "switch" if short('B') || short('C') => Some("git checkout -B (moves a shared branch)"),
        _ => None,
    };
    if let Some(why) = why {
        a.ask(why);
    }
}
