//! Commands as a program and arguments, never a shell string.
//!
//! Project commands, services and migrations run as `argv` through `marlowe_os::child`. A string
//! from a profile override or from the model is split here (quotes, no escapes beyond `\"` inside
//! double quotes) and refused when it needs a shell, so nothing in this crate can smuggle a pipe
//! or a `&&` past the permission layer, which classifies shells (`bash`, `powershell`) itself.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A program and its arguments.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Cmd {
    pub argv: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CmdError {
    #[error("the command is empty")]
    Empty,
    #[error("unterminated quote in `{0}`")]
    Quote(String),
    #[error(
        "`{0}` is shell syntax; a command here is a program and its arguments. Put the steps in a \
         script file and run that, or use the shell tools"
    )]
    ShellSyntax(String),
}

impl Cmd {
    pub fn new<S: AsRef<str>>(parts: &[S]) -> Self {
        Self { argv: parts.iter().map(|s| s.as_ref().to_string()).collect() }
    }

    /// Splits `s` into arguments. Single quotes are literal; double quotes allow `\"`; outside
    /// quotes a backslash is just a character (Windows paths). Refuses unquoted `| & ; < > `` ` ``
    /// and `$(`.
    pub fn parse(s: &str) -> Result<Self, CmdError> {
        let argv = split_args(s)?;
        if argv.is_empty() {
            return Err(CmdError::Empty);
        }
        Ok(Self { argv })
    }

    pub fn program(&self) -> &str {
        self.argv.first().map(String::as_str).unwrap_or("")
    }

    pub fn args(&self) -> &[String] {
        self.argv.get(1..).unwrap_or(&[])
    }

    /// The command as one line, quoting arguments that need it. For logs and approvals.
    pub fn display(&self) -> String {
        self.argv.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
    }

    /// This command with `extra` appended.
    pub fn with_args<S: AsRef<str>>(&self, extra: &[S]) -> Self {
        let mut argv = self.argv.clone();
        argv.extend(extra.iter().map(|s| s.as_ref().to_string()));
        Self { argv }
    }

    /// The program's file name without directory or extension, lower-cased (`C:\x\NPM.CMD` is `npm`).
    pub fn program_name(&self) -> String {
        program_name(self.program())
    }

    /// Whether the command hands arbitrary code to a shell or an interpreter inline (`bash -c`,
    /// `powershell -Command`, `cmd /c`, `python -c`, `node -e`). Those are shell commands in
    /// disguise: the permission layer's tripwire must see them, so they always ask.
    pub fn is_inline_code(&self) -> bool {
        let name = self.program_name();
        let has = |flags: &[&str]| self.args().iter().any(|a| flags.iter().any(|f| a.eq_ignore_ascii_case(f)));
        match name.as_str() {
            "bash" | "sh" | "zsh" | "fish" | "dash" | "ksh" | "cmd" | "powershell" | "pwsh" | "wsl" | "busybox" | "env" => true,
            "python" | "python3" | "py" | "pypy" => has(&["-c"]),
            "node" | "nodejs" | "bun" | "deno" => has(&["-e", "--eval", "-p", "--print", "eval"]),
            "ruby" | "perl" | "php" | "lua" => has(&["-e", "-r", "-E"]),
            _ => false,
        }
    }
}

fn quote(a: &str) -> String {
    if !a.is_empty() && !a.contains(|c: char| c.is_whitespace() || c == '"' || c == '\'') {
        return a.to_string();
    }
    format!("\"{}\"", a.replace('"', "\\\""))
}

/// File stem of `program`, lower-cased.
pub fn program_name(program: &str) -> String {
    let base = program.rsplit(['/', '\\']).next().unwrap_or(program);
    let stem = match base.rsplit_once('.') {
        Some((s, ext)) if matches!(ext.to_ascii_lowercase().as_str(), "exe" | "cmd" | "bat" | "com" | "ps1" | "sh") => s,
        _ => base,
    };
    stem.to_ascii_lowercase()
}

/// See [`Cmd::parse`].
pub fn split_args(s: &str) -> Result<Vec<String>, CmdError> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => cur.push(ch),
                        None => return Err(CmdError::Quote(s.to_string())),
                    }
                }
            }
            '"' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') if chars.peek() == Some(&'"') => {
                            chars.next();
                            cur.push('"');
                        }
                        Some(ch) => cur.push(ch),
                        None => return Err(CmdError::Quote(s.to_string())),
                    }
                }
            }
            c if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            '|' | '&' | ';' | '<' | '>' | '`' => return Err(CmdError::ShellSyntax(c.to_string())),
            '$' if chars.peek() == Some(&'(') => return Err(CmdError::ShellSyntax("$(".into())),
            ch => {
                started = true;
                cur.push(ch);
            }
        }
    }
    if started {
        out.push(cur);
    }
    Ok(out)
}

/// Whether `name` is a plausible environment variable name.
pub fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && name.len() <= 128
}

/// Finds `program` the way a shell would (the `PATH` search, with `PATHEXT` on Windows), so a
/// `.cmd` shim such as `npm` resolves. A program with a directory part is looked up relative to
/// `cwd`. `None` when nothing is found.
pub fn resolve_program(program: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    resolve_in(program, cwd, std::env::var_os("PATH").as_deref(), std::env::var("PATHEXT").ok().as_deref())
}

fn resolve_in(program: &str, cwd: Option<&Path>, path: Option<&std::ffi::OsStr>, pathext: Option<&str>) -> Option<PathBuf> {
    let exts: Vec<String> = if cfg!(windows) {
        let raw = pathext.filter(|s| !s.is_empty()).unwrap_or(".COM;.EXE;.BAT;.CMD");
        raw.split(';').filter(|e| !e.is_empty()).map(|e| e.to_ascii_lowercase()).collect()
    } else {
        Vec::new()
    };
    let try_dir = |dir: &Path| -> Option<PathBuf> {
        let base = dir.join(program);
        let has_ext = Path::new(program).extension().is_some();
        if has_ext || exts.is_empty() {
            if base.is_file() {
                return Some(base);
            }
        }
        for ext in &exts {
            let mut name = base.as_os_str().to_os_string();
            name.push(ext);
            let p = PathBuf::from(name);
            if p.is_file() {
                return Some(p);
            }
        }
        None
    };
    if program.contains(['/', '\\']) {
        let p = Path::new(program);
        if p.is_absolute() {
            return try_abs(p, &exts);
        }
        return try_dir(cwd.unwrap_or(Path::new(".")));
    }
    // The Microsoft Store's `python.exe` stubs live in WindowsApps and open the Store; skip them.
    path.into_iter()
        .flat_map(std::env::split_paths)
        .filter(|d| !d.to_string_lossy().to_ascii_lowercase().contains("\\windowsapps"))
        .find_map(|d| try_dir(&d))
}

fn try_abs(p: &Path, exts: &[String]) -> Option<PathBuf> {
    if p.is_file() {
        return Some(p.to_path_buf());
    }
    exts.iter().find_map(|e| {
        let mut name = p.as_os_str().to_os_string();
        name.push(e);
        let c = PathBuf::from(name);
        c.is_file().then_some(c)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_quotes_and_keeps_windows_backslashes() {
        let c = Cmd::parse(r#"node "C:\Program Files\x\run.js" --name 'a b' "say \"hi\"" plain"#).unwrap();
        assert_eq!(c.argv, ["node", r"C:\Program Files\x\run.js", "--name", "a b", r#"say "hi""#, "plain"]);
        assert_eq!(Cmd::parse("  ").unwrap_err(), CmdError::Empty);
        assert!(matches!(Cmd::parse("echo 'x"), Err(CmdError::Quote(_))));
        assert_eq!(Cmd::parse(r#"echo """#).unwrap().argv, ["echo", ""]);
    }

    #[test]
    fn shell_syntax_is_refused_unless_quoted() {
        for bad in ["a && b", "a | b", "a; b", "a > f", "a < f", "echo `x`", "echo $(x)", "a &"] {
            assert!(matches!(Cmd::parse(bad), Err(CmdError::ShellSyntax(_))), "{bad}");
        }
        assert!(Cmd::parse(r#"grep "a|b" file"#).is_ok());
        assert!(Cmd::parse("echo $HOME").is_ok(), "a bare variable is just text here");
    }

    #[test]
    fn display_quotes_only_what_needs_it() {
        let c = Cmd::new(&["cargo", "test", "--", "a b", ""]);
        assert_eq!(c.display(), r#"cargo test -- "a b" """#);
        assert_eq!(Cmd::parse(&c.display()).unwrap(), c);
    }

    #[test]
    fn program_names_and_inline_code() {
        assert_eq!(program_name(r"C:\x\NPM.CMD"), "npm");
        assert_eq!(program_name("/usr/bin/python3"), "python3");
        assert!(Cmd::new(&["bash", "-c", "x"]).is_inline_code());
        assert!(Cmd::new(&["powershell", "-Command", "x"]).is_inline_code());
        assert!(Cmd::new(&["python", "-c", "print(1)"]).is_inline_code());
        assert!(Cmd::new(&["node", "-e", "1"]).is_inline_code());
        assert!(!Cmd::new(&["python", "server.py"]).is_inline_code());
        assert!(!Cmd::new(&["npm", "run", "dev"]).is_inline_code());
    }

    #[test]
    fn env_names() {
        assert!(valid_env_name("HF_TOKEN"));
        assert!(!valid_env_name("1X"));
        assert!(!valid_env_name("A=B"));
        assert!(!valid_env_name(""));
    }

    #[test]
    fn resolution_finds_a_shim_through_pathext() {
        let dir = std::env::temp_dir().join(format!("marlowe-verify-cmd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let name = if cfg!(windows) { "shimtool.cmd" } else { "shimtool" };
        std::fs::write(dir.join(name), "").unwrap();
        let path = std::env::join_paths([&dir]).unwrap();
        let found = resolve_in("shimtool", None, Some(&path), Some(".EXE;.CMD"));
        assert_eq!(found, Some(dir.join(name)));
        assert_eq!(resolve_in("missing-tool", None, Some(&path), Some(".EXE;.CMD")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
