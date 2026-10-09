//! The crew's shell policy: which commands run free in the sandbox and which ask.
//!
//! Lexical, and meant to over-ask. It sees what a command line (or a script or python snippet it
//! is handed) shows: git writes outside the worktree or to what a worktree shares with the
//! checkout (config, refs, branches, stash), `git push`, tampering with git's environment, and
//! deletes, copies, moves and redirects outside the sandbox. It cannot see what a program does
//! once started, so it is a tripwire on top of the sandbox's cwd, not OS confinement.

use std::path::{Path, PathBuf};

use marlowe_proto::SandboxKind;

use crate::run::clip;
use crate::sandbox::{normalize, within, Sandbox, ShellVerdict};

/// Largest script read to look for pushes in.
const SCRIPT_SCAN_BYTES: u64 = 512 * 1024;

const DELETES: [&str; 9] = ["rm", "rmdir", "rd", "del", "erase", "remove-item", "ri", "unlink", "shred"];

/// Programs whose path arguments are written (moved, copied to, created or overwritten).
const WRITERS: [&str; 22] = [
    "cp", "copy", "xcopy", "robocopy", "mv", "move", "ln", "copy-item", "cpi", "move-item", "mi", "rename-item", "rni",
    "set-content", "sc", "add-content", "ac", "out-file", "new-item", "ni", "tee", "tee-object",
];

/// Interpreters whose script argument is read and scanned like inline code.
const INTERPRETERS: [&str; 9] = ["bash", "sh", "zsh", "python", "python3", "py", "pwsh", "powershell", "node"];

impl Sandbox {
    /// Whether a shell command may run free. Once the run is `tainted`, every command asks.
    pub fn classify_shell(&self, command: &str, tainted: bool) -> ShellVerdict {
        let shown = clip(command.lines().next().unwrap_or(""), 100);
        if tainted {
            return ShellVerdict::Ask(format!("shell after untrusted content: {shown}"));
        }
        if let Some(why) = git_env_change(command) {
            return ShellVerdict::Ask(format!("{why}: {shown}"));
        }
        let mut cwd = Some(self.root.clone());
        for segment in segments(command) {
            if self.escapes(cwd.as_deref(), &redirect_targets(segment)) {
                return ShellVerdict::Ask(format!("redirect outside the sandbox: {shown}"));
            }
            let words: Vec<String> =
                segment.replace(['"', '\'', '`'], " ").split_whitespace().map(str::to_string).collect();
            for (i, w) in words.iter().enumerate() {
                let rest = &words[i + 1..];
                let own: Vec<&str> = rest.iter().take_while(|a| !is_program_word(a)).map(String::as_str).collect();
                let prog = program(w);
                match prog.as_str() {
                    "cd" | "chdir" | "pushd" | "set-location" | "sl" if i == 0 => {
                        if let Some(dir) = own.iter().find(|a| !a.starts_with('-') && !a.eq_ignore_ascii_case("/d")) {
                            cwd = cwd.as_deref().and_then(|c| resolve(c, dir));
                        }
                    }
                    "git" => {
                        if let Some(why) = self.git_verdict(cwd.as_deref(), rest) {
                            return ShellVerdict::Ask(format!("{why}: {shown}"));
                        }
                    }
                    p if DELETES.contains(&p) && self.escapes(cwd.as_deref(), &positional(&own)) => {
                        return ShellVerdict::Ask(format!("delete outside the sandbox: {shown}"));
                    }
                    p if WRITERS.contains(&p) && self.escapes(cwd.as_deref(), &write_targets(p, &own)) => {
                        return ShellVerdict::Ask(format!("writes outside the sandbox: {shown}"));
                    }
                    p if INTERPRETERS.contains(&p) => {
                        if let Some(text) = positional(&own).first().and_then(|s| read_script(cwd.as_deref(), s)) {
                            if let ShellVerdict::Ask(why) = self.classify_code(&text) {
                                return ShellVerdict::Ask(format!("{why}: {shown}"));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        ShellVerdict::Free
    }

    /// Whether code (python, or a script a shell runs) may run free in an untainted run. Asks when
    /// it mentions pushing with git or git's environment, the one side effect a sandboxed cwd does
    /// not contain. Lexical.
    pub fn classify_code(&self, code: &str) -> ShellVerdict {
        if let Some(why) = git_env_change(code) {
            return ShellVerdict::Ask(why.to_string());
        }
        let words: Vec<String> = code
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .filter(|w| !w.is_empty())
            .map(str::to_ascii_lowercase)
            .collect();
        if words.iter().any(|w| w == "git") && words.iter().any(|w| w == "push") {
            return ShellVerdict::Ask("code that may run git push".into());
        }
        ShellVerdict::Free
    }

    /// Whether any of `targets` resolves outside the writable sandbox (or cannot be resolved).
    fn escapes(&self, cwd: Option<&Path>, targets: &[&str]) -> bool {
        targets.iter().any(|a| cwd.and_then(|c| resolve(c, a)).is_none_or(|p| !self.allows_write(&p)))
    }

    /// Why a `git` invocation (its arguments after `git`, run in `cwd`) needs approval, if it does.
    fn git_verdict(&self, cwd: Option<&Path>, args: &[String]) -> Option<&'static str> {
        let mut dir = cwd.map(Path::to_path_buf);
        let mut elsewhere = false;
        let mut i = 0;
        while i < args.len() {
            let a = args[i].as_str();
            if a == "-C" {
                dir = match (dir.as_deref(), args.get(i + 1)) {
                    (Some(c), Some(d)) => resolve(c, d),
                    _ => None,
                };
                i += 2;
            } else if a == "-c" {
                i += 2;
            } else if a == "--git-dir" || a == "--work-tree" {
                elsewhere = true;
                i += 2;
            } else if a.starts_with('-') {
                elsewhere |= a.starts_with("--git-dir=") || a.starts_with("--work-tree=");
                i += 1;
            } else {
                break;
            }
        }
        let sub = args.get(i)?.to_ascii_lowercase();
        let flags = &args[i + 1..];
        let pos: Vec<&str> = flags.iter().filter(|f| !f.starts_with('-')).map(String::as_str).collect();
        let has = |f: &str| flags.iter().any(|x| x == f);
        let short = |c: char| flags.iter().any(|f| f.starts_with('-') && !f.starts_with("--") && f.contains(c));
        if sub == "push" {
            return Some("git push");
        }
        let inside = !elsewhere
            && self.kind != SandboxKind::None
            && dir.as_deref().is_some_and(|d| within(&self.root, d));
        if !inside && !read_only(&sub, flags, &pos) {
            return Some("git writes outside the sandbox");
        }
        // Inside the worktree; these change what it shares with the original checkout.
        match sub.as_str() {
            "branch" if short('f') || has("--force") => Some("git branch -f"),
            "branch" if ['d', 'D', 'm', 'M', 'C'].into_iter().any(short) || has("--delete") || has("--move") => {
                Some("git branch delete or rename (branches are shared with the checkout)")
            }
            "reset" if has("--hard") && self.kind != SandboxKind::Worktree => Some("git reset --hard outside a worktree"),
            "config" if config_writes(flags) => Some("git config (shared with the original checkout)"),
            "update-ref" | "gc" | "prune" | "repack" | "pack-refs" | "replace" | "filter-branch" | "filter-repo" => {
                Some("git ref or object maintenance (shared with the checkout)")
            }
            "symbolic-ref" if pos.len() >= 2 || short('d') || has("--delete") => Some("git symbolic-ref"),
            "tag" if short('d') || short('f') || has("--delete") || has("--force") => Some("git tag -d/-f"),
            "reflog" if pos.first().is_some_and(|p| matches!(*p, "expire" | "delete")) => Some("git reflog expire/delete"),
            "worktree" if pos.first().is_some_and(|p| *p != "list") => Some("git worktree (shared with the checkout)"),
            "remote" if pos.first().is_some_and(|p| !matches!(*p, "show" | "get-url")) => {
                Some("git remote change (shared with the checkout)")
            }
            "stash" if pos.first().is_some_and(|p| matches!(*p, "drop" | "clear")) => Some("git stash drop/clear (shared)"),
            "checkout" | "switch" if short('B') || short('C') => Some("git checkout -B (moves a shared branch)"),
            _ => None,
        }
    }
}

/// Git subcommands (with their arguments) that only read.
fn read_only(sub: &str, flags: &[String], pos: &[&str]) -> bool {
    match sub {
        "status" | "log" | "diff" | "show" | "rev-parse" | "rev-list" | "ls-files" | "ls-tree" | "ls-remote" | "cat-file"
        | "blame" | "grep" | "describe" | "shortlog" | "show-ref" | "for-each-ref" | "merge-base" | "name-rev" | "version"
        | "help" | "count-objects" | "var" | "check-ignore" | "check-attr" | "whatchanged" | "cherry" | "range-diff" => true,
        "config" => !config_writes(flags),
        "branch" | "tag" => pos.is_empty() && !flags.iter().any(|f| f.starts_with('-') && !f.starts_with("--") && f.contains(['d', 'D', 'm', 'M', 'f', 'c', 'C'])),
        "remote" => pos.first().is_none_or(|p| matches!(*p, "show" | "get-url")),
        "stash" => pos.first().is_some_and(|p| matches!(*p, "list" | "show")),
        "worktree" => pos.first() == Some(&"list"),
        "reflog" => pos.first().is_none_or(|p| *p == "show"),
        _ => false,
    }
}

/// Whether `git config <flags>` writes (any scope: local, worktree, global or a file).
fn config_writes(flags: &[String]) -> bool {
    const WRITE: [&str; 8] = ["--unset", "--unset-all", "--add", "--replace-all", "--rename-section", "--remove-section", "-e", "--edit"];
    const READ: [&str; 7] = ["--get", "--get-all", "--get-regexp", "--get-urlmatch", "--list", "-l", "--get-color"];
    const VALUE: [&str; 6] = ["--file", "-f", "--blob", "--type", "--default", "--comment"];
    if flags.iter().any(|f| WRITE.contains(&f.as_str())) {
        return true;
    }
    if flags.iter().any(|f| READ.contains(&f.as_str()) || f == "--get-colorbool") {
        return false;
    }
    let mut pos = Vec::new();
    let mut i = 0;
    while i < flags.len() {
        let f = flags[i].as_str();
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

/// Text that sets or clears git's own environment (its config overrides, repository location,
/// ssh or askpass programs), which would undo the crew's no-push environment.
fn git_env_change(text: &str) -> Option<&'static str> {
    let l = text.to_ascii_lowercase();
    ["git_config", "git_dir", "git_work_tree", "git_ssh", "git_askpass", "git_exec_path", "git_proxy_command"]
        .iter()
        .any(|v| l.contains(v))
        .then_some("changes git's environment")
}

/// Positional arguments: not flags (`-x`, `--x`) and not two-letter cmd switches (`/s`).
fn positional<'a>(args: &[&'a str]) -> Vec<&'a str> {
    args.iter().copied().filter(|a| !a.starts_with('-') && !(a.starts_with('/') && a.len() == 2)).collect()
}

/// The paths a writer writes: the destination of a copy, both sides of a move or rename, every
/// path of a create or overwrite.
fn write_targets<'a>(prog: &str, args: &[&'a str]) -> Vec<&'a str> {
    let pos = positional(args);
    let after = |flag: &str| args.iter().position(|a| a.eq_ignore_ascii_case(flag)).and_then(|i| args.get(i + 1)).copied();
    match prog {
        "cp" | "copy" | "xcopy" | "ln" => pos.last().copied().into_iter().collect(),
        "robocopy" => pos.get(1).copied().into_iter().collect(),
        "copy-item" | "cpi" => after("-destination").or(pos.get(1).copied()).into_iter().collect(),
        "move-item" | "mi" => pos.into_iter().chain(after("-destination")).collect(),
        _ => pos,
    }
}

/// Targets of `>` and `>>` redirects in one segment, except the null device.
fn redirect_targets(segment: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = segment;
    while let Some(at) = rest.find('>') {
        let tail = rest[at + 1..].trim_start_matches('>').trim_start();
        let end = tail.find(|c: char| c.is_whitespace() || "<>()|;&".contains(c)).unwrap_or(tail.len());
        let target = tail[..end].trim_matches(|c| c == '"' || c == '\'');
        let null = ["nul", "nul:", "/dev/null", "$null", "/dev/stdout", "/dev/stderr", "con"];
        if !target.is_empty() && !null.contains(&target.to_ascii_lowercase().as_str()) {
            out.push(target);
        }
        rest = &tail[end..];
    }
    out
}

/// The text of a script an interpreter is given, when it is a readable file of modest size.
fn read_script(cwd: Option<&Path>, arg: &str) -> Option<String> {
    let path = resolve(cwd?, arg)?;
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > SCRIPT_SCAN_BYTES {
        return None;
    }
    std::fs::read(&path).ok().map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// A shell argument as a path; `None` for ones that expand (`~`, `$X`, `%X%`).
fn resolve(cwd: &Path, arg: &str) -> Option<PathBuf> {
    if arg.starts_with('~') || arg.contains('$') || arg.contains('%') {
        return None;
    }
    Some(normalize(cwd, Path::new(arg)))
}

/// A command line cut at `&&`, `||`, `;`, `|`, `&` and newlines.
fn segments(command: &str) -> Vec<&str> {
    command.split(['\n', ';', '|', '&']).map(str::trim).filter(|s| !s.is_empty()).collect()
}

/// A word as a program name: lower-case file stem (`C:\x\git.exe` → `git`).
fn program(word: &str) -> String {
    let base = word.rsplit(['/', '\\']).next().unwrap_or(word).to_ascii_lowercase();
    base.strip_suffix(".exe").map(str::to_string).unwrap_or(base)
}

/// Words that start a new command inside one segment (`xargs rm`, `cmd /c git`).
fn is_program_word(word: &str) -> bool {
    let p = program(word);
    p == "git" || DELETES.contains(&p.as_str()) || WRITERS.contains(&p.as_str())
}
