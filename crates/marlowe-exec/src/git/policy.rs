//! What git is never allowed to do from here, whatever the permission mode (design G7, A16).
//!
//! Pure functions over words, so the same rules judge a structured `git` tool call, a `gh` call and
//! a command line handed to `bash` or `powershell`. A hit is a *denial*, not an approval request:
//! no mode, rule or "allow always" lifts it.
//!
//! - force in any spelling: `-f`, `--force`, `--force-with-lease[=..]`, `--force-if-includes`, a
//!   `+refspec`, `--mirror`, `--prune`;
//! - deleting a protected branch on a remote (`--delete`, `-d`, `:branch`);
//! - skipping hooks: `--no-verify` anywhere, `commit -n`, any one-shot `-c core.hooksPath=..`
//!   (also through `GIT_CONFIG_PARAMETERS` / `GIT_CONFIG_KEY_n`), `core.hooksPath` set to nothing;
//! - forcing through config: `remote.*.mirror`, a `remote.*.push` that starts with `+`.
//!
//! Long options count in git's abbreviated spellings too (`--no-verif`, `--force-w`, `--mirr`),
//! and a redirect glued to an option (`--force>log`) does not hide it.
//!
//! Command-line scanning fails closed: it looks for `git` only where a command can start, so
//! `echo git push -f` and `git commit -m "no -f here"` pass, but a line the splitter cannot read
//! (an open quote, a PowerShell backtick escape) is re-scanned without quote handling and denied
//! if any `git` word in it would be.

use std::path::Path;

/// Branch names that a push may never delete.
const PROTECTED: &[&str] = &["main", "master", "develop", "development", "trunk", "production", "prod", "stable"];

/// Whether `branch` is one a remote delete must not touch (`main`, `master`, `release/*`, ...).
pub fn is_protected(branch: &str) -> bool {
    let b = branch.trim().trim_start_matches("refs/heads/").trim_start_matches("heads/");
    let b = b.rsplit_once('/').filter(|(remote, _)| *remote == "origin" || *remote == "upstream").map_or(b, |(_, rest)| rest);
    let lower = b.to_ascii_lowercase();
    PROTECTED.contains(&lower.as_str()) || lower.starts_with("release/") || lower.starts_with("releases/")
}

pub const FORCE_DENIED: &str = "force pushes are never allowed here, in any mode or spelling (-f, --force, \
    --force-with-lease, --force-if-includes, +refspec, --mirror). Push normally; if the remote has \
    moved, fetch and rebase or merge, then push again.";
pub const HOOKS_DENIED: &str = "skipping git hooks is never allowed (--no-verify, commit -n, an empty \
    core.hooksPath). Fix what the hook reports instead.";

fn delete_denied(branch: &str) -> String {
    format!("deleting the protected branch `{branch}` on a remote is never allowed")
}

/// Why a ref name is unusable as an argument, or `None` when it is a plain name. Refs, remotes
/// and paths from the model never start with `-` (option injection) or `+` (a forced refspec),
/// and carry no control characters.
pub fn bad_ref(name: &str) -> Option<String> {
    if name.starts_with('+') {
        return Some(FORCE_DENIED.to_string());
    }
    if name.starts_with('-') {
        return Some(format!("`{name}` starts with `-`; a branch, remote or revision never does"));
    }
    if name.is_empty() || name.chars().any(|c| c.is_control() || c == '\0') {
        return Some("empty or control characters in a ref name".to_string());
    }
    None
}

/// Judges a structured call's arguments (`git`, `gh`): refuses force/no-verify switches however
/// they are spelled and refs that smuggle options. `None` means no objection.
pub fn deny_structured(tool: &str, args: &serde_json::Value) -> Option<String> {
    let obj = args.as_object()?;
    for (key, value) in obj {
        let k = key.to_ascii_lowercase().replace('-', "_");
        let on = match value {
            serde_json::Value::Bool(b) => *b,
            serde_json::Value::String(s) => !s.is_empty() && s != "false" && s != "0",
            serde_json::Value::Number(n) => n.as_i64().unwrap_or(0) != 0,
            _ => false,
        };
        let forcey = matches!(
            k.as_str(),
            "force" | "force_with_lease" | "force_if_includes" | "mirror" | "prune" | "forced" | "overwrite_remote"
        );
        if forcey && on {
            return Some(FORCE_DENIED.to_string());
        }
        if matches!(k.as_str(), "no_verify" | "skip_hooks" | "no_hooks" | "skip_verify" | "noverify") && on {
            return Some(HOOKS_DENIED.to_string());
        }
    }
    let op = obj.get("op").and_then(|v| v.as_str()).unwrap_or("");
    if tool == "git" {
        for key in ["remote", "branch", "name", "from", "rev", "base"] {
            if let Some(s) = obj.get(key).and_then(|v| v.as_str()) {
                // `base` and `rev` may be ranges (`main...HEAD`); each end is checked.
                for part in s.split("...").flat_map(|p| p.split("..")) {
                    if let Some(why) = bad_ref(part).filter(|_| !part.is_empty()) {
                        return Some(why);
                    }
                }
            }
        }
        if op == "push" {
            if let Some(b) = obj.get("branch").and_then(|v| v.as_str()) {
                if b.contains(':') {
                    return Some("push one branch by name; refspecs (`src:dst`, `:dst`) are not accepted".to_string());
                }
            }
        }
        if op == "branch" && obj.get("delete").and_then(|v| v.as_bool()) == Some(true) {
            if let Some(n) = obj.get("name").and_then(|v| v.as_str()).filter(|n| is_protected(n)) {
                return Some(format!("deleting the protected branch `{n}` is not allowed"));
            }
        }
    }
    if tool == "gh" {
        for key in ["remote", "head", "base"] {
            if let Some(s) = obj.get(key).and_then(|v| v.as_str()) {
                if let Some(why) = bad_ref(s) {
                    return Some(why);
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------- command lines

/// One word of a command line: its text with quotes resolved, and whether anything in it could
/// still be expanded by the shell (it was not wholly single-quoted).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Word {
    text: String,
    expands: bool,
}

/// Splits `line` into commands (at `;`, `&`, `&&`, `|`, `||`, newlines and grouping) and each
/// command into words, with POSIX-style quoting; `#` at the start of a word begins a comment
/// (returned too, for the caller to look at). `None` when a quote is left open.
fn commands(line: &str) -> Option<(Vec<Vec<Word>>, Vec<String>)> {
    let mut comments: Vec<String> = Vec::new();
    let mut out: Vec<Vec<Word>> = Vec::new();
    let mut cmd: Vec<Word> = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let mut expands = false;
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    let flush = |cur: &mut String, started: &mut bool, expands: &mut bool, cmd: &mut Vec<Word>| {
        if *started {
            cmd.push(Word { text: std::mem::take(cur), expands: *expands });
        }
        *started = false;
        *expands = false;
    };
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' => {
                started = true;
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    cur.push(chars[i]);
                    i += 1;
                }
                if i >= chars.len() {
                    return None;
                }
            }
            '"' => {
                started = true;
                expands = true;
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' && i + 1 < chars.len() && matches!(chars[i + 1], '"' | '\\' | '$' | '`') {
                        i += 1;
                    } else if chars[i] == '`' || (chars[i] == '$' && chars.get(i + 1) == Some(&'(')) {
                        copy_substitution(&chars, &mut i, &mut cur);
                        continue;
                    }
                    cur.push(chars[i]);
                    i += 1;
                }
                if i >= chars.len() {
                    return None;
                }
            }
            '\\' if i + 1 < chars.len() && matches!(chars[i + 1], ' ' | '"' | '\'' | '\\' | '$' | ';' | '&' | '|' | '(' | ')' | '<' | '>') => {
                started = true;
                i += 1;
                cur.push(chars[i]);
            }
            '`' | '$' if c == '`' || chars.get(i + 1) == Some(&'(') => {
                // A substitution runs its own command: keep it whole in the word, and the caller
                // scans its inside.
                started = true;
                expands = true;
                copy_substitution(&chars, &mut i, &mut cur);
                continue;
            }
            '#' if !started => {
                let from = i + 1;
                while i + 1 < chars.len() && chars[i + 1] != '\n' {
                    i += 1;
                }
                comments.push(chars[from..(i + 1).max(from)].iter().collect());
            }
            ' ' | '\t' | '\r' => flush(&mut cur, &mut started, &mut expands, &mut cmd),
            ';' | '&' | '|' | '\n' | '(' | ')' | '{' | '}' if c != '{' && c != '}' || cur.is_empty() => {
                flush(&mut cur, &mut started, &mut expands, &mut cmd);
                if !cmd.is_empty() {
                    out.push(std::mem::take(&mut cmd));
                }
                // `&&`, `||`
                if matches!(c, '&' | '|') && chars.get(i + 1) == Some(&c) {
                    i += 1;
                }
            }
            _ => {
                started = true;
                cur.push(c);
            }
        }
        i += 1;
    }
    flush(&mut cur, &mut started, &mut expands, &mut cmd);
    if !cmd.is_empty() {
        out.push(cmd);
    }
    Some((out, comments))
}

/// Copies the substitution starting at `chars[*i]` (`$(...)` with nested parentheses balanced, or
/// a backtick pair) into `cur` and leaves `*i` just past it.
fn copy_substitution(chars: &[char], i: &mut usize, cur: &mut String) {
    if chars[*i] == '`' {
        cur.push('`');
        *i += 1;
        while *i < chars.len() {
            let c = chars[*i];
            cur.push(c);
            *i += 1;
            if c == '`' {
                break;
            }
        }
        return;
    }
    let mut depth = 0;
    while *i < chars.len() {
        let c = chars[*i];
        cur.push(c);
        *i += 1;
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
    }
}

/// The text inside `$( ... )` and backticks in `word`, for a recursive scan.
fn substitutions(word: &str) -> Vec<String> {
    let mut found = Vec::new();
    let b: Vec<char> = word.chars().collect();
    let mut i = 0;
    while i < b.len() {
        if b[i] == '$' && b.get(i + 1) == Some(&'(') {
            let mut depth = 1;
            let mut j = i + 2;
            while j < b.len() && depth > 0 {
                match b[j] {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
                j += 1;
            }
            let end = if depth == 0 { j - 1 } else { j };
            found.push(b[i + 2..end.min(b.len())].iter().collect());
            i = j;
        } else if b[i] == '`' {
            let mut j = i + 1;
            while j < b.len() && b[j] != '`' {
                j += 1;
            }
            found.push(b[i + 1..j.min(b.len())].iter().collect());
            i = j + 1;
        } else {
            i += 1;
        }
    }
    found
}

fn base_name(word: &str) -> String {
    let name = word.rsplit(['/', '\\']).next().unwrap_or(word).to_ascii_lowercase();
    for ext in [".exe", ".cmd", ".bat", ".ps1"] {
        if let Some(stripped) = name.strip_suffix(ext) {
            return stripped.to_string();
        }
    }
    name
}

const SHELLS: &[&str] = &["bash", "sh", "zsh", "dash", "ksh", "pwsh", "powershell", "cmd"];
const PREFIXES: &[&str] =
    &["sudo", "command", "builtin", "exec", "nohup", "time", "env", "xargs", "call", "start", "doas", "setsid", "stdbuf", "nice", "timeout", "then", "do", "else", "if", "while", "until", "!", "&"];

fn is_assignment(w: &str) -> bool {
    match w.split_once('=') {
        Some((name, _)) => !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
        None => false,
    }
}

/// Judges every `git` invocation in a shell command line. `Some(reason)` is a denial.
pub fn deny_command(line: &str) -> Option<String> {
    scan_git(line, 0, &mut |env, args| deny_git_env(env, args))
}

/// Whether the command line runs `git push` for real (not a dry run).
pub fn pushes(line: &str) -> bool {
    scan_git(line, 0, &mut |_, args| (git_subcommand(args).is_some_and(|(sub, rest)| sub == "push" && !dry_run(rest))).then_some(())).is_some()
}

fn dry_run(rest: &[&str]) -> bool {
    rest.iter().any(|a| *a == "--dry-run" || (a.starts_with('-') && !a.starts_with("--") && a[1..].chars().take_while(|c| *c != 'o').any(|c| c == 'n')))
}

/// Calls `f` with the assignments that precede `git` and its arguments, for every git invocation
/// in `line`, looking inside shell wrappers and substitutions; the first `Some` wins.
fn scan_git<T>(line: &str, depth: u8, f: &mut dyn FnMut(&[&str], &[&str]) -> Option<T>) -> Option<T> {
    if depth > 4 {
        return scan_loose(line, f);
    }
    let Some((cmds, comments)) = commands(line) else { return scan_loose(line, f) };
    // `cmd /c echo #& git push -f`: a `#` comment to bash is no comment to cmd, so a comment that
    // holds a command separator is read as code too.
    for c in comments.iter().filter(|c| c.contains([';', '&', '|'])) {
        if let Some(hit) = scan_loose(c, f) {
            return Some(hit);
        }
    }
    for cmd in &cmds {
        for w in cmd {
            if w.expands {
                for inner in substitutions(&w.text) {
                    if let Some(hit) = scan_git(&inner, depth + 1, f) {
                        return Some(hit);
                    }
                }
            }
        }
        if let Some(hit) = judge_command(cmd, depth, f) {
            return Some(hit);
        }
    }
    None
}

/// The fallback for a line [`commands`] cannot read: quote characters and backticks dropped, split
/// at separators and whitespace, and every `git` word judged with what follows it. It over-denies
/// (`echo git push -f` hits here) because a denial must not depend on the line parsing.
fn scan_loose<T>(line: &str, f: &mut dyn FnMut(&[&str], &[&str]) -> Option<T>) -> Option<T> {
    let cleaned: String = line.chars().filter(|c| !matches!(c, '\'' | '"' | '`')).collect();
    for seg in cleaned.split([';', '&', '|', '(', ')', '{', '}', '\n']) {
        let words: Vec<&str> = seg.split_whitespace().collect();
        for (i, w) in words.iter().enumerate() {
            if base_name(w) == "git" {
                let env: Vec<&str> = words[..i].iter().copied().filter(|w| is_assignment(w)).collect();
                if let Some(hit) = f(&env, &words[i + 1..]) {
                    return Some(hit);
                }
            }
        }
    }
    None
}

/// Wrapper programs that run another program, and the options of theirs that take a value.
fn wrapper_value_flags(wrapper: &str) -> Option<&'static [&'static str]> {
    Some(match wrapper {
        "sudo" | "doas" => &["-u", "-g", "-h", "-p", "-C", "-D", "-r", "-t", "-T", "-U"],
        "env" => &["-u", "-C", "-S"],
        "nice" => &["-n"],
        "timeout" => &["-s", "-k"],
        "xargs" => &["-n", "-I", "-L", "-P", "-s", "-d", "-E", "-a"],
        "stdbuf" => &["-i", "-o", "-e"],
        w if PREFIXES.contains(&w) => &[],
        _ => return None,
    })
}

fn judge_command<T>(cmd: &[Word], depth: u8, f: &mut dyn FnMut(&[&str], &[&str]) -> Option<T>) -> Option<T> {
    let mut i = 0;
    let mut env: Vec<&str> = Vec::new();
    // Skip what can precede the program: assignments and wrappers with their own arguments.
    while i < cmd.len() {
        let w = cmd[i].text.as_str();
        if is_assignment(w) {
            env.push(w);
            i += 1;
            continue;
        }
        let wrapper = base_name(w);
        let Some(value_flags) = wrapper_value_flags(&wrapper) else { break };
        i += 1;
        while let Some(n) = cmd.get(i).map(|w| w.text.as_str()) {
            let bare = n.trim_start_matches('-');
            let numeric = !bare.is_empty() && bare.chars().all(|c| c.is_ascii_digit() || c == '.') || (bare.len() > 1 && bare[..bare.len() - 1].chars().all(|c| c.is_ascii_digit()) && bare.ends_with(['s', 'm', 'h']));
            if is_assignment(n) {
                env.push(n);
                i += 1;
            } else if (matches!(wrapper.as_str(), "timeout") && numeric && !n.starts_with('-')) {
                i += 1;
            } else if n.starts_with('-') && n.len() > 1 {
                i += 1 + usize::from(value_flags.contains(&n));
            } else {
                break;
            }
        }
    }
    let program = base_name(&cmd.get(i)?.text);
    let rest: Vec<&str> = cmd[i + 1..].iter().map(|w| w.text.as_str()).collect();
    if SHELLS.contains(&program.as_str()) {
        // `bash -c "..."`, `pwsh -Command "..."`, `cmd /c ...`
        let at = rest.iter().position(|a| {
            let l = a.to_ascii_lowercase();
            l == "-c" || l == "-lc" || l == "-command" || l == "-cmd" || l == "/c" || l == "/k" || l == "-ic"
        })?;
        let inner = rest[at + 1..].join(" ");
        return scan_git(&inner, depth + 1, f);
    }
    if program == "eval" {
        return scan_git(&rest.join(" "), depth + 1, f);
    }
    if program == "git" {
        return f(&env, &rest);
    }
    None
}

/// Global options of `git` that take a separate value.
const GLOBAL_WITH_VALUE: &[&str] = &["-C", "-c", "--git-dir", "--work-tree", "--namespace", "--exec-path", "--config-env", "--super-prefix"];

/// The subcommand of `git <args>` and the arguments after it, skipping global options.
fn git_subcommand<'a, 'b>(args: &'b [&'a str]) -> Option<(&'a str, &'b [&'a str])> {
    let mut i = 0;
    while i < args.len() {
        let a = args[i];
        if a == "-c" || a == "--config-env" || GLOBAL_WITH_VALUE.contains(&a) {
            i += 2;
        } else if a.starts_with('-') {
            i += 1;
        } else {
            return Some((a, &args[i + 1..]));
        }
    }
    None
}

/// A long option's name as git reads it: `--force-w>log` and `--no-verif=x` give `force-w` and
/// `no-verif`. `None` for `--` and for short options.
fn long_name(a: &str) -> Option<&str> {
    let long = a.strip_prefix("--")?;
    let end = long.find(['=', '<', '>', '|', '&', ';']).unwrap_or(long.len());
    Some(&long[..end]).filter(|n| !n.is_empty())
}

/// Whether `name` is `full` or an abbreviation of it of at least `min` characters (`min` is
/// rounded down from what git needs: a miss would let it through, a hit costs a denial).
fn abbrev(name: &str, full: &str, min: usize) -> bool {
    name.len() >= min && full.starts_with(name)
}

fn truthy(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "yes" | "on" | "1")
}

/// Judges one config setting made on a command line or through the environment. `value` is `None`
/// when it is not known (`--config-env`, `GIT_CONFIG_KEY_n`), which counts as the worst case.
fn deny_setting(key: &str, value: Option<&str>) -> Option<String> {
    let k = key.trim().to_ascii_lowercase();
    if k == "core.hookspath" {
        return Some(HOOKS_DENIED.to_string());
    }
    if k.starts_with("remote.") {
        if k.ends_with(".mirror") && value.map_or(true, truthy) {
            return Some(FORCE_DENIED.to_string());
        }
        if k.ends_with(".push") && value.map_or(true, |v| v.trim_start().starts_with('+')) {
            return Some(FORCE_DENIED.to_string());
        }
    }
    None
}

/// Judges the assignments before `git`: config injected through the environment.
fn deny_env(env: &[&str]) -> Option<String> {
    for a in env {
        let Some((name, value)) = a.split_once('=') else { continue };
        let name = name.to_ascii_uppercase();
        if name == "GIT_CONFIG_PARAMETERS" {
            // `'core.hooksPath'='x' 'remote.origin.mirror'='true'`: every key counts.
            let v = value.to_ascii_lowercase();
            if v.contains("hookspath") {
                return Some(HOOKS_DENIED.to_string());
            }
            if v.contains("remote.") && (v.contains(".mirror") || v.contains(".push")) {
                return Some(FORCE_DENIED.to_string());
            }
        } else if name.starts_with("GIT_CONFIG_KEY_") {
            if let Some(why) = deny_setting(value, None) {
                return Some(why);
            }
        }
    }
    None
}

/// Judges `git <args>`.
pub fn deny_git(args: &[&str]) -> Option<String> {
    deny_git_env(&[], args)
}

/// Judges `git <args>` run with the environment assignments `env` (`NAME=value`).
pub fn deny_git_env(env: &[&str], args: &[&str]) -> Option<String> {
    if let Some(why) = deny_env(env) {
        return Some(why);
    }
    // Config set for one command, before the subcommand. A legitimate hooks setup is a
    // persistent `git config core.hooksPath .husky`, never a one-shot `-c`.
    let mut i = 0;
    while i < args.len() {
        let a = args[i];
        let setting = if a == "-c" {
            i += 1;
            args.get(i).map(|kv| match kv.split_once('=') {
                Some((k, v)) => (k.to_string(), Some(v.to_string())),
                None => (kv.to_string(), None),
            })
        } else if a == "--config-env" {
            i += 1;
            args.get(i).map(|kv| (kv.split_once('=').map_or(*kv, |(k, _)| k).to_string(), None))
        } else if let Some(kv) = a.strip_prefix("--config-env=") {
            Some((kv.split_once('=').map_or(kv, |(k, _)| k).to_string(), None))
        } else if GLOBAL_WITH_VALUE.contains(&a) {
            i += 1;
            None
        } else if a.starts_with('-') {
            None
        } else {
            break;
        };
        if let Some((k, v)) = setting {
            if let Some(why) = deny_setting(&k, v.as_deref()) {
                return Some(why);
            }
        }
        i += 1;
    }
    let (sub, rest) = git_subcommand(args)?;
    // `--no-verify` on anything that has hooks, in any abbreviation git accepts.
    if rest.iter().any(|a| long_name(a).is_some_and(|n| abbrev(n, "no-verify", 4))) {
        return Some(HOOKS_DENIED.to_string());
    }
    match sub {
        "push" => deny_push(rest),
        "commit" => deny_commit(rest),
        "config" => deny_config(rest),
        "remote" if rest.iter().any(|a| long_name(a).is_some_and(|n| abbrev(n, "mirror", 1))) => Some(FORCE_DENIED.to_string()),
        _ => None,
    }
}

fn deny_config(rest: &[&str]) -> Option<String> {
    let mut positional: Vec<&str> = rest.iter().filter(|a| !a.starts_with('-')).copied().collect();
    if positional.first() == Some(&"set") {
        positional.remove(0);
    }
    let [key, value, ..] = positional.as_slice() else { return None };
    if key.eq_ignore_ascii_case("core.hookspath") {
        // Persistent and pointing at a real directory is how husky works; pointing at nothing is a skip.
        return matches!(*value, "" | "/dev/null" | "NUL" | "nul").then(|| HOOKS_DENIED.to_string());
    }
    deny_setting(key, Some(value))
}

const VALUE_LONG_COMMIT: &[&str] =
    &["message", "file", "reuse-message", "reedit-message", "template", "author", "date", "cleanup", "fixup", "squash"];
const FORCE_LONG: &[&str] = &["force", "force-with-lease", "force-if-includes"];

fn deny_commit(rest: &[&str]) -> Option<String> {
    let mut i = 0;
    while i < rest.len() {
        let a = rest[i];
        if a == "--" {
            break;
        }
        if let Some(cluster) = a.strip_prefix('-').filter(|c| !c.starts_with('-') && !c.is_empty()) {
            for ch in cluster.chars() {
                match ch {
                    'n' => return Some(HOOKS_DENIED.to_string()),
                    // These take a value: the rest of the cluster, or the next word.
                    'm' | 'F' | 'C' | 'c' | 't' => {
                        if cluster.ends_with(ch) {
                            i += 1;
                        }
                        break;
                    }
                    'S' | 'u' => break,
                    _ => {}
                }
            }
        } else if long_name(a).is_some_and(|n| !a.contains('=') && VALUE_LONG_COMMIT.iter().any(|v| abbrev(n, v, 3))) {
            i += 1;
        }
        i += 1;
    }
    None
}

/// A word without a redirect glued to it (`:main>log` is `:main`).
fn bare(a: &str) -> &str {
    a.split(['<', '>', '|', '&', ';']).next().unwrap_or(a)
}

fn deny_push(rest: &[&str]) -> Option<String> {
    let mut positional: Vec<&str> = Vec::new();
    let mut delete = false;
    let mut i = 0;
    while i < rest.len() {
        let a = rest[i];
        if a == "--" {
            positional.extend(rest[i + 1..].iter().map(|a| bare(a)));
            break;
        }
        if let Some(name) = long_name(a) {
            if FORCE_LONG.iter().any(|d| abbrev(name, d, 3)) || abbrev(name, "mirror", 1) || abbrev(name, "prune", 3) {
                return Some(FORCE_DENIED.to_string());
            }
            if abbrev(name, "delete", 2) {
                delete = true;
            } else if !a.contains('=') && ["push-option", "repo", "receive-pack", "exec"].iter().any(|v| abbrev(name, v, 3)) {
                i += 1;
            }
        } else if a.starts_with('-') && a.len() > 1 {
            for ch in a[1..].chars() {
                match ch {
                    'f' => return Some(FORCE_DENIED.to_string()),
                    'd' => delete = true,
                    'o' => {
                        if a.ends_with('o') {
                            i += 1;
                        }
                        break;
                    }
                    _ => {}
                }
            }
        } else {
            positional.push(bare(a));
        }
        i += 1;
    }
    // positional[0] is the remote; the rest are refspecs.
    for spec in positional.iter().skip(1) {
        if spec.starts_with('+') {
            return Some(FORCE_DENIED.to_string());
        }
        if let Some(target) = spec.strip_prefix(':') {
            if target.is_empty() || is_protected(target) {
                return Some(delete_denied(target));
            }
        }
        if delete && is_protected(spec) {
            return Some(delete_denied(spec));
        }
        if let Some((_, dst)) = spec.split_once(':') {
            if dst.starts_with('+') {
                return Some(FORCE_DENIED.to_string());
            }
        }
    }
    if delete && positional.len() < 2 {
        return Some("`git push --delete` without naming the branch is refused".to_string());
    }
    None
}

// ---------------------------------------------------------------- secrets

/// What kind of secret a path looks like (a label for the approval), or `None`. Templates such as
/// `.env.example` are not secrets.
pub fn secret_path(path: &str) -> Option<&'static str> {
    let norm = path.replace('\\', "/");
    let name = Path::new(&norm).file_name()?.to_string_lossy().to_ascii_lowercase();
    if name.ends_with(".example") || name.ends_with(".sample") || name.ends_with(".template") || name.ends_with(".dist") || name.ends_with(".pub") {
        return None;
    }
    if name == ".env" || name.starts_with(".env.") || name.ends_with(".env") {
        return Some("environment file");
    }
    if name.ends_with(".pem") || name.ends_with(".key") || name.ends_with(".p12") || name.ends_with(".pfx") || name.ends_with(".jks") || name.ends_with(".keystore") || name.ends_with(".ppk") {
        return Some("private key or certificate");
    }
    if matches!(name.as_str(), "id_rsa" | "id_dsa" | "id_ecdsa" | "id_ed25519") {
        return Some("SSH private key");
    }
    if matches!(name.as_str(), "credentials" | "credentials.json" | ".netrc" | "_netrc" | ".pypirc" | ".npmrc" | ".git-credentials" | ".htpasswd" | "secrets.json" | "secrets.yaml" | "secrets.yml" | "secrets.toml" | "service-account.json" | "terraform.tfvars")
        || name.ends_with(".tfstate")
        || name.starts_with("service-account")
        || name.starts_with("client_secret")
    {
        return Some("credentials file");
    }
    None
}

/// Secret-shaped lines among the `+` lines of a unified diff: private-key blocks and well-known
/// token shapes. Returns `(path, what)` for each file with a hit.
pub fn secret_content(diff: &str) -> Vec<(String, &'static str)> {
    let mut hits: Vec<(String, &'static str)> = Vec::new();
    let mut file = String::new();
    for line in diff.lines() {
        if let Some(p) = line.strip_prefix("+++ b/") {
            file = p.to_string();
            continue;
        }
        let Some(added) = line.strip_prefix('+').filter(|_| !line.starts_with("+++")) else { continue };
        let what = secret_shape(added);
        if let Some(what) = what {
            if !hits.iter().any(|(f, _)| *f == file) {
                hits.push((file.clone(), what));
            }
        }
    }
    hits
}

fn secret_shape(line: &str) -> Option<&'static str> {
    if line.contains("-----BEGIN") && line.contains("PRIVATE KEY") {
        return Some("a private key block");
    }
    let token = |prefix: &str, min: usize| {
        line.match_indices(prefix).any(|(at, _)| {
            line[at + prefix.len()..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').count() >= min
        })
    };
    if token("AKIA", 16) {
        return Some("an AWS access key id");
    }
    if token("ghp_", 30) || token("gho_", 30) || token("ghs_", 30) || token("github_pat_", 30) {
        return Some("a GitHub token");
    }
    if token("sk-ant-", 20) || token("sk-or-", 20) || token("sk-", 40) {
        return Some("an API key");
    }
    if token("hf_", 30) {
        return Some("a Hugging Face token");
    }
    if token("xoxb-", 20) || token("xoxp-", 20) || token("xapp-", 20) {
        return Some("a Slack token");
    }
    None
}

#[cfg(test)]
mod tests;
