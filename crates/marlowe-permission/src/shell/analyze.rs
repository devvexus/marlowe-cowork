//! Walks every command a parsed line runs and records what it would do (see the module root).

use std::path::{Path, PathBuf};

use super::danger;
use super::git;
use super::net::{fetch_value_flags, host_of_bare, host_of_url, is_local, registry_hosts};
use super::paths::{is_null_device, program, resolve, same, script_kind};
use super::{parse_at, Analysis, Cmd, Dialect, Scope, ShellCtx, Simple, Verdict, Word, MAX_DEPTH};

/// Analyzes `command` written in `dialect`, run from `ctx.cwd`.
pub fn analyze(dialect: Dialect, command: &str, ctx: &ShellCtx<'_>) -> Analysis {
    let mut a = Analysis::default();
    text_scan(command, &mut a);
    match parse_at(dialect, command, 0) {
        Err(e) => a.ask(format!("could not parse the command ({e})")),
        Ok(cmds) => {
            let mut cwd = Some(ctx.cwd.clone());
            walk(&cmds, &mut cwd, ctx, 0, &mut a);
        }
    }
    a.literal = match dialect {
        Dialect::Bash => super::words::bash_words(command),
        Dialect::PowerShell => super::words::powershell_words(command),
        Dialect::Cmd => None,
    };
    a
}

/// The crew's verdict on code (python, or a script a shell runs): asks when it mentions pushing
/// with git or git's environment. Lexical.
pub fn classify_code(code: &str) -> Verdict {
    let mut a = Analysis::default();
    code_scan(code, &mut a, false);
    match a.asks.first() {
        Some(why) => Verdict::Ask(why.clone()),
        None => Verdict::Free,
    }
}

/// Code handed to an interpreter: git push or git environment mentions ask; URL literals are
/// hosts (when `hosts`).
pub(crate) fn code_scan(code: &str, a: &mut Analysis, hosts: bool) {
    git_env_scan(code, a);
    let words: Vec<String> = code
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    if words.iter().any(|w| w == "git") && words.iter().any(|w| w == "push") {
        a.ask("code that may run git push");
    }
    if hosts {
        for (i, _) in code.match_indices("://") {
            let start = code[..i].rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')).map_or(0, |p| p + 1);
            let end = code[i + 3..].find(|c: char| c.is_whitespace() || "'\"`)<>]}".contains(c)).map_or(code.len(), |p| i + 3 + p);
            if let Some(h) = host_of_url(&code[start..end]).filter(|h| !is_local(h)) {
                a.host(h);
            }
        }
    }
}

/// Text that sets or clears git's own environment (config overrides, repository location, ssh or
/// askpass programs), which would undo the crew's no-push environment.
fn git_env_scan(text: &str, a: &mut Analysis) {
    let l = text.to_ascii_lowercase();
    if ["git_config", "git_dir", "git_work_tree", "git_ssh", "git_askpass", "git_exec_path", "git_proxy_command", "git_index_file", "git_object_directory", "git_alternate_object", "git_namespace", "git_template_dir"]
        .iter()
        .any(|v| l.contains(v))
    {
        a.ask("changes git's environment");
    }
}

/// Whole-text checks that do not depend on parsing.
fn text_scan(text: &str, a: &mut Analysis) {
    git_env_scan(text, a);
    let l = text.to_ascii_lowercase();
    if ["net.webclient", "downloadstring", "downloadfile", "downloaddata", "system.net.http", "net.http.httpclient", "net.sockets", "openread("]
        .iter()
        .any(|v| l.contains(v))
    {
        a.ask("network access through .NET (the host is not visible)");
    }
    if ["[system.diagnostics.process]", "[diagnostics.process]", "system.reflection", "[reflection.", "interopservices", "[activator]", "[scriptblock]::create", "[powershell]::create", "add-type"]
        .iter()
        .any(|v| l.contains(v))
    {
        a.ask("runs code through .NET");
    }
}

fn walk(cmds: &[Cmd], cwd: &mut Option<PathBuf>, ctx: &ShellCtx<'_>, depth: u8, a: &mut Analysis) {
    for c in cmds {
        command(c, &c.words, cwd, ctx, depth, a);
    }
}

/// Parses `src` in `dialect` and walks it from `cwd` (a nested shell gets its own cwd).
fn nested(dialect: Dialect, src: &str, cwd: &Option<PathBuf>, ctx: &ShellCtx<'_>, depth: u8, a: &mut Analysis) {
    if depth + 1 > MAX_DEPTH {
        a.ask("nested too deeply to inspect");
        return;
    }
    text_scan(src, a);
    match parse_at(dialect, src, depth + 1) {
        Err(e) => a.ask(format!("could not parse the nested command ({e})")),
        Ok(cmds) => {
            let mut inner = cwd.clone();
            walk(&cmds, &mut inner, ctx, depth + 1, a);
        }
    }
}

/// A script file a shell or interpreter is handed: parsed (shells) or scanned (code).
fn script(path_arg: &Word, dialect: Option<Dialect>, cwd: &Option<PathBuf>, ctx: &ShellCtx<'_>, depth: u8, a: &mut Analysis) {
    if path_arg.dynamic {
        a.ask("runs a script chosen at run time");
        return;
    }
    let Some(path) = cwd.as_deref().and_then(|c| resolve(Dialect::Bash, c, &path_arg.text).or_else(|| resolve(Dialect::PowerShell, c, &path_arg.text))) else {
        return;
    };
    let Some(text) = (ctx.read_script)(&path) else { return };
    match dialect {
        Some(d) => nested(d, &text, cwd, ctx, depth, a),
        None => code_scan(&text, a, true),
    }
}

const DELETES: &[&str] = &["rm", "rmdir", "rd", "del", "erase", "remove-item", "ri", "unlink", "shred", "srm", "trash"];

/// Programs whose path arguments are written (created, moved, copied to, overwritten, re-moded).
const WRITERS: &[&str] = &[
    "cp", "copy", "xcopy", "robocopy", "mv", "move", "ln", "copy-item", "cpi", "move-item", "mi", "rename-item", "rni",
    "ren", "rename", "set-content", "add-content", "ac", "out-file", "new-item", "ni", "tee", "tee-object", "mkdir",
    "md", "touch", "truncate", "chmod", "chown", "chgrp", "icacls", "attrib", "takeown", "install", "clear-content",
    "clc", "set-itemproperty", "new-itemproperty", "remove-itemproperty", "set-item", "export-csv",
];

/// File operations that `accept_edits` frees inside the workspace.
const FS_OPS: &[&str] = &[
    "mkdir", "md", "touch", "cp", "copy", "mv", "move", "rm", "rmdir", "rd", "del", "erase", "ren", "rename", "ln",
    "new-item", "ni", "copy-item", "cpi", "move-item", "mi", "rename-item", "rni", "remove-item", "ri", "set-content",
    "add-content", "out-file", "tee", "unlink",
];

const READ_ONLY: &[&str] = &[
    "ls", "dir", "ll", "la", "pwd", "cat", "type", "head", "tail", "less", "more", "wc", "grep", "egrep", "fgrep", "rg",
    "ag", "ack", "fd", "tree", "which", "where", "whereis", "whoami", "hostname", "uname", "id", "echo", "printf",
    "date", "printenv", "file", "stat", "du", "df", "diff", "cmp", "comm", "cut", "tr", "jq", "column", "nl", "od",
    "xxd", "hexdump", "strings", "basename", "dirname", "realpath", "readlink", "true", "false", "test", "[", "[[",
    "seq", "sleep", "md5sum", "sha1sum", "sha256sum", "sha512sum", "b2sum", "cksum", "get-childitem", "gci",
    "get-content", "gc", "select-string", "sls", "get-location", "gl", "test-path", "get-item", "gi",
    "get-itemproperty", "gp", "measure-object", "measure", "select-object", "select", "where-object", "?",
    "foreach-object", "%", "sort-object", "group-object", "format-table", "ft", "format-list", "fl", "out-string",
    "out-null", "write-output", "write-host", "get-command", "gcm", "get-process", "get-service", "get-help",
    "resolve-path", "split-path", "join-path", "convertto-json", "convertfrom-json", "get-date", "get-filehash",
    "compare-object", "get-member", "findstr", "ver",
];

/// One simple command (its `words` may be a suffix after a wrapper like `sudo` or `env`).
fn command(c: &Cmd, words: &[Word], cwd: &mut Option<PathBuf>, ctx: &ShellCtx<'_>, depth: u8, a: &mut Analysis) {
    let d = c.dialect;
    for (name, _) in &c.assigns {
        env_assign(name, a);
    }
    for r in &c.redirects {
        if r.writes() {
            write_target(d, &r.target, cwd, ctx, a, "redirect");
        }
    }
    if d == Dialect::PowerShell {
        dotnet_scan(words.iter().chain(c.assigns.iter().map(|(_, v)| v)), a);
    }
    if d == Dialect::Cmd && (words.iter().any(|w| w.dynamic) || c.redirects.iter().any(|r| r.target.dynamic)) {
        // cmd.exe expands %VARIABLES% before it splits the line, so a value can add commands.
        a.ask("cmd.exe expands a variable before it reads the line");
    }
    let Some(first) = words.first() else { return };
    let top = std::ptr::eq(words.as_ptr(), c.words.as_ptr());
    if top && !c.invoked && (first.quoted || first.dynamic || first.text.starts_with(|ch: char| ch.is_ascii_digit() || ch == '@')) {
        // A PowerShell expression statement (`'text'`, `$x.Length`, `(1+2)`): runs nothing
        // itself; its subexpressions were walked as commands of their own.
        return;
    }
    if first.dynamic && first.text == "{...}" {
        // `& { ... }`: the script block's commands were walked when it was parsed.
        return;
    }
    if first.dynamic {
        a.ask("runs a command chosen at run time");
        a.commands.push(Simple { program: first.text.clone(), line: first.text.clone(), dynamic: true, opaque: true, ..Simple::default() });
        return;
    }
    let raw = first.text.as_str();
    let prog = program(raw);
    let args = &words[1..];
    let mut simple = Simple {
        program: prog.clone(),
        args: args.iter().map(|w| w.text.clone()).collect(),
        dynamic: args.iter().any(|w| w.dynamic),
        ..Simple::default()
    };
    simple.line = std::iter::once(prog.as_str()).chain(simple.args.iter().map(String::as_str)).collect::<Vec<_>>().join(" ");
    simple.dangerous = danger::simple_danger(d, raw, args);
    if danger::url_launch(d, raw, args) {
        a.ask("opens a URL in another program (its host is not checked)");
    }

    // A .NET static call (`[IO.File]::Delete(...)`) is not a program (see `dotnet_scan`).
    if raw.starts_with('[') && raw.contains("]::") {
        a.commands.push(simple);
        return;
    }
    // A script file run directly (`./build.sh`, `.\tools\x.ps1`).
    if let Some(kind) = script_kind(raw) {
        if raw.contains(['/', '\\']) || raw.starts_with('.') {
            script(first, Some(kind), cwd, ctx, depth, a);
            simple.opaque = true;
            a.commands.push(simple);
            return;
        }
    }

    let pos = positionals(d, &prog, args);
    match prog.as_str() {
        // ---- working directory
        "cd" | "chdir" | "pushd" | "set-location" | "sl" | "popd" => {
            let target = pos.first();
            *cwd = match (prog.as_str(), target, cwd.as_deref()) {
                ("popd", _, _) => None,
                (_, Some(t), Some(c)) if !t.dynamic && t.text != "-" => {
                    let t = if d == Dialect::Cmd { t.text.trim_start_matches("/d ").to_string() } else { t.text.clone() };
                    resolve(d, c, &t)
                }
                (_, None, Some(c)) if d == Dialect::Cmd || d == Dialect::PowerShell && prog != "cd" => Some(c.to_path_buf()),
                _ => None,
            };
            simple.cd_only = true;
            simple.read_only = true;
        }

        // ---- wrappers: the rest of the words is the command
        "sudo" | "doas" | "gsudo" | "runas" | "pkexec" => {
            a.ask("runs a command elevated");
            simple.opaque = true;
            a.commands.push(simple);
            let rest = skip_flags(args, &["-u", "-g", "-C", "-h", "-p", "-U", "/user"]);
            command(c, rest, cwd, ctx, depth, a);
            return;
        }
        "env" => {
            let mut i = 0;
            while i < args.len() {
                let t = args[i].text.as_str();
                if t == "-u" || t == "--unset" || t == "-C" || t == "--chdir" {
                    if let Some(n) = args.get(i + 1) {
                        env_assign(&n.text, a);
                    }
                    i += 2;
                } else if t == "-S" || t == "--split-string" {
                    if let Some(s) = args.get(i + 1) {
                        nested(Dialect::Bash, &s.text, cwd, ctx, depth, a);
                    }
                    return;
                } else if t.starts_with('-') {
                    i += 1;
                } else if let Some((name, _)) = t.split_once('=').filter(|(n, _)| !n.is_empty() && !n.contains(['/', '\\'])) {
                    env_assign(name, a);
                    i += 1;
                } else {
                    break;
                }
            }
            if i >= args.len() {
                simple.read_only = true;
                a.commands.push(simple);
                return;
            }
            command(c, &args[i..], cwd, ctx, depth, a);
            return;
        }
        "nohup" | "time" | "exec" | "command" | "builtin" | "caffeinate" | "winpty" | "chronic" | "unbuffer" | "setsid" | "stdbuf" | "nice" | "ionice" | "timeout" | "xvfb-run" | "flock" => {
            let mut rest = skip_flags(args, &["-n", "-c", "-p", "-s", "-k", "--signal", "--kill-after", "-i", "-o", "-e"]);
            // `timeout 10 cmd`, `flock /tmp/lock cmd`: the first positional is not the command.
            if matches!(prog.as_str(), "timeout" | "flock") && !rest.is_empty() {
                rest = &rest[1..];
            }
            if rest.is_empty() {
                simple.read_only = prog == "time" || prog == "command";
                a.commands.push(simple);
                return;
            }
            command(c, rest, cwd, ctx, depth, a);
            return;
        }
        "xargs" => {
            let mut i = 0;
            let mut placeholder: Option<String> = None;
            while i < args.len() && args[i].text.starts_with('-') {
                let t = args[i].text.as_str();
                if matches!(t, "-I" | "-i") {
                    placeholder = args.get(i + 1).map(|w| w.text.clone());
                    i += 2;
                } else if matches!(t, "-n" | "-P" | "-L" | "-d" | "-a" | "-E" | "-s" | "--max-args" | "--max-procs" | "--delimiter" | "--arg-file") {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            let mut inner: Vec<Word> = args[i..].to_vec();
            if inner.is_empty() {
                inner.push(Word::lit("echo"));
            }
            for w in inner.iter_mut().skip(1) {
                if placeholder.as_deref().is_some_and(|p| w.text.contains(p)) {
                    w.dynamic = true;
                }
            }
            inner.push(Word { text: "{xargs}".into(), dynamic: true, quoted: false });
            command(c, &inner, cwd, ctx, depth, a);
            return;
        }

        // ---- shells and interpreters handed code
        "bash" | "sh" | "zsh" | "dash" | "ksh" | "git-bash" | "fish" => {
            simple.opaque = true;
            let code = args.iter().position(|w| !w.quoted && w.text.starts_with('-') && !w.text.starts_with("--") && w.text.contains('c'));
            if let Some(i) = code {
                match args.get(i + 1) {
                    Some(s) if s.dynamic => a.ask("shell code built at run time"),
                    Some(s) => nested(Dialect::Bash, &s.text, cwd, ctx, depth, a),
                    None => a.ask("shell -c with no code"),
                }
            } else if let Some(file) = args.iter().find(|w| !w.text.starts_with('-')) {
                script(file, Some(Dialect::Bash), cwd, ctx, depth, a);
            } else if let Some(stdin) = &c.stdin {
                nested(Dialect::Bash, stdin, cwd, ctx, depth, a);
            } else if !args.iter().any(|w| w.text == "--version") {
                a.ask("runs shell code piped in");
            }
        }
        "export" | "declare" | "typeset" | "local" | "readonly" | "set" | "unset" | "shopt" | "set-variable" | "sv" | "remove-variable" => {
            for w in args {
                let t = w.text.as_str();
                let name = t.split_once('=').map_or(t, |(n, _)| n);
                if !name.starts_with('-') && !name.is_empty() {
                    env_assign(name, a);
                }
            }
            if let Some(v) = param_value(args, &["-name"]) {
                env_assign(&v.text, a);
            }
            simple.cd_only = true;
        }
        "powershell" | "pwsh" | "powershell_ise" => {
            simple.opaque = true;
            pwsh_invocation(c, args, cwd, ctx, depth, a);
        }
        "cmd" => {
            simple.opaque = true;
            match args.iter().position(|w| matches!(w.text.to_ascii_lowercase().as_str(), "/c" | "/k" | "/r")) {
                Some(i) => {
                    let rest: Vec<&str> = args[i + 1..].iter().map(|w| w.text.as_str()).collect();
                    if args[i + 1..].iter().any(|w| w.dynamic) {
                        a.ask("cmd code built at run time");
                    }
                    nested(Dialect::Cmd, &rest.join(" "), cwd, ctx, depth, a);
                }
                None => {
                    if let Some(stdin) = &c.stdin {
                        nested(Dialect::Cmd, stdin, cwd, ctx, depth, a);
                    }
                }
            }
        }
        "eval" => {
            simple.opaque = true;
            if args.iter().any(|w| w.dynamic) {
                a.ask("eval of text built at run time");
            }
            let text: Vec<&str> = args.iter().map(|w| w.text.as_str()).collect();
            nested(Dialect::Bash, &text.join(" "), cwd, ctx, depth, a);
        }
        "invoke-expression" | "iex" => {
            simple.opaque = true;
            let code = param_value(args, &["-command"]).or_else(|| pos.first().copied());
            match code {
                Some(w) if !w.dynamic => nested(Dialect::PowerShell, &w.text, cwd, ctx, depth, a),
                Some(_) => a.ask("Invoke-Expression of text built at run time"),
                // Piped in: the text is whatever the previous command produced.
                None => a.ask("Invoke-Expression of piped text"),
            }
        }
        "invoke-command" | "icm" => {
            simple.opaque = true;
            if param_value(args, &["-computername", "-session", "-hostname", "-cn", "-vmname", "-containerid"]).is_some() {
                a.ask("runs commands on another machine");
            }
            if let Some(f) = param_value(args, &["-filepath"]) {
                script(f, Some(Dialect::PowerShell), cwd, ctx, depth, a);
            }
        }
        "start-process" | "saps" | "start" if d == Dialect::PowerShell || prog != "start" => {
            simple.opaque = true;
            if param_value(args, &["-verb"]).is_some_and(|v| v.text.eq_ignore_ascii_case("runas")) {
                a.ask("starts a process elevated");
            }
            let file = param_value(args, &["-filepath", "-path"]).or_else(|| pos.first().copied());
            let list = param_value(args, &["-argumentlist", "-args"]).or_else(|| pos.get(1).copied());
            match file {
                Some(f) if f.dynamic => a.ask("starts a program chosen at run time"),
                Some(f) => {
                    let mut inner = vec![f.clone()];
                    if let Some(l) = list {
                        if l.dynamic {
                            inner.push(Word { text: l.text.clone(), dynamic: true, quoted: false });
                        } else {
                            inner.extend(split_args(&l.text));
                        }
                    }
                    command(c, &inner, cwd, ctx, depth, a);
                }
                None => {}
            }
        }
        "start" => {
            // cmd's `start ["title"] program args`.
            simple.opaque = true;
            let rest: Vec<Word> = args.iter().filter(|w| !w.text.starts_with('/')).cloned().collect();
            let rest = if rest.first().is_some_and(|w| w.quoted) { rest[1..].to_vec() } else { rest };
            if !rest.is_empty() {
                command(c, &rest, cwd, ctx, depth, a);
            }
        }
        "trap" => {
            // The action is shell code run later (on a signal or at exit).
            simple.opaque = true;
            let mut rest = args;
            if rest.first().is_some_and(|w| w.text == "--") {
                rest = &rest[1..];
            }
            match rest.first() {
                Some(w) if w.dynamic => a.ask("trap with code built at run time"),
                Some(w) if !w.text.starts_with('-') => nested(Dialect::Bash, &w.text, cwd, ctx, depth, a),
                _ => {}
            }
        }
        "set-alias" | "new-alias" | "sal" | "nal" => a.ask("defines an alias (later commands may not be what they say)"),
        "alias" if !args.is_empty() => a.ask("defines an alias (later commands may not be what they say)"),
        "." | "source" => {
            if let Some(f) = pos.first() {
                let kind = script_kind(&f.text).unwrap_or(d);
                script(f, Some(kind), cwd, ctx, depth, a);
            }
            simple.opaque = true;
        }
        "python" | "python3" | "py" | "pythonw" | "python2" => {
            if let Some(i) = args.iter().position(|w| w.text == "-c") {
                simple.opaque = true;
                match args.get(i + 1) {
                    Some(s) if s.dynamic => a.ask("python code built at run time"),
                    Some(s) => code_scan(&s.text, a, true),
                    None => {}
                }
            } else if let Some(i) = args.iter().position(|w| w.text == "-m") {
                if let Some(m) = args.get(i + 1) {
                    let rest: Vec<String> = args[i + 2..].iter().map(|w| w.text.clone()).collect();
                    let module = m.text.to_ascii_lowercase();
                    for h in registry_hosts(module.as_str(), &rest) {
                        a.host(h.to_string());
                    }
                    if module == "pip" && rest.first().is_some_and(|s| s == "uninstall") {
                        a.ask("uninstalls Python packages");
                    }
                    simple.read_only = module == "pip" && rest.first().is_some_and(|s| matches!(s.as_str(), "list" | "show" | "freeze"));
                }
            } else if let Some(f) = args.iter().find(|w| !w.text.starts_with('-')) {
                script(f, None, cwd, ctx, depth, a);
            } else if let Some(stdin) = &c.stdin {
                code_scan(stdin, a, true);
            } else if args.iter().any(|w| w.text == "--version" || w.text == "-V") {
                simple.read_only = true;
            } else {
                a.ask("runs code piped in");
            }
        }
        "node" | "deno" | "perl" | "ruby" | "php" | "lua" | "osascript" | "Rscript" | "rscript" => {
            let flag = args.iter().position(|w| matches!(w.text.as_str(), "-e" | "--eval" | "-p" | "--print" | "-E" | "-r"));
            if let Some(i) = flag {
                simple.opaque = true;
                match args.get(i + 1) {
                    Some(s) if s.dynamic => a.ask("code built at run time"),
                    Some(s) => code_scan(&s.text, a, true),
                    None => {}
                }
            } else if let Some(f) = args.iter().find(|w| !w.text.starts_with('-')) {
                if prog == "deno" && matches!(f.text.as_str(), "run" | "task" | "test") {
                    // deno subcommands
                } else {
                    script(f, None, cwd, ctx, depth, a);
                }
            } else if let Some(stdin) = &c.stdin {
                code_scan(stdin, a, true);
            } else if args.iter().any(|w| w.text == "--version" || w.text == "-v") {
                simple.read_only = true;
            } else {
                a.ask("runs code piped in");
            }
        }

        // ---- git and GitHub
        "git" => {
            let facts = git::examine(d, args, cwd.as_deref(), ctx, a);
            simple.read_only = facts.read_only && attached_values_inside(d, args, cwd, ctx);
        }
        "gh" => {
            let sub: Vec<String> = pos.iter().take(2).map(|w| w.text.to_ascii_lowercase()).collect();
            let read = match sub.first().map(String::as_str) {
                Some("pr" | "issue" | "run" | "repo" | "release" | "workflow" | "gist" | "label" | "cache" | "secret" | "variable") => {
                    sub.get(1).is_some_and(|s| matches!(s.as_str(), "view" | "list" | "diff" | "checks" | "status" | "watch" | "download"))
                }
                Some("status" | "search" | "browse" | "help" | "version" | "auth") => true,
                Some("api") => !args.iter().any(|w| matches!(w.text.as_str(), "-X" | "--method" | "-f" | "-F" | "--field" | "--raw-field" | "--input")),
                _ => false,
            };
            if !read {
                a.ask("gh changes something on GitHub");
            }
            a.host("github.com".into());
        }

        // ---- publishing and system installs: fixed approvals
        "npm" | "pnpm" | "yarn" | "bun" | "cargo" | "poetry" | "uv" | "twine" | "gem" | "dotnet" | "vsce" | "docker" | "podman" | "flit" | "hatch" | "mvn" | "gradle"
            if pos.iter().any(|w| matches!(w.text.as_str(), "publish" | "unpublish" | "deprecate" | "push" | "upload" | "owner" | "yank" | "dist-tag")) =>
        {
            a.ask("publishes to a registry");
            for h in registry_hosts(&prog, &simple.args) {
                a.host(h.to_string());
            }
        }
        "winget" | "choco" | "scoop" | "apt" | "apt-get" | "brew" | "msiexec" | "dnf" | "yum" | "pacman" | "snap" | "install-module" | "install-package" | "uninstall-module" => {
            a.ask("installs or removes software outside the workspace");
        }
        "reg" | "regedit" | "setx" | "schtasks" | "at" | "crontab" | "register-scheduledtask" | "new-scheduledtask" | "new-service" | "set-service" | "net" | "netsh" | "bcdedit" | "diskpart" | "format" | "mkfs" | "shutdown" | "restart-computer" | "stop-computer" | "reboot" | "set-executionpolicy" | "set-mppreference" | "add-mppreference" | "new-netfirewallrule" | "set-netfirewallprofile" | "mount" | "umount" | "launchctl" | "systemctl" | "service" | "useradd" | "usermod" | "passwd" | "wsl" => {
            if !(prog == "net" && pos.first().is_some_and(|w| matches!(w.text.to_ascii_lowercase().as_str(), "view" | "statistics"))) {
                a.ask("changes system configuration outside the workspace");
            }
        }
        "sc" if d != Dialect::PowerShell => a.ask("changes Windows services"),
        "mshta" | "rundll32" | "regsvr32" | "wscript" | "cscript" | "certutil" | "bitsadmin" | "installutil" | "regasm" | "msxsl" | "odbcconf" | "cmstp" => {
            a.ask("runs a Windows system program often used to fetch or run code");
        }
        "invoke-item" | "ii" | "explorer" | "xdg-open" | "open" => {}

        // ---- network
        "curl" | "wget" | "invoke-webrequest" | "iwr" | "invoke-restmethod" | "irm" | "start-bitstransfer" | "http" | "https" | "xh" | "aria2c" | "httpie" => {
            let fam = if prog == "curl" && d != Dialect::PowerShell {
                "curl"
            } else if prog == "wget" && d != Dialect::PowerShell {
                "wget"
            } else {
                "ps"
            };
            let values = fetch_value_flags(fam);
            let mut found = false;
            let mut i = 0;
            while i < args.len() {
                let w = &args[i];
                let t = w.text.as_str();
                let lower = t.to_ascii_lowercase();
                let takes = if fam == "ps" { values.contains(&lower.as_str()) } else { values.contains(&t) };
                if t.starts_with('-') && (lower == "-uri" || lower == "--url" || lower == "-source") {
                    if let Some(u) = args.get(i + 1) {
                        found |= url_host(u, a);
                    }
                    i += 2;
                    continue;
                }
                if t.starts_with('-') && takes {
                    if matches!(lower.as_str(), "-o" | "--output" | "-outfile" | "--output-document" | "-destination") || (fam == "wget" && t == "-O") {
                        if let Some(f) = args.get(i + 1) {
                            if !is_null_device(&f.text) && f.text != "-" {
                                write_target(d, f, cwd, ctx, a, "download");
                            }
                        }
                    }
                    i += 2;
                    continue;
                }
                if t.starts_with('-') {
                    i += 1;
                    continue;
                }
                found |= url_host(w, a);
                i += 1;
            }
            if !found {
                a.ask("network access to a host not visible in the command");
            }
        }
        "ssh" | "scp" | "sftp" | "rsync" | "ftp" | "nc" | "ncat" | "netcat" | "telnet" | "mosh" | "sshpass" => {
            let mut found = false;
            for w in &pos {
                if w.dynamic {
                    continue;
                }
                let t = w.text.as_str();
                let host = if matches!(prog.as_str(), "ssh" | "sftp" | "telnet" | "nc" | "ncat" | "netcat" | "ftp" | "mosh") {
                    host_of_url(t).or_else(|| {
                        let h = t.rsplit('@').next().unwrap_or(t);
                        (!h.is_empty() && !h.chars().all(|c| c.is_ascii_digit())).then(|| h.to_ascii_lowercase())
                    })
                } else {
                    host_of_url(t).filter(|_| t.contains(':') && !t.starts_with(['.', '/']))
                };
                if let Some(h) = host {
                    found = true;
                    if !is_local(&h) {
                        a.host(h);
                    }
                    if matches!(prog.as_str(), "ssh" | "sftp" | "telnet" | "nc" | "ncat" | "netcat" | "ftp" | "mosh") {
                        break;
                    }
                }
            }
            if prog == "ssh" && pos.len() > 1 {
                a.ask("runs commands on another machine");
            }
            if !found && !pos.is_empty() {
                a.ask("network access to a host not visible in the command");
            }
            if matches!(prog.as_str(), "scp" | "rsync") {
                if let Some(dst) = pos.last().filter(|w| !w.text.contains(':') || w.text.len() > 2 && w.text.as_bytes()[1] == b':') {
                    write_target(d, dst, cwd, ctx, a, "copy");
                }
            }
        }

        // ---- deletes and writes
        p if DELETES.contains(&p) => {
            let targets = path_targets(d, &prog, args, &pos);
            let mut inside = !targets.is_empty();
            for t in &targets {
                inside &= delete_target(d, t, cwd, ctx, a);
            }
            simple.fs_inside = inside;
        }
        "find" => {
            let roots: Vec<&Word> = args.iter().take_while(|w| !w.text.starts_with('-') && !matches!(w.text.as_str(), "(" | "!")).collect();
            let mut i = 0;
            let mut acts = false;
            while i < args.len() {
                let t = args[i].text.as_str();
                match t {
                    "-delete" => {
                        acts = true;
                        if roots.is_empty() {
                            delete_target(d, &Word::lit("."), cwd, ctx, a);
                        }
                        for r in &roots {
                            delete_target(d, r, cwd, ctx, a);
                        }
                    }
                    "-exec" | "-execdir" | "-ok" | "-okdir" => {
                        acts = true;
                        let end = args[i + 1..].iter().position(|w| matches!(w.text.as_str(), ";" | "+")).map_or(args.len(), |p| i + 1 + p);
                        let inner: Vec<Word> = args[i + 1..end]
                            .iter()
                            .map(|w| if w.text.contains("{}") { Word { text: w.text.clone(), dynamic: true, quoted: w.quoted } } else { w.clone() })
                            .collect();
                        if !inner.is_empty() {
                            command(c, &inner, cwd, ctx, depth, a);
                        }
                        i = end;
                    }
                    "-fprint" | "-fprint0" | "-fprintf" | "-fls" => {
                        acts = true;
                        if let Some(f) = args.get(i + 1) {
                            write_target(d, f, cwd, ctx, a, "find output");
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            let roots: Vec<Word> = roots.iter().map(|w| (*w).clone()).collect();
            simple.read_only = !acts && reads_inside(d, &roots, cwd, ctx);
        }
        "sed" | "perl-i" if args.iter().any(|w| w.text == "-i" || w.text.starts_with("-i") || w.text == "--in-place" || w.text.starts_with("--in-place=")) => {
            let has_script_flag = args.iter().any(|w| matches!(w.text.as_str(), "-e" | "-f" | "--expression" | "--file"));
            let files: Vec<&Word> = pos.iter().copied().skip(if has_script_flag { 0 } else { 1 }).collect();
            let mut inside = !files.is_empty();
            for f in files {
                inside &= write_target(d, f, cwd, ctx, a, "in-place edit");
            }
            simple.fs_inside = inside;
        }
        "sed" | "awk" | "gawk" | "sort" | "uniq" => {
            let writes = args.iter().any(|w| w.text == "-o" || w.text.starts_with("--output")) || (prog.ends_with("awk") && args.iter().any(|w| w.text.contains("system(") || w.text.contains("print >") || w.text.contains("| getline")))
                // `sort -ro out`, `sort --o=out` (an abbreviation), `--compress-program=sh` (runs it).
                || prog == "sort" && args.iter().any(|w| {
                    let t = w.text.as_str();
                    let name = t.split_once('=').map_or(t, |(n, _)| n);
                    (t.starts_with('-') && !t.starts_with("--") && t[1..].contains('o'))
                        || name.len() >= 3 && name.starts_with("--") && ("--output".starts_with(name) || name.len() >= 5 && "--compress-program".starts_with(name))
                });
            if prog.ends_with("awk") && args.iter().any(|w| w.text.contains("system(") || w.text.contains("|")) {
                a.ask("awk program runs commands");
            }
            simple.read_only = !writes && reads_inside(d, &args_after_script(&prog, args), cwd, ctx);
        }
        "dd" => {
            for w in args {
                if let Some(of) = w.text.strip_prefix("of=") {
                    write_target(d, &Word { text: of.to_string(), dynamic: w.dynamic, quoted: w.quoted }, cwd, ctx, a, "dd");
                }
            }
        }
        p if WRITERS.contains(&p) || (p == "sc" && d == Dialect::PowerShell) => {
            let targets = write_targets(d, &prog, args, &pos);
            let mut inside = !targets.is_empty();
            for t in &targets {
                inside &= write_target(d, t, cwd, ctx, a, "writes");
            }
            // `mv` also removes its sources.
            if matches!(prog.as_str(), "mv" | "move" | "move-item" | "mi") {
                for s in &pos[..pos.len().saturating_sub(1)] {
                    inside &= delete_target(d, s, cwd, ctx, a);
                }
            }
            simple.fs_inside = inside && FS_OPS.contains(&prog.as_str());
        }

        // ---- package managers and build tools (network for installs)
        "npm" | "pnpm" | "yarn" | "bun" | "npx" | "bunx" | "pnpx" | "pip" | "pip3" | "uv" | "uvx" | "pipx" | "poetry" | "pdm" | "cargo" | "go" | "gem" | "bundle" | "dotnet" => {
            for h in registry_hosts(&prog, &simple.args) {
                a.host(h.to_string());
            }
            if matches!(prog.as_str(), "pip" | "pip3") && pos.first().is_some_and(|w| w.text == "uninstall") {
                a.ask("uninstalls Python packages");
            }
            if prog == "cargo" && pos.first().is_some_and(|w| w.text == "uninstall") {
                a.ask("uninstalls a cargo binary");
            }
            if (prog == "npm" || prog == "pnpm" || prog == "yarn") && args.iter().any(|w| w.text == "-g" || w.text == "--global") {
                a.ask("changes global packages");
            }
            simple.read_only = match prog.as_str() {
                "cargo" => pos.first().is_some_and(|w| matches!(w.text.as_str(), "metadata" | "tree" | "--version" | "version" | "locate-project" | "pkgid" | "verify-project")) || args.iter().any(|w| w.text == "--version" || w.text == "-V"),
                "npm" | "pnpm" | "yarn" => pos.first().is_some_and(|w| matches!(w.text.as_str(), "ls" | "list" | "view" | "outdated" | "why" | "explain" | "info")) || args.iter().any(|w| w.text == "--version" || w.text == "-v"),
                "pip" | "pip3" => pos.first().is_some_and(|w| matches!(w.text.as_str(), "list" | "show" | "freeze" | "check")),
                "go" => pos.first().is_some_and(|w| matches!(w.text.as_str(), "version" | "env" | "list" | "doc" | "vet")),
                _ => false,
            };
        }
        p => {
            simple.read_only = READ_ONLY.contains(&p) || (p == "rustc" || p == "rustup" || p == "java" || p == "tsc") && args.iter().all(|w| w.text.starts_with('-')) && args.iter().any(|w| w.text.contains("version") || w.text == "-V");
            // `./ls` is a program in the workspace, not the system's `ls`.
            if raw.contains(['/', '\\']) && !is_system_path(raw) {
                simple.read_only = false;
            }
            // Flags that write or run (`tree -o`, `rg --pre`).
            if writes_or_runs(p, args) {
                simple.read_only = false;
            }
            // A reader's file arguments must stay inside the workspace (or the temp dir).
            if simple.read_only && FILE_READERS.contains(&p) && !reads_inside(d, args, cwd, ctx) {
                simple.read_only = false;
            }
        }
    }
    if c.redirects.iter().any(|r| r.writes() && !is_null_device(&r.target.text)) {
        simple.read_only = false;
        simple.writes_redirect = true;
    }
    a.commands.push(simple);
}

/// Programs whose positional arguments (and attached flag values) are files they read.
const FILE_READERS: &[&str] = &[
    "cat", "type", "head", "tail", "less", "more", "wc", "grep", "egrep", "fgrep", "rg", "ag", "ack", "ls", "dir", "ll",
    "la", "tree", "stat", "file", "du", "diff", "cmp", "comm", "cut", "jq", "column", "nl", "od", "xxd", "hexdump",
    "strings", "md5sum", "sha1sum", "sha256sum", "sha512sum", "b2sum", "cksum", "readlink", "realpath", "get-content",
    "gc", "select-string", "sls", "get-childitem", "gci", "test-path", "get-item", "gi", "get-itemproperty", "gp",
    "get-filehash", "findstr", "fd", "tac", "paste", "base64",
];

/// `/usr/bin/ls`, `C:\Windows\System32\x.exe`: a system program spelled with its directory.
fn is_system_path(raw: &str) -> bool {
    let l = raw.replace('\\', "/").to_ascii_lowercase();
    ["/usr/bin/", "/bin/", "/usr/local/bin/", "/mingw64/bin/", "c:/windows/", "/c/windows/"].iter().any(|p| l.starts_with(p))
}

/// Read-only programs given a flag that writes a file or runs one.
fn writes_or_runs(prog: &str, args: &[Word]) -> bool {
    args.iter().any(|w| {
        let t = w.text.as_str();
        let name = t.split_once('=').map_or(t, |(n, _)| n);
        match prog {
            "tree" => t.starts_with('-') && !t.starts_with("--") && t[1..].contains('o') || name == "--output",
            "rg" => name == "--pre" || name.len() >= 4 && "--pre".starts_with(name),
            _ => false,
        }
    })
}

/// sed/awk read the files after their script.
fn args_after_script(prog: &str, args: &[Word]) -> Vec<Word> {
    if !(prog == "sed" || prog.ends_with("awk")) {
        return args.to_vec();
    }
    let mut seen_script = args.iter().any(|w| matches!(w.text.as_str(), "-e" | "-f" | "--expression" | "--file"));
    let mut out = Vec::new();
    for w in args {
        if w.text.starts_with('-') {
            continue;
        }
        if !seen_script {
            seen_script = true;
            continue;
        }
        out.push(w.clone());
    }
    out
}

/// Every file a reader is given stays inside the writable roots or the temp dir: positional
/// arguments, `--flag=value` values, and the attached value of `-f` (grep's pattern file).
/// Brace expansion and run-time words count as outside.
fn reads_inside(d: Dialect, args: &[Word], cwd: &Option<PathBuf>, ctx: &ShellCtx<'_>) -> bool {
    for w in args {
        let t = w.text.as_str();
        if w.dynamic || !w.quoted && t.contains('{') && t.contains('}') && t.contains(',') {
            return false;
        }
        let value = if let Some(long) = t.strip_prefix("--") {
            long.split_once('=').map(|(_, v)| v)
        } else if t.starts_with('-') && t.len() > 1 {
            t.strip_prefix("-f").filter(|v| !v.is_empty())
        } else {
            Some(t)
        };
        let Some(v) = value.filter(|v| !v.is_empty()) else { continue };
        if !path_inside(d, v, cwd, ctx) {
            return false;
        }
    }
    true
}

/// `--x=value` values that look like paths stay inside (git's `--contents=`, `--output=`).
fn attached_values_inside(d: Dialect, args: &[Word], cwd: &Option<PathBuf>, ctx: &ShellCtx<'_>) -> bool {
    args.iter().all(|w| match w.text.strip_prefix("--").and_then(|l| l.split_once('=')) {
        Some((_, v)) if looks_like_path(v) => path_inside(d, v, cwd, ctx),
        _ => true,
    })
}

fn looks_like_path(v: &str) -> bool {
    v.starts_with(['/', '\\', '~', '.']) || v.len() > 2 && v.as_bytes()[1] == b':' || v.contains("../") || v.contains("..\\")
}

fn path_inside(d: Dialect, v: &str, cwd: &Option<PathBuf>, ctx: &ShellCtx<'_>) -> bool {
    match cwd.as_deref().and_then(|c| resolve_scratch(d, c, v)) {
        Some(p) => ctx.writable(&p) || p.starts_with(std::env::temp_dir()),
        None => false,
    }
}

/// .NET static calls that write, run or connect (`[IO.File]::WriteAllText(...)`).
fn dotnet_scan<'w>(words: impl Iterator<Item = &'w Word>, a: &mut Analysis) {
    for w in words {
        let l = w.text.to_ascii_lowercase();
        if !(l.contains('[') && l.contains("]::")) {
            continue;
        }
        if ["io.file]", "io.directory]", "system.io.", "process]", "net.", "registry", "setenvironmentvariable"].iter().any(|t| l.contains(t)) {
            a.ask("calls a .NET API that writes, runs or connects");
        }
    }
}

/// `powershell -Command ...`, `-EncodedCommand`, `-File`, or a positional command.
fn pwsh_invocation(c: &Cmd, args: &[Word], cwd: &Option<PathBuf>, ctx: &ShellCtx<'_>, depth: u8, a: &mut Analysis) {
    const VALUE: [&str; 9] = ["-executionpolicy", "-ep", "-ex", "-windowstyle", "-w", "-outputformat", "-inputformat", "-workingdirectory", "-configurationname"];
    let mut i = 0;
    while i < args.len() {
        let w = &args[i];
        let lower = w.text.to_ascii_lowercase();
        if VALUE.contains(&lower.as_str()) {
            i += 2;
            continue;
        }
        if ["-command", "-c", "-commandwithargs", "-cwa"].contains(&lower.as_str()) || (!lower.starts_with('-') && !lower.ends_with(".ps1")) {
            let start = if lower.starts_with('-') { i + 1 } else { i };
            let rest = &args[start.min(args.len())..];
            if rest.first().is_some_and(|w| w.text == "-") {
                if let Some(stdin) = &c.stdin {
                    nested(Dialect::PowerShell, stdin, cwd, ctx, depth, a);
                }
                return;
            }
            if rest.iter().any(|w| w.dynamic && !w.text.starts_with("{...}")) {
                a.ask("PowerShell code built at run time");
            }
            let text: Vec<&str> = rest.iter().map(|w| w.text.as_str()).collect();
            nested(Dialect::PowerShell, &text.join(" "), cwd, ctx, depth, a);
            return;
        }
        if ["-encodedcommand", "-enc", "-e", "-ec", "-en", "-encoded"].contains(&lower.as_str()) {
            match args.get(i + 1).and_then(|w| decode_utf16_base64(&w.text)) {
                Some(code) => nested(Dialect::PowerShell, &code, cwd, ctx, depth, a),
                None => a.ask("an encoded PowerShell command that could not be decoded"),
            }
            return;
        }
        if lower == "-file" || lower == "-f" || lower.ends_with(".ps1") {
            let f = if lower.starts_with('-') { args.get(i + 1) } else { Some(w) };
            if let Some(f) = f {
                script(f, Some(Dialect::PowerShell), cwd, ctx, depth, a);
            }
            return;
        }
        i += 1;
    }
    if let Some(stdin) = &c.stdin {
        nested(Dialect::PowerShell, stdin, cwd, ctx, depth, a);
    }
}

/// `-EncodedCommand`'s base64 of UTF-16LE text.
fn decode_utf16_base64(s: &str) -> Option<String> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut bits = 0u32;
    let mut n = 0u32;
    let mut bytes = Vec::new();
    for ch in s.trim().bytes() {
        if ch == b'=' {
            break;
        }
        let v = ALPHABET.iter().position(|c| *c == ch)? as u32;
        bits = (bits << 6) | v;
        n += 6;
        if n >= 8 {
            n -= 8;
            bytes.push((bits >> n) as u8);
            bits &= (1 << n) - 1;
        }
    }
    if bytes.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = bytes.chunks(2).map(|p| u16::from_le_bytes([p[0], p[1]])).collect();
    String::from_utf16(&units).ok()
}

/// Splits an `-ArgumentList` string into words (double quotes group).
fn split_args(s: &str) -> Vec<Word> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut any = false;
    for ch in s.chars() {
        match ch {
            '"' | '\'' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() || any {
                    out.push(Word { text: std::mem::take(&mut cur), dynamic: false, quoted: any });
                    any = false;
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() || any {
        out.push(Word { text: cur, dynamic: false, quoted: any });
    }
    out
}

/// A URL-ish word's host recorded; true when one was found (or the word is dynamic: then asks).
fn url_host(w: &Word, a: &mut Analysis) -> bool {
    if w.dynamic {
        a.ask("network access to a host chosen at run time");
        return true;
    }
    match host_of_bare(&w.text) {
        Some(h) => {
            if !is_local(&h) {
                a.host(h);
            }
            true
        }
        None => false,
    }
}

/// Environment names whose change redirects what later commands run.
fn env_assign(name: &str, a: &mut Analysis) {
    let n = name.trim_start_matches('$');
    let n = n.strip_prefix("env:").or_else(|| n.strip_prefix("ENV:")).or_else(|| n.strip_prefix("Env:")).unwrap_or(n);
    let upper = n.to_ascii_uppercase();
    if upper.starts_with("GIT_") {
        a.ask("changes git's environment");
    } else if matches!(upper.as_str(), "PATH" | "LD_PRELOAD" | "LD_LIBRARY_PATH" | "BASH_ENV" | "ENV" | "PROMPT_COMMAND" | "NODE_OPTIONS" | "PYTHONSTARTUP" | "PSMODULEPATH" | "COMSPEC" | "SHELLOPTS" | "PERL5OPT" | "RUBYOPT" | "CARGO_HOME" | "RUSTC_WRAPPER")
        || upper.starts_with("DYLD_")
    {
        a.ask(format!("changes {upper} (what later commands run)"));
    }
}

/// Records a write to `w`; true when it stays inside the writable roots.
fn write_target(d: Dialect, w: &Word, cwd: &Option<PathBuf>, ctx: &ShellCtx<'_>, a: &mut Analysis, what: &str) -> bool {
    if is_null_device(&w.text) {
        return true;
    }
    if w.dynamic {
        a.ask(format!("{what} to a path chosen at run time"));
        return false;
    }
    match cwd.as_deref().and_then(|c| resolve_scratch(d, c, &w.text)) {
        Some(p) if ctx.is_protected(&p) => {
            a.deny(format!("{what} into a protected path ({})", p.display()));
            false
        }
        Some(p) if ctx.writable(&p) => true,
        _ => {
            a.ask(format!("{what} outside the {}", place(ctx)));
            false
        }
    }
}

/// Records a delete of `w`; true when it is strictly inside a writable root.
fn delete_target(d: Dialect, w: &Word, cwd: &Option<PathBuf>, ctx: &ShellCtx<'_>, a: &mut Analysis) -> bool {
    if w.dynamic {
        a.ask("delete of a path chosen at run time");
        return false;
    }
    match cwd.as_deref().and_then(|c| resolve_scratch(d, c, &w.text)) {
        Some(p) if ctx.is_protected(&p) => {
            a.deny(format!("delete in a protected path ({})", p.display()));
            false
        }
        Some(p) if ctx.writable(&p) => {
            // Deleting a root itself is allowed where it is writable, but is not "inside".
            !is_root(ctx, &p)
        }
        _ => {
            a.ask(format!("delete outside the {}", place(ctx)));
            false
        }
    }
}

fn is_root(ctx: &ShellCtx<'_>, p: &Path) -> bool {
    match ctx.scope {
        Scope::Workspace { roots } => roots.iter().any(|r| same(r, p)),
        Scope::Sandbox { root, .. } => same(root, p),
    }
}

fn place(ctx: &ShellCtx<'_>) -> &'static str {
    match ctx.scope {
        Scope::Workspace { .. } => "workspace",
        Scope::Sandbox { .. } => "sandbox",
    }
}

/// [`resolve`], plus Git Bash's `/tmp` as the temp dir.
fn resolve_scratch(d: Dialect, cwd: &Path, arg: &str) -> Option<PathBuf> {
    if cfg!(windows) && d == Dialect::Bash {
        if let Some(rest) = arg.strip_prefix("/tmp").filter(|r| r.is_empty() || r.starts_with('/')) {
            return Some(super::lexical_join(&std::env::temp_dir(), Path::new(rest.trim_start_matches('/'))));
        }
    }
    resolve(d, cwd, arg)
}

/// Positional arguments: not flags, not cmd switches, not PowerShell parameter values.
fn positionals<'w>(d: Dialect, prog: &str, args: &'w [Word]) -> Vec<&'w Word> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut ended = false;
    while i < args.len() {
        let w = &args[i];
        let t = w.text.as_str();
        if ended || w.quoted && !t.starts_with('-') {
            out.push(w);
            i += 1;
            continue;
        }
        if t == "--" && d != Dialect::PowerShell {
            ended = true;
            i += 1;
            continue;
        }
        if d == Dialect::PowerShell && t.starts_with('-') && t.len() > 1 && t[1..].chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
            // `-Name value` unless a known switch or `-Name:value`.
            let lower = t.to_ascii_lowercase();
            if !lower.contains(':') && !ps_switch(&lower) && args.get(i + 1).is_some_and(|n| !n.text.starts_with('-')) {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if t.starts_with('-') && t.len() > 1 {
            i += 1 + flag_takes_value(prog, t) as usize;
            continue;
        }
        if (d == Dialect::Cmd || (d != Dialect::PowerShell && t.len() == 2)) && t.starts_with('/') && t.len() <= 4 && t[1..].chars().all(|c| c.is_ascii_alphabetic()) {
            // cmd-style switches (`/s`, `/q`, `/MIR`).
            if d == Dialect::Cmd || DELETES.contains(&prog) || WRITERS.contains(&prog) {
                i += 1;
                continue;
            }
        }
        if d == Dialect::Cmd && t.starts_with('/') {
            i += 1;
            continue;
        }
        out.push(w);
        i += 1;
    }
    out
}

/// PowerShell switch parameters (no value follows).
fn ps_switch(lower: &str) -> bool {
    matches!(
        lower,
        "-recurse" | "-force" | "-whatif" | "-confirm" | "-append" | "-nonewline" | "-noclobber" | "-passthru" | "-verbose"
            | "-debug" | "-container" | "-wait" | "-nonewwindow" | "-usebasicparsing" | "-raw" | "-asjob" | "-r" | "-rf"
            | "-fo" | "-file" | "-directory" | "-hidden" | "-readonly" | "-system" | "-quiet" | "-wh" | "-noprofile"
            | "-noninteractive" | "-nologo" | "-simplematch" | "-casesensitive" | "-list" | "-leaf" | "-parent"
            | "-resolve" | "-isabsolute" | "-unique" | "-descending" | "-compress" | "-asstring" | "-asplaintext"
    )
}

fn flag_takes_value(prog: &str, flag: &str) -> bool {
    match prog {
        "cp" | "mv" | "install" | "ln" => matches!(flag, "-t" | "-S" | "-m" | "-o" | "-g" | "--suffix"),
        "mkdir" => matches!(flag, "-m" | "--mode"),
        "touch" => matches!(flag, "-d" | "-t" | "-r" | "--date" | "--reference"),
        "ssh" | "scp" | "sftp" => matches!(flag, "-i" | "-p" | "-P" | "-o" | "-l" | "-F" | "-J" | "-L" | "-R" | "-D" | "-c" | "-b" | "-E" | "-S" | "-W" | "-w"),
        "rsync" => matches!(flag, "-e" | "--rsh" | "--exclude" | "--include" | "--filter"),
        "find" => false,
        "chmod" | "chown" | "chgrp" => matches!(flag, "--reference"),
        "git" => false,
        "gh" => matches!(flag, "-R" | "--repo" | "-t" | "--title" | "-b" | "--body" | "-B" | "--base" | "-H" | "--head" | "-X" | "--method" | "-f" | "-F" | "--jq" | "-q"),
        "sed" => matches!(flag, "-e" | "-f" | "--expression" | "--file"),
        "grep" | "rg" => matches!(flag, "-e" | "-f" | "-m" | "-A" | "-B" | "-C" | "-g" | "--glob" | "-t" | "--type"),
        _ => false,
    }
}

/// The value of a PowerShell parameter (`-Path x` or `-Path:x`).
fn param_value<'w>(args: &'w [Word], names: &[&str]) -> Option<&'w Word> {
    for (i, w) in args.iter().enumerate() {
        let lower = w.text.to_ascii_lowercase();
        let (name, inline) = match lower.split_once(':') {
            Some((n, _)) => (n.to_string(), true),
            None => (lower.clone(), false),
        };
        if names.contains(&name.as_str()) {
            if inline {
                return None;
            }
            return args.get(i + 1);
        }
    }
    None
}

/// Delete targets: positionals, plus `-Path`/`-LiteralPath` values.
fn path_targets<'w>(d: Dialect, _prog: &str, args: &'w [Word], pos: &[&'w Word]) -> Vec<&'w Word> {
    let mut out: Vec<&Word> = pos.to_vec();
    if d == Dialect::PowerShell {
        for name in ["-path", "-literalpath", "-lp"] {
            if let Some(v) = param_value(args, &[name]) {
                out.push(v);
            }
        }
    }
    out
}

/// The paths a writer writes.
fn write_targets<'w>(d: Dialect, prog: &str, args: &'w [Word], pos: &[&'w Word]) -> Vec<&'w Word> {
    let mut out: Vec<&Word> = Vec::new();
    let flag_value = |names: &[&str]| -> Option<&'w Word> {
        args.iter().position(|w| names.contains(&w.text.as_str())).and_then(|i| args.get(i + 1))
    };
    match prog {
        "cp" | "copy" | "xcopy" | "ln" | "install" => {
            if let Some(t) = flag_value(&["-t", "--target-directory"]) {
                out.push(t);
            } else if let Some(last) = pos.last() {
                out.push(last);
            }
        }
        "robocopy" => out.extend(pos.get(1).copied()),
        "mv" | "move" => out.extend(pos.last().copied()),
        "copy-item" | "cpi" => {
            out.extend(param_value(args, &["-destination"]).or(pos.get(1).copied()));
        }
        "move-item" | "mi" => {
            out.extend(pos.iter().copied());
            out.extend(param_value(args, &["-destination"]));
            out.extend(param_value(args, &["-path", "-literalpath"]));
        }
        "chmod" | "chown" | "chgrp" => out.extend(pos.iter().skip(1).copied()),
        "icacls" | "takeown" | "attrib" => out.extend(pos.first().copied()),
        "new-item" | "ni" | "set-content" | "add-content" | "ac" | "out-file" | "clear-content" | "clc" | "tee-object" | "rename-item" | "rni" | "set-itemproperty" | "new-itemproperty" | "remove-itemproperty" | "set-item" | "export-csv" | "sc" => {
            let named = param_value(args, &["-path", "-literalpath", "-filepath", "-lp"]);
            match named {
                Some(v) => out.push(v),
                None => out.extend(pos.first().copied()),
            }
        }
        "ren" | "rename" => out.extend(pos.first().copied()),
        _ => out.extend(pos.iter().copied()),
    }
    let _ = d;
    out
}

/// Words after leading flags (and the values of `value_flags`).
fn skip_flags<'w>(args: &'w [Word], value_flags: &[&str]) -> &'w [Word] {
    let mut i = 0;
    while i < args.len() && args[i].text.starts_with('-') {
        if args[i].text == "--" {
            return &args[i + 1..];
        }
        i += if value_flags.contains(&args[i].text.as_str()) { 2 } else { 1 };
    }
    &args[i.min(args.len())..]
}
