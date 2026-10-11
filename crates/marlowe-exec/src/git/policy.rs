//! What git is never allowed to do from here, whatever the permission mode (design G7, A16).
//!
//! Pure functions over words, so the same rules judge a structured `git` tool call, a `gh` call and
//! a command line handed to `bash` or `powershell`. A hit is a *denial*, not an approval request:
//! no mode, rule or "allow always" lifts it.
//!
//! - force in any spelling: `-f`, `--force`, `--force-with-lease[=..]`, `--force-if-includes`, a
//!   `+refspec`, `--mirror`, `--prune`;
//! - deleting a protected branch on a remote (`--delete`, `-d`, `:branch`);
//! - skipping hooks: `--no-verify` anywhere, `commit -n`, `-c core.hooksPath=<nothing>`.
//!
//! Command-line scanning is deliberately fail-open: a command it cannot read is left to the
//! rules engine (which asks for anything unparseable). It looks for `git` only where a command
//! can start, so `echo git push -f` and `git commit -m "no -f here"` pass.

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
/// command into words, with POSIX-style quoting. `None` when a quote is left open.
fn commands(line: &str) -> Option<Vec<Vec<Word>>> {
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
    Some(out)
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
    scan_git(line, 0, &mut |args| deny_git(args))
}

/// Whether the command line runs `git push` for real (not a dry run).
pub fn pushes(line: &str) -> bool {
    scan_git(line, 0, &mut |args| (git_subcommand(args).is_some_and(|(sub, rest)| sub == "push" && !dry_run(rest))).then_some(())).is_some()
}

fn dry_run(rest: &[&str]) -> bool {
    rest.iter().any(|a| *a == "--dry-run" || (a.starts_with('-') && !a.starts_with("--") && a[1..].chars().take_while(|c| *c != 'o').any(|c| c == 'n')))
}

/// Calls `f` with the arguments (after `git`) of every git invocation in `line`, looking inside
/// shell wrappers and substitutions; the first `Some` wins.
fn scan_git<T>(line: &str, depth: u8, f: &mut dyn FnMut(&[&str]) -> Option<T>) -> Option<T> {
    if depth > 4 {
        return None;
    }
    let cmds = commands(line)?;
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

fn judge_command<T>(cmd: &[Word], depth: u8, f: &mut dyn FnMut(&[&str]) -> Option<T>) -> Option<T> {
    let mut i = 0;
    // Skip what can precede the program: assignments and wrappers with their own arguments.
    while i < cmd.len() {
        let w = cmd[i].text.as_str();
        if is_assignment(w) {
            i += 1;
            continue;
        }
        let wrapper = base_name(w);
        let Some(value_flags) = wrapper_value_flags(&wrapper) else { break };
        i += 1;
        while let Some(n) = cmd.get(i).map(|w| w.text.as_str()) {
            let bare = n.trim_start_matches('-');
            let numeric = !bare.is_empty() && bare.chars().all(|c| c.is_ascii_digit() || c == '.') || (bare.len() > 1 && bare[..bare.len() - 1].chars().all(|c| c.is_ascii_digit()) && bare.ends_with(['s', 'm', 'h']));
            if is_assignment(n) || (matches!(wrapper.as_str(), "timeout") && numeric && !n.starts_with('-')) {
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
        return f(&rest);
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

/// Judges `git <args>`.
pub fn deny_git(args: &[&str]) -> Option<String> {
    // `-c core.hooksPath=<nothing>` before the subcommand.
    let mut i = 0;
    while i < args.len() {
        let a = args[i];
        if a == "-c" || a == "--config-env" {
            if let Some(kv) = args.get(i + 1) {
                let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                if k.eq_ignore_ascii_case("core.hookspath") && matches!(v, "" | "/dev/null" | "NUL" | "nul" | "NUL:") {
                    return Some(HOOKS_DENIED.to_string());
                }
            }
            i += 2;
        } else if GLOBAL_WITH_VALUE.contains(&a) {
            i += 2;
        } else if a.starts_with('-') {
            i += 1;
        } else {
            break;
        }
    }
    let (sub, rest) = git_subcommand(args)?;
    // `--no-verify` on anything that has hooks; exact spelling only.
    if rest.iter().any(|a| *a == "--no-verify" || a.starts_with("--no-verify=")) {
        return Some(HOOKS_DENIED.to_string());
    }
    match sub {
        "push" => deny_push(rest),
        "commit" => deny_commit(rest),
        "config" => deny_config(rest),
        _ => None,
    }
}

fn deny_config(rest: &[&str]) -> Option<String> {
    let positional: Vec<&&str> = rest.iter().filter(|a| !a.starts_with("--")).collect();
    match positional.as_slice() {
        [key, value, ..] if key.eq_ignore_ascii_case("core.hookspath") && matches!(**value, "" | "/dev/null" | "NUL" | "nul") => {
            Some(HOOKS_DENIED.to_string())
        }
        _ => None,
    }
}

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
        } else if matches!(a, "--message" | "--file" | "--reuse-message" | "--reedit-message" | "--template" | "--author" | "--date" | "--cleanup" | "--fixup" | "--squash") {
            i += 1;
        }
        i += 1;
    }
    None
}

fn deny_push(rest: &[&str]) -> Option<String> {
    let mut positional: Vec<&str> = Vec::new();
    let mut delete = false;
    let mut i = 0;
    while i < rest.len() {
        let a = rest[i];
        if a == "--" {
            positional.extend(&rest[i + 1..]);
            break;
        }
        if let Some(long) = a.strip_prefix("--") {
            let name = long.split('=').next().unwrap_or(long);
            match name {
                "force" | "force-with-lease" | "force-if-includes" | "mirror" | "prune" => return Some(FORCE_DENIED.to_string()),
                "delete" => delete = true,
                "push-option" | "repo" | "receive-pack" | "exec" if !long.contains('=') => i += 1,
                _ => {}
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
            positional.push(a);
        }
        i += 1;
    }
    // positional[0] is the remote; the rest are refspecs.
    let specs = positional.iter().skip(1);
    for spec in specs {
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
