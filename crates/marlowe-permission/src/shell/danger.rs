// Copyright 2025 OpenAI (codex), Apache-2.0. Derived from codex-rs/shell-command/src/
// command_safety/{is_dangerous_command.rs, windows_dangerous_commands.rs} at
// 6c0c6759d0617d7670826b61aca0a931bbb6db1f. Modified for Marlowe: `shlex` and `url` are replaced
// by small local equivalents, `bash -lc` scripts are read by Marlowe's bash parser, and the result
// feeds `Simple::dangerous` (never widened into a prefix rule) rather than an approval by itself.
// See http://www.apache.org/licenses/LICENSE-2.0 and THIRD_PARTY.md.

//! Commands that destroy work or hand a URL to another program.
//!
//! Two views, both kept as their sources wrote them:
//! - [`dangerous_command_match`] (Codex): forced `rm`, through `sudo`/`env`/`trap` and literal
//!   commands inside `bash -lc`; on Windows also forced PowerShell/cmd deletes and ShellExecute
//!   launches of a URL (`Start-Process https://...`, `cmd /c start https://...`, a browser).
//! - [`destructive`] (Marlowe's list, checked against FlashAgent's fixtures): recursive forced
//!   deletes, disk and power commands, and git commands that discard work a commit cannot bring
//!   back (`reset --hard`, `clean -f`, `checkout .`, `restore .`, `stash drop`, `branch -D`, push).
//!
//! The analyzer marks a command dangerous when either says so: an allow rule must then name it
//! exactly, `accept_edits` does not free it, and an approval of it is not widened.

use super::net::host_of_url;
use super::paths::program;
use super::{Dialect, Word};

/// The platform whose command semantics apply.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DangerousCommandPlatform {
    Posix,
    Windows,
}

impl DangerousCommandPlatform {
    pub fn host() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Posix
        }
    }
}

/// Which dangerous-command rule matched.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DangerousCommandMatch {
    /// An `rm` invocation includes the force option.
    ForcedRm,
    /// Another rule matched.
    Other,
}

const MAX_DANGEROUS_COMMAND_WRAPPER_DEPTH: usize = 8;

/// The dangerous-command rule an already-tokenized command matches.
pub fn dangerous_command_match(command: &[String]) -> Option<DangerousCommandMatch> {
    dangerous_command_match_for_platform(command, DangerousCommandPlatform::host())
}

pub fn dangerous_command_match_for_platform(command: &[String], platform: DangerousCommandPlatform) -> Option<DangerousCommandMatch> {
    match_with_depth(command, 0, platform)
}

fn match_with_depth(command: &[String], depth: usize, platform: DangerousCommandPlatform) -> Option<DangerousCommandMatch> {
    if depth > MAX_DANGEROUS_COMMAND_WRAPPER_DEPTH {
        return Some(DangerousCommandMatch::Other);
    }
    if let Some(m) = match_for_exec(command, depth, platform) {
        return Some(m);
    }
    // Any literal command of a `bash -lc` script, control flow and substitutions included.
    if let Some(m) = shell_lc_literal_commands(command).and_then(|cmds| cmds.iter().find_map(|c| match_with_depth(c, depth + 1, platform))) {
        return Some(m);
    }
    if platform == DangerousCommandPlatform::Windows && is_dangerous_command_windows(command) {
        return Some(DangerousCommandMatch::Other);
    }
    None
}

/// PowerShell words (a command already parsed out of a PowerShell line).
pub fn dangerous_powershell_words_match(command: &[String], platform: DangerousCommandPlatform) -> Option<DangerousCommandMatch> {
    (platform == DangerousCommandPlatform::Windows && is_dangerous_powershell_words(command)).then_some(DangerousCommandMatch::Other)
}

fn lookup_key(raw: &str, platform: DangerousCommandPlatform) -> Option<String> {
    match platform {
        DangerousCommandPlatform::Posix => raw.rsplit('/').next().filter(|n| !n.is_empty()).map(str::to_owned),
        DangerousCommandPlatform::Windows => {
            let name = raw.rsplit(['/', '\\']).next().filter(|n| !n.is_empty())?;
            let name = match name.as_bytes() {
                [d, b':', ..] if d.is_ascii_alphabetic() => &name[2..],
                _ => name,
            };
            let name = name.to_ascii_lowercase();
            for suffix in [".exe", ".cmd", ".bat", ".com"] {
                if let Some(s) = name.strip_suffix(suffix) {
                    return Some(s.to_string());
                }
            }
            (!name.is_empty()).then_some(name)
        }
    }
}

fn match_for_exec(command: &[String], depth: usize, platform: DangerousCommandPlatform) -> Option<DangerousCommandMatch> {
    match command.first().and_then(|c| lookup_key(c, platform)).as_deref() {
        Some("rm") if rm_args_include_force_option(&command[1..]) => Some(DangerousCommandMatch::ForcedRm),
        Some("sudo") => match_with_depth(&command[1..], depth + 1, platform),
        Some("env") => {
            let mut i = 1;
            while let Some(a) = command.get(i) {
                if a == "--" {
                    i += 1;
                    break;
                }
                if matches!(a.as_str(), "-i" | "--ignore-environment")
                    || a.split_once('=').is_some_and(|(n, _)| !n.is_empty() && !n.starts_with('-'))
                {
                    i += 1;
                    continue;
                }
                break;
            }
            match_with_depth(&command[i.min(command.len())..], depth + 1, platform)
        }
        Some("trap") => {
            let mut at = 1;
            if command.get(at).is_some_and(|a| a == "--") {
                at += 1;
            }
            let action = command.get(at).filter(|a| !a.starts_with('-'))?;
            let sh = vec!["sh".to_string(), "-c".to_string(), action.clone()];
            match_with_depth(&sh, depth + 1, platform)
        }
        _ => None,
    }
}

fn rm_args_include_force_option(args: &[String]) -> bool {
    args.iter()
        .take_while(|a| a.as_str() != "--")
        .any(|a| a == "--force" || a.strip_prefix('-').is_some_and(|f| !f.starts_with('-') && f.contains('f')))
}

/// The literal part of every command of a `bash -lc` script (dynamic words dropped; commands
/// whose name is dynamic skipped). Fit for spotting dangerous literals, never for proving safety.
fn shell_lc_literal_commands(command: &[String]) -> Option<Vec<Vec<String>>> {
    let (_, script) = super::words::extract_bash_command(command)?;
    let cmds = super::parse_at(Dialect::Bash, script, 0).ok()?;
    Some(
        cmds.iter()
            .filter(|c| c.words.first().is_some_and(|w| !w.dynamic))
            .map(|c| c.words.iter().filter(|w| !w.dynamic).map(|w| w.text.clone()).collect())
            .collect(),
    )
}

// ---- Windows (windows_dangerous_commands.rs)

/// Dangerous on Windows: forced deletes through PowerShell or cmd, and handing a URL to
/// ShellExecute (Start-Process, `start`, explorer, mshta, rundll32 url.dll, a browser).
pub fn is_dangerous_command_windows(command: &[String]) -> bool {
    is_dangerous_powershell(command) || is_dangerous_cmd(command) || is_direct_gui_launch(command)
}

fn is_dangerous_powershell(command: &[String]) -> bool {
    let Some((exe, rest)) = command.split_first() else { return false };
    if !is_powershell_executable(exe) {
        return false;
    }
    match parse_powershell_invocation(rest) {
        Some(tokens) => is_dangerous_powershell_words(&tokens),
        None => false,
    }
}

/// PowerShell words that delete by force or open a URL in another program.
pub fn is_dangerous_powershell_words(words: &[String]) -> bool {
    let lc: Vec<String> = words.iter().map(|t| t.trim_matches('\'').trim_matches('"').to_ascii_lowercase()).collect();
    let has_url = args_have_url(words);
    if has_url
        && lc.iter().any(|t| {
            matches!(t.as_str(), "start-process" | "start" | "saps" | "invoke-item" | "ii")
                || t.contains("start-process")
                || t.contains("invoke-item")
        })
    {
        return true;
    }
    if has_url && lc.iter().any(|t| t.contains("shellexecute") || t.contains("shell.application")) {
        return true;
    }
    if let Some(first) = lc.first() {
        if first == "rundll32" && lc.iter().any(|t| t.contains("url.dll,fileprotocolhandler")) && has_url {
            return true;
        }
        if (first == "mshta" || is_browser_executable(first) || matches!(first.as_str(), "explorer" | "explorer.exe")) && has_url {
            return true;
        }
    }
    has_force_delete_cmdlet(&lc)
}

fn is_dangerous_cmd(command: &[String]) -> bool {
    let Some((exe, rest)) = command.split_first() else { return false };
    let Some(base) = executable_basename(exe) else { return false };
    if base != "cmd" && base != "cmd.exe" {
        return false;
    }
    let mut iter = rest.iter();
    for arg in iter.by_ref() {
        let lower = arg.to_ascii_lowercase();
        match lower.as_str() {
            "/c" | "/r" | "-c" => break,
            _ if lower.starts_with('/') => continue,
            _ => return false,
        }
    }
    let remaining: Vec<String> = iter.cloned().collect();
    if remaining.is_empty() {
        return false;
    }
    let cmd_tokens: Vec<String> = match remaining.as_slice() {
        [only] => shlex_split(only).unwrap_or_else(|| vec![only.clone()]),
        _ => remaining,
    };
    let tokens: Vec<String> = cmd_tokens.into_iter().flat_map(|t| split_embedded_cmd_operators(&t)).collect();
    const SEPARATORS: &[&str] = &["&", "&&", "|", "||"];
    tokens.split(|t| SEPARATORS.contains(&t.as_str())).any(cmd_segment_dangerous)
}

/// One cmd.exe command: `start <url>`, `del /f`, `rd /s /q`.
pub(crate) fn cmd_segment_dangerous(segment: &[String]) -> bool {
    let Some(cmd) = segment.first() else { return false };
    if cmd.eq_ignore_ascii_case("start") && args_have_url(segment) {
        return true;
    }
    if (cmd.eq_ignore_ascii_case("del") || cmd.eq_ignore_ascii_case("erase")) && segment.iter().any(|a| a.eq_ignore_ascii_case("/f")) {
        return true;
    }
    (cmd.eq_ignore_ascii_case("rd") || cmd.eq_ignore_ascii_case("rmdir"))
        && segment.iter().any(|a| a.eq_ignore_ascii_case("/s"))
        && segment.iter().any(|a| a.eq_ignore_ascii_case("/q"))
}

fn is_direct_gui_launch(command: &[String]) -> bool {
    let Some((exe, rest)) = command.split_first() else { return false };
    let Some(base) = executable_basename(exe) else { return false };
    if matches!(base.as_str(), "explorer" | "explorer.exe" | "mshta" | "mshta.exe") && args_have_url(rest) {
        return true;
    }
    if (base == "rundll32" || base == "rundll32.exe")
        && rest.iter().any(|t| t.to_ascii_lowercase().contains("url.dll,fileprotocolhandler"))
        && args_have_url(rest)
    {
        return true;
    }
    is_browser_executable(&base) && args_have_url(rest)
}

/// `echo hi&del` -> `echo hi`, `&`, `del` (`&`, `&&`, `|`, `||`).
fn split_embedded_cmd_operators(token: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut it = token.char_indices().peekable();
    while let Some((i, ch)) = it.next() {
        if ch == '&' || ch == '|' {
            if i > start {
                parts.push(token[start..i].to_string());
            }
            let len = match it.peek() {
                Some(&(j, next)) if next == ch => {
                    it.next();
                    j + next.len_utf8() - i
                }
                _ => ch.len_utf8(),
            };
            parts.push(token[i..i + len].to_string());
            start = i + len;
        }
    }
    if start < token.len() {
        parts.push(token[start..].to_string());
    }
    parts.retain(|s| !s.trim().is_empty());
    parts
}

fn has_force_delete_cmdlet(tokens: &[String]) -> bool {
    const DELETE: &[&str] = &["remove-item", "ri", "rm", "del", "erase", "rd", "rmdir"];
    const SEG: &[char] = &[';', '|', '&', '\n', '\r', '\t'];
    const SOFT: &[char] = &['{', '}', '(', ')', '[', ']', ',', ';'];
    let mut segments: Vec<Vec<String>> = vec![Vec::new()];
    for tok in tokens {
        let mut cur = String::new();
        for ch in tok.chars() {
            if SEG.contains(&ch) {
                let s = cur.trim();
                if !s.is_empty() {
                    if let Some(seg) = segments.last_mut() {
                        seg.push(s.to_string());
                    }
                }
                cur.clear();
                if segments.last().is_some_and(|l| !l.is_empty()) {
                    segments.push(Vec::new());
                }
            } else {
                cur.push(ch);
            }
        }
        let s = cur.trim();
        if !s.is_empty() {
            if let Some(seg) = segments.last_mut() {
                seg.push(s.to_string());
            }
        }
    }
    segments.into_iter().any(|seg| {
        let atoms = seg.iter().flat_map(|t| t.split(|c| SOFT.contains(&c))).map(str::trim).filter(|s| !s.is_empty());
        let (mut del, mut force) = (false, false);
        for a in atoms {
            del |= DELETE.iter().any(|d| a.eq_ignore_ascii_case(d));
            force |= a.eq_ignore_ascii_case("-force") || a.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("-force:"));
        }
        del && force
    })
}

pub(crate) fn args_have_url(args: &[String]) -> bool {
    args.iter().any(|a| looks_like_url(a))
}

/// An http(s) URL, possibly wrapped in quotes or parentheses (`Start-Process('https://x')`).
pub(crate) fn looks_like_url(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    let urlish = match lower.find("https://").or_else(|| lower.find("http://")) {
        Some(at) => &token[at..],
        None => token.trim_start_matches(|c: char| c == '"' || c == '\'' || c == '(' || c.is_whitespace()),
    };
    let end = urlish.find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ')' | ';')).unwrap_or(urlish.len());
    let candidate = &urlish[..end];
    let l = candidate.to_ascii_lowercase();
    (l.starts_with("http://") || l.starts_with("https://")) && host_of_url(candidate).is_some()
}

fn executable_basename(exe: &str) -> Option<String> {
    let name = exe.rsplit(['/', '\\']).next().filter(|n| !n.is_empty())?;
    let name = match name.as_bytes() {
        [d, b':', ..] if d.is_ascii_alphabetic() => &name[2..],
        _ => name,
    };
    (!name.is_empty()).then(|| name.to_ascii_lowercase())
}

fn is_powershell_executable(exe: &str) -> bool {
    matches!(executable_basename(exe).as_deref(), Some("powershell" | "powershell.exe" | "pwsh" | "pwsh.exe"))
}

pub(crate) fn is_browser_executable(name: &str) -> bool {
    matches!(
        name,
        "chrome" | "chrome.exe" | "msedge" | "msedge.exe" | "firefox" | "firefox.exe" | "iexplore" | "iexplore.exe"
    )
}

fn parse_powershell_invocation(args: &[String]) -> Option<Vec<String>> {
    let mut idx = 0;
    while idx < args.len() {
        let arg = &args[idx];
        let lower = arg.to_ascii_lowercase();
        match lower.as_str() {
            "-command" | "/command" | "-c" => {
                let script = args.get(idx + 1)?;
                if idx + 2 != args.len() {
                    return None;
                }
                return shlex_split(script);
            }
            _ if lower.starts_with("-command:") || lower.starts_with("/command:") => {
                if idx + 1 != args.len() {
                    return None;
                }
                return shlex_split(arg.split_once(':')?.1);
            }
            "-nologo" | "-noprofile" | "-noninteractive" | "-mta" | "-sta" => idx += 1,
            _ if lower.starts_with('-') => idx += 1,
            _ => return Some(args[idx..].to_vec()),
        }
    }
    None
}

/// POSIX `shlex`-style split: whitespace separates, `'...'` is literal, `"..."` honours `\"`,
/// `\\`, `\$`, `` \` `` and a line continuation, a bare backslash escapes the next character.
/// `None` for an unclosed quote or a trailing backslash.
pub fn shlex_split(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            c if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match it.next()? {
                        '\'' => break,
                        ch => cur.push(ch),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match it.next()? {
                        '"' => break,
                        '\\' => match it.next()? {
                            e @ ('"' | '\\' | '$' | '`') => cur.push(e),
                            '\n' => {}
                            e => {
                                cur.push('\\');
                                cur.push(e);
                            }
                        },
                        ch => cur.push(ch),
                    }
                }
            }
            '\\' => {
                in_word = true;
                match it.next()? {
                    '\n' => {}
                    e => cur.push(e),
                }
            }
            ch => {
                in_word = true;
                cur.push(ch);
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    Some(out)
}

// ---- Marlowe's destructive list (FlashAgent fixtures)

/// Why one command (program already normalised, words after quote removal) destroys work that
/// neither a commit nor the workspace checkpoint brings back, or touches the disk or power.
pub fn destructive(program: &str, args: &[String]) -> Option<&'static str> {
    let flag = |long: &str, short: char| {
        args.iter().take_while(|a| a.as_str() != "--").any(|a| {
            a == long || a.strip_prefix('-').is_some_and(|f| !f.starts_with('-') && f.contains(short))
        })
    };
    match program {
        "rm" if flag("--recursive", 'r') && flag("--force", 'f') || flag("--recursive", 'R') && flag("--force", 'f') => {
            Some("recursive forced delete")
        }
        "dd" if args.iter().any(|a| a.starts_with("of=/dev/") || a.starts_with("of=\\\\.\\")) => Some("writes a raw device"),
        p if p.starts_with("mkfs") || p == "format" || p == "diskpart" => Some("formats a disk"),
        "shutdown" | "reboot" | "halt" | "poweroff" | "restart-computer" | "stop-computer" => Some("shuts the machine down"),
        "sudo" | "doas" | "gsudo" => Some("runs a command elevated"),
        "git" => git_destructive(args),
        _ => None,
    }
}

/// `git` subcommands that discard work (FlashAgent's destructive git arm, plus push).
fn git_destructive(args: &[String]) -> Option<&'static str> {
    // Skip global options (`-C dir`, `-c k=v`, `--git-dir=...`).
    let mut i = 0;
    while i < args.len() && args[i].starts_with('-') {
        i += if matches!(args[i].as_str(), "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace") { 2 } else { 1 };
    }
    let sub = args.get(i)?.as_str();
    let rest = &args[(i + 1).min(args.len())..];
    let has = |names: &[&str]| rest.iter().take_while(|a| a.as_str() != "--").any(|a| names.contains(&a.as_str()));
    // A long option matched by an unambiguous prefix, as git accepts (`--har` for `--hard`).
    let long = |full: &str, min: usize| {
        rest.iter().any(|a| {
            let name = a.split_once('=').map_or(a.as_str(), |(n, _)| n);
            name.len() >= min && name.starts_with("--") && full.starts_with(name)
        })
    };
    let short = |c: char| rest.iter().take_while(|a| a.as_str() != "--").any(|a| a.len() > 1 && a.starts_with('-') && !a.starts_with("--") && a[1..].contains(c));
    let dry = has(&["-n", "--dry-run"]);
    match sub {
        "reset" if long("--hard", 5) => Some("git reset --hard discards uncommitted work"),
        "clean" if !dry && (short('f') || long("--force", 4)) => Some("git clean deletes untracked files"),
        "branch" if short('D') || (short('d') || long("--delete", 4)) && (short('f') || long("--force", 4)) => {
            Some("force-deletes a branch")
        }
        "checkout" => {
            let dashdash = rest.iter().position(|a| a == "--");
            let pathspec = match dashdash {
                Some(p) => p + 1 < rest.len() || rest.len() == p + 1,
                None => rest.iter().any(|a| a == "." || a == "./"),
            };
            if pathspec || short('f') || long("--force", 4) {
                Some("git checkout overwrites the working tree")
            } else {
                None
            }
        }
        "restore" => {
            let staged_only = has(&["--staged", "-S"]) && !has(&["--worktree", "-W"]);
            (!staged_only).then_some("git restore overwrites the working tree")
        }
        "switch" if long("--discard-changes", 4) || short('f') || long("--force", 7) => Some("git switch discards changes"),
        "stash" if rest.first().is_some_and(|s| s == "drop" || s == "clear") => Some("drops stashed work"),
        "push" if !dry || short('f') || long("--force", 4) => Some("git push publishes or rewrites history"),
        _ => None,
    }
}

/// [`destructive`] or Codex's [`dangerous_command_match`] / PowerShell-word check, for one
/// parsed command.
pub(crate) fn simple_danger(dialect: Dialect, raw_program: &str, args: &[Word]) -> Option<String> {
    let prog = program(raw_program);
    let texts: Vec<String> = args.iter().map(|w| w.text.clone()).collect();
    if let Some(why) = destructive(&prog, &texts) {
        return Some(why.to_string());
    }
    let mut argv = vec![raw_program.to_string()];
    argv.extend(texts.iter().cloned());
    if let Some(m) = dangerous_command_match_for_platform(&argv, DangerousCommandPlatform::Posix) {
        return Some(match m {
            DangerousCommandMatch::ForcedRm => "forced delete".to_string(),
            DangerousCommandMatch::Other => "dangerous command".to_string(),
        });
    }
    let mut words = vec![prog.clone()];
    words.extend(texts.iter().cloned());
    let forced = match dialect {
        Dialect::PowerShell => has_force_delete_cmdlet(&words.iter().map(|w| w.to_ascii_lowercase()).collect::<Vec<_>>()),
        Dialect::Cmd => cmd_segment_dangerous(&words),
        Dialect::Bash => false,
    };
    forced.then(|| "forced delete".to_string())
}

/// Hands a URL to another program (ShellExecute, a browser): an egress the host check cannot see.
pub(crate) fn url_launch(_dialect: Dialect, raw_program: &str, args: &[Word]) -> bool {
    let prog = program(raw_program);
    let texts: Vec<String> = args.iter().map(|w| w.text.clone()).collect();
    if !args_have_url(&texts) {
        return false;
    }
    let mut words = vec![prog.clone()];
    words.extend(texts.iter().cloned());
    match prog.as_str() {
        "start-process" | "saps" | "invoke-item" | "ii" => true,
        "start" => true,
        "explorer" | "mshta" | "xdg-open" | "open" => true,
        "rundll32" => texts.iter().any(|t| t.to_ascii_lowercase().contains("url.dll,fileprotocolhandler")),
        p if is_browser_executable(p) => true,
        _ => words.iter().any(|t| t.to_ascii_lowercase().contains("shellexecute") || t.to_ascii_lowercase().contains("shell.application")),
    }
}

#[cfg(test)]
mod tests;
