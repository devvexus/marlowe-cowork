//! Shell command analysis for the permission layer (design 2.3 M8, 3.5 tripwire, A16).
//!
//! A command line is parsed by its own dialect ([`bash`], [`pwsh`], [`cmd`]) into the simple
//! commands it would run, including those inside `$(...)`, backticks, subshells, script blocks,
//! here-docs fed to an interpreter, `bash -c`, `pwsh -Command`, `cmd /c`, `eval`, `iex '...'`,
//! `Start-Process`, `xargs`, `find -exec` and readable script files. [`analyze`] walks every one
//! of them and reports:
//!
//! - hard denies ([`Analysis::deny`]): force-push variants, `--no-verify`, deleting a protected
//!   branch, writes to protected paths. No mode, rule or approval lifts them.
//! - tripwires ([`Analysis::asks`]): fixed approvals (push, publish, deletes and writes outside the
//!   writable roots, git environment or config injection, elevation, commands chosen at run time)
//!   and anything it cannot parse. They Ask in every mode, `bypass` included.
//! - network hosts ([`Analysis::hosts`]): first contact Asks unless allowed.
//! - per command: its normalised line for rules, whether it only reads, whether it is a file
//!   operation inside the roots.
//!
//! It is lexical and errs towards asking: what a started program does is invisible to it, so it
//! is a tripwire in front of the sandbox, not confinement (9.3-a). The crew's sandbox policy is
//! [`classify_sandbox_shell`] / [`classify_code`] (moved from `marlowe-crew/src/shell_policy.rs`).

mod analyze;
pub mod bash;
pub mod cmd;
pub mod danger;
mod git;
mod net;
mod paths;
pub mod pwsh;
pub mod pwsh_ast;
pub mod words;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_flash;

use std::path::{Path, PathBuf};

pub use analyze::{analyze, classify_code};
pub use net::{host_of_url, registry_hosts};
pub use paths::{lexical_join, within};

/// Nesting deeper than this (`bash -c "bash -c ..."`, substitutions in substitutions) is refused.
pub const MAX_DEPTH: u8 = 8;

/// Largest script file read to look inside.
pub const SCRIPT_SCAN_BYTES: u64 = 512 * 1024;

/// Which shell language a command line is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dialect {
    /// bash / sh (Git Bash on Windows).
    Bash,
    /// Windows PowerShell or pwsh.
    PowerShell,
    /// `cmd.exe`.
    Cmd,
}

impl Dialect {
    /// The dialect of a shell tool by its name (`bash`, `powershell`); `None` for other tools.
    pub fn of_tool(name: &str) -> Option<Dialect> {
        match name {
            "bash" | "sh" => Some(Dialect::Bash),
            "powershell" | "pwsh" => Some(Dialect::PowerShell),
            "cmd" => Some(Dialect::Cmd),
            _ => None,
        }
    }

    /// Parses `src` in this dialect into the simple commands it runs.
    pub fn parse(self, src: &str) -> Result<Vec<Cmd>, Unparseable> {
        parse_at(self, src, 0)
    }
}

pub(crate) fn parse_at(dialect: Dialect, src: &str, depth: u8) -> Result<Vec<Cmd>, Unparseable> {
    if depth > MAX_DEPTH {
        return Err(Unparseable("nested too deeply".into()));
    }
    match dialect {
        Dialect::Bash => bash::parse(src, depth),
        Dialect::PowerShell => pwsh::parse(src, depth),
        Dialect::Cmd => cmd::parse(src, depth),
    }
}

/// Why a command line could not be parsed (an unclosed quote, a `case` statement, ...). The
/// caller asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unparseable(pub String);

impl std::fmt::Display for Unparseable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One shell word after quote removal. `dynamic` when part of it is only known at run time
/// (`$X`, `$(...)`, `%X%`, a script block's value).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Word {
    pub text: String,
    pub dynamic: bool,
    /// Some part of it was quoted (so `{`, `if`, `NAME=x` are plain words).
    pub quoted: bool,
}

impl Word {
    pub fn lit(text: impl Into<String>) -> Self {
        Word { text: text.into(), dynamic: false, quoted: false }
    }
}

/// An output or input redirection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    /// `>`, `>>`, `<`.
    pub op: &'static str,
    pub target: Word,
}

impl Redirect {
    /// Whether the target is written.
    pub fn writes(&self) -> bool {
        self.op.starts_with('>')
    }
}

/// One simple command: environment assignments, words, redirections, and text it reads on stdin
/// from a here-doc or here-string (looked inside when the program is an interpreter).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    pub dialect: Dialect,
    pub assigns: Vec<(String, Word)>,
    pub words: Vec<Word>,
    pub redirects: Vec<Redirect>,
    pub stdin: Option<String>,
    /// The first word is run even when quoted or computed: always in bash and cmd; in PowerShell
    /// only after the call operator `&` (otherwise `'text'` or `$x.Length` is an expression).
    pub invoked: bool,
}

impl Cmd {
    pub fn new(dialect: Dialect) -> Self {
        Cmd {
            dialect,
            assigns: Vec::new(),
            words: Vec::new(),
            redirects: Vec::new(),
            stdin: None,
            invoked: dialect != Dialect::PowerShell,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.assigns.is_empty() && self.words.is_empty() && self.redirects.is_empty() && self.stdin.is_none()
    }
}

/// Where writes are free, for [`analyze`].
#[derive(Clone, Copy)]
pub enum Scope<'a> {
    /// The conductor: the session's workspace roots (plus `--add-dir`s).
    Workspace { roots: &'a [PathBuf] },
    /// A crew sandbox.
    Sandbox { root: &'a Path, kind: SandboxKind, allows_write: &'a dyn Fn(&Path) -> bool },
}

/// The crew sandbox kinds, as the policy sees them (mirrors `marlowe_proto::SandboxKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxKind {
    Worktree,
    Scratch,
    None,
}

/// What [`analyze`] needs besides the command.
#[derive(Clone)]
pub struct ShellCtx<'a> {
    /// Where the command starts.
    pub cwd: PathBuf,
    pub scope: Scope<'a>,
    /// Paths no command may write or delete (`Marlowe_Harness`, the default profile).
    pub protected: &'a [PathBuf],
    /// Branch names a push may never delete (besides `main`, `master`).
    pub protected_branches: &'a [String],
    /// Extra writable places (the temp dir).
    pub scratch: &'a [PathBuf],
    /// Reads a script file an interpreter is handed, to look inside; `None` when unreadable.
    pub read_script: &'a dyn Fn(&Path) -> Option<String>,
}

impl<'a> ShellCtx<'a> {
    /// A context for the conductor in `roots`, starting in `cwd`, reading scripts from disk.
    pub fn workspace(cwd: PathBuf, roots: &'a [PathBuf]) -> Self {
        ShellCtx {
            cwd,
            scope: Scope::Workspace { roots },
            protected: &[],
            protected_branches: &[],
            scratch: &[],
            read_script: &read_script_file,
        }
    }

    /// Whether `path` (absolute) may be written without asking.
    pub fn writable(&self, path: &Path) -> bool {
        if self.scratch.iter().any(|s| within(s, path)) {
            return true;
        }
        match self.scope {
            Scope::Workspace { roots } => roots.iter().any(|r| within(r, path)),
            Scope::Sandbox { kind, allows_write, .. } => kind != SandboxKind::None && allows_write(path),
        }
    }

    pub fn is_protected(&self, path: &Path) -> bool {
        self.protected.iter().any(|p| within(p, path))
    }
}

/// Reads a script of at most [`SCRIPT_SCAN_BYTES`].
pub fn read_script_file(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > SCRIPT_SCAN_BYTES {
        return None;
    }
    std::fs::read(path).ok().map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// One command the line would run, as rules see it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Simple {
    /// Lower-case program name without directory or `.exe` (`git`, `remove-item`).
    pub program: String,
    pub args: Vec<String>,
    /// `program args...` joined by single spaces: what `bash(git push:*)` is matched against.
    pub line: String,
    /// Any word was only known at run time.
    pub dynamic: bool,
    /// Only reads (ls, cat, git status, Get-ChildItem, ...).
    pub read_only: bool,
    /// A file operation (mkdir, touch, cp, mv, rm, New-Item, ...) whose targets are all writable.
    pub fs_inside: bool,
    /// Changes only the shell's own state: its directory (`cd`, `Set-Location`) or variables
    /// (`export X=1`). Environment names that redirect later commands are tripwires anyway.
    pub cd_only: bool,
    /// Arbitrary code handed to an interpreter (`bash -c`, `python -c`, `eval`, `iex`) or a
    /// wrapper (`sudo`, `xargs`): never generalised into a rule.
    pub opaque: bool,
    /// Destroys work or system state ([`danger`]): only an exact rule allows it, `accept_edits`
    /// does not free it, and an approval of it is not widened.
    pub dangerous: Option<String>,
    /// Redirects its output into a file (not the null device): no allow rule covers that.
    pub writes_redirect: bool,
}

impl Simple {
    /// Commands that never need a rule of their own (cd, echo, true): skipped when every command
    /// of a line must be allowed.
    pub fn trivial(&self) -> bool {
        !self.writes_redirect
            && (self.cd_only
                || matches!(
                    self.program.as_str(),
                    "true" | "false" | ":" | "echo" | "printf" | "write-output" | "write-host" | "pwd" | "get-location"
                ))
    }
}

/// What a command line would do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Analysis {
    /// Every command it runs, outermost first.
    pub commands: Vec<Simple>,
    /// A hard deny: no mode, rule or approval lifts it.
    pub deny: Option<String>,
    /// Tripwires: fixed approvals and unparseable input. Each Asks in every mode.
    pub asks: Vec<String>,
    /// Network hosts it contacts (lower-case; localhost excluded).
    pub hosts: Vec<String>,
    /// The argv of every command when the line is plain words joined by `&&`, `||`, `;`, `|`
    /// ([`words`]); `None` when anything is expanded, redirected, grouped or computed.
    pub literal: Option<Vec<Vec<String>>>,
}

impl Analysis {
    /// Every command only reads files inside the roots (and nothing tripped).
    pub fn read_only(&self) -> bool {
        self.clean() && !self.commands.is_empty() && self.commands.iter().all(|c| c.dangerous.is_none() && (c.read_only || c.trivial()))
    }

    /// Every command reads or is a file operation inside the writable roots, and none destroys
    /// work.
    pub fn fs_inside(&self) -> bool {
        self.clean()
            && !self.commands.is_empty()
            && self.commands.iter().all(|c| c.dangerous.is_none() && (c.read_only || c.fs_inside || c.trivial()))
    }

    /// Why some command is dangerous, if one is.
    pub fn dangerous(&self) -> Option<&str> {
        self.commands.iter().find_map(|c| c.dangerous.as_deref())
    }

    fn clean(&self) -> bool {
        self.deny.is_none() && self.asks.is_empty() && self.hosts.is_empty()
    }

    /// The commands a rule must cover (all but the trivial ones).
    pub fn rule_commands(&self) -> impl Iterator<Item = &Simple> {
        self.commands.iter().filter(|c| !c.trivial())
    }

    pub(crate) fn ask(&mut self, why: impl Into<String>) {
        let why = why.into();
        if !self.asks.contains(&why) {
            self.asks.push(why);
        }
    }

    pub(crate) fn deny(&mut self, why: impl Into<String>) {
        if self.deny.is_none() {
            self.deny = Some(why.into());
        }
    }

    pub(crate) fn host(&mut self, host: String) {
        if !self.hosts.contains(&host) {
            self.hosts.push(host);
        }
    }
}

/// The crew sandbox's verdict on a shell command or code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Free,
    /// Needs approval; the reason names what tripped.
    Ask(String),
    /// Refused in every mode.
    Deny(String),
}

/// The crew's shell policy: free inside the sandbox; asks for pushes, git writes outside the
/// worktree or to what it shares with the checkout, deletes, copies, moves and redirects outside,
/// git environment changes, new network hosts, and anything after untrusted content; denies force
/// pushes, `--no-verify` and shared `git config` writes.
pub fn classify_sandbox_shell(
    dialect: Dialect,
    root: &Path,
    kind: SandboxKind,
    allows_write: &dyn Fn(&Path) -> bool,
    command: &str,
    tainted: bool,
) -> Verdict {
    let shown = clip(command.lines().next().unwrap_or(""), 100);
    if tainted {
        return Verdict::Ask(format!("shell after untrusted content: {shown}"));
    }
    let ctx = ShellCtx {
        cwd: root.to_path_buf(),
        scope: Scope::Sandbox { root, kind, allows_write },
        protected: &[],
        protected_branches: &[],
        scratch: &[],
        read_script: &read_script_file,
    };
    let a = analyze(dialect, command, &ctx);
    if let Some(why) = a.deny {
        return Verdict::Deny(format!("{why}: {shown}"));
    }
    if let Some(why) = a.asks.first() {
        return Verdict::Ask(format!("{why}: {shown}"));
    }
    if let Some(h) = a.hosts.first() {
        return Verdict::Ask(format!("contacts {h}: {shown}"));
    }
    Verdict::Free
}

/// First `max` characters of `s`, with an ellipsis when cut.
pub(crate) fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}
