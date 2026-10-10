//! Allow / ask / deny rules (design 2.3 M7, M11): `bash(git push:*)`, `edit(src/**)`,
//! `read(//C:/secrets/**)`, `web_fetch(domain:docs.rs)`, `mcp__github`, `mcp__github__create_issue`.
//! Claude Code spellings (`Bash(npm run test:*)`, `Edit(...)`, `WebFetch(domain:...)`) parse to the
//! same rules.
//!
//! Precedence is deny > ask > allow, whatever layer a rule came from. A shell line is allowed
//! only when every command in it is; it is asked or denied when any one is. A call with several
//! paths is allowed only when every path is. [`propose`] turns an approved call into the narrowest
//! useful rule (tool plus normalised command prefix, path glob or domain), never a bare built-in
//! tool, so "don't ask again" for `cargo test` still asks for `cargo publish` or `git push`.

pub mod policy;

use std::path::{Path, PathBuf};

use crate::shell::{within, Analysis};

/// What a rule does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Action {
    Allow,
    Ask,
    Deny,
}

/// A parse failure, for the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleError(pub String);

impl std::fmt::Display for RuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The kind of tool a rule's name stands for.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ToolPat {
    /// Every shell tool (`bash`, `powershell`): rules are about the command, not the dialect.
    Shell,
    /// Every tool that writes files (write, edit, multi_edit, notebook_edit, ...).
    FileWrite,
    /// Every tool that reads files by path (read, glob, grep, ...).
    FileRead,
    /// One tool by name (`web_fetch`, `python`, `mcp__srv__tool`).
    Named(String),
    /// Every tool of an MCP server (`mcp__srv`, `mcp__srv__*`).
    McpServer(String),
}

/// What a rule's parenthesised part matches.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Spec {
    /// Any call of the tool.
    Any,
    /// A command pattern: `git push:*` (that prefix, at a word boundary), `npm run *` (glob), or
    /// an exact line.
    Command(String),
    /// A gitignore-style path glob.
    Path(String),
    /// `domain:host` (the host or any subdomain; `*.x` only subdomains).
    Domain(String),
}

/// One parsed rule.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Rule {
    pub tool: ToolPat,
    pub spec: Spec,
    /// The text as written (shown in `/permissions`, written back to settings).
    pub text: String,
}

impl Rule {
    /// Parses `tool` or `tool(specifier)`.
    pub fn parse(text: &str) -> Result<Rule, RuleError> {
        let t = text.trim();
        if t.is_empty() {
            return Err(RuleError("an empty rule".into()));
        }
        let (name, inner) = match t.find('(') {
            Some(open) => {
                if !t.ends_with(')') {
                    return Err(RuleError(format!("`{t}`: a rule's specifier ends with `)`")));
                }
                (t[..open].trim(), Some(t[open + 1..t.len() - 1].trim()))
            }
            None => (t, None),
        };
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '*' || c == '-') {
            return Err(RuleError(format!("`{t}`: `{name}` is not a tool name")));
        }
        let tool = tool_pat(name);
        let spec = match inner {
            None | Some("") | Some("*") => Spec::Any,
            Some(s) => match &tool {
                ToolPat::Shell => Spec::Command(s.to_string()),
                ToolPat::FileWrite | ToolPat::FileRead => Spec::Path(s.to_string()),
                ToolPat::Named(n) if is_web(n) => match s.strip_prefix("domain:") {
                    Some(d) if !d.trim().is_empty() => Spec::Domain(d.trim().to_ascii_lowercase()),
                    _ => return Err(RuleError(format!("`{t}`: web rules take `domain:host`"))),
                },
                ToolPat::Named(n) if n == "python" => Spec::Command(s.to_string()),
                ToolPat::Named(_) => Spec::Path(s.to_string()),
                ToolPat::McpServer(_) => return Err(RuleError(format!("`{t}`: MCP rules take no specifier"))),
            },
        };
        Ok(Rule { tool, spec, text: t.to_string() })
    }

    /// Whether this rule's tool part covers `call`.
    fn covers_tool(&self, call: &Subject<'_>) -> bool {
        match &self.tool {
            ToolPat::Shell => call.kind == Kind::Shell,
            ToolPat::FileWrite => call.kind == Kind::FileWrite,
            ToolPat::FileRead => call.kind == Kind::FileRead,
            ToolPat::Named(n) => n == call.tool,
            ToolPat::McpServer(s) => call.tool.strip_prefix("mcp__").and_then(|r| r.split_once("__")).is_some_and(|(srv, _)| srv == s),
        }
    }
}

fn is_web(name: &str) -> bool {
    matches!(name, "web_fetch" | "web_search" | "http_check" | "browser")
}

/// A rule's tool name, Claude Code spellings included.
fn tool_pat(name: &str) -> ToolPat {
    if let Some(rest) = name.strip_prefix("mcp__") {
        return match rest.split_once("__") {
            None => ToolPat::McpServer(rest.to_string()),
            Some((srv, "*")) => ToolPat::McpServer(srv.to_string()),
            Some(_) => ToolPat::Named(name.to_string()),
        };
    }
    let snake = snake_case(name);
    match snake.as_str() {
        "bash" | "powershell" | "power_shell" | "shell" => ToolPat::Shell,
        "edit" | "write" | "multi_edit" | "notebook_edit" | "edit_lines" => ToolPat::FileWrite,
        "read" | "notebook_read" | "glob" | "grep" | "ls" => {
            // `read` covers every reader; `glob`/`grep` are their own.
            if snake == "read" || snake == "notebook_read" || snake == "ls" {
                ToolPat::FileRead
            } else {
                ToolPat::Named(snake)
            }
        }
        "task" => ToolPat::Named("agent".into()),
        "todo_write" => ToolPat::Named("todo".into()),
        "ask_user_question" => ToolPat::Named("ask_user".into()),
        "exit_plan_mode" => ToolPat::Named("exit_plan".into()),
        "bash_output" => ToolPat::Named("job_output".into()),
        "kill_shell" | "kill_bash" => ToolPat::Named("job_kill".into()),
        _ => ToolPat::Named(snake),
    }
}

/// `WebFetch` -> `web_fetch`; already-snake names unchanged.
pub fn snake_case(name: &str) -> String {
    if name.eq_ignore_ascii_case("powershell") {
        return "powershell".into();
    }
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 && !out.ends_with('_') {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// What kind of call a subject is (from the tool's manifest).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Shell,
    FileWrite,
    FileRead,
    Web,
    Mcp,
    Other,
}

/// A tool call as rules see it.
#[derive(Debug, Clone)]
pub struct Subject<'a> {
    pub tool: &'a str,
    pub kind: Kind,
    /// Absolute paths the call reads or writes.
    pub paths: Vec<PathBuf>,
    /// For shells: the line's analysis (its commands are matched one by one).
    pub shell: Option<&'a Analysis>,
    /// For python and similar: the code or script text, matched as one line.
    pub code: Option<&'a str>,
    /// The network host it reaches.
    pub domain: Option<String>,
    /// Where relative globs are anchored (the workspace root).
    pub root: Option<&'a Path>,
    /// The user's home, for `~/` globs.
    pub home: Option<&'a Path>,
}

impl<'a> Subject<'a> {
    pub fn new(tool: &'a str, kind: Kind) -> Self {
        Subject { tool, kind, paths: Vec::new(), shell: None, code: None, domain: None, root: None, home: None }
    }
}

/// Rules from every layer, in no particular order (precedence is by action).
#[derive(Debug, Clone, Default)]
pub struct RuleSet {
    rules: Vec<(Action, Rule)>,
}

/// Which rule decided, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Matched {
    pub action: Action,
    pub rule: String,
}

impl RuleSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, action: Action, rule: Rule) {
        if !self.rules.iter().any(|(a, r)| *a == action && r.text == rule.text) {
            self.rules.push((action, rule));
        }
    }

    /// Parses and adds; returns the parse error, if any (the rule is skipped).
    pub fn add_text(&mut self, action: Action, text: &str) -> Result<(), RuleError> {
        self.add(action, Rule::parse(text)?);
        Ok(())
    }

    pub fn extend(&mut self, other: &RuleSet) {
        for (a, r) in &other.rules {
            self.add(*a, r.clone());
        }
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (Action, &Rule)> {
        self.rules.iter().map(|(a, r)| (*a, r))
    }

    pub fn contains(&self, action: Action, text: &str) -> bool {
        self.rules.iter().any(|(a, r)| *a == action && r.text == text.trim())
    }

    /// The deciding rule for `call`: any matching deny, else any matching ask, else an allow
    /// that covers every part of the call.
    pub fn evaluate(&self, call: &Subject<'_>) -> Option<Matched> {
        for action in [Action::Deny, Action::Ask] {
            if let Some(r) = self.rules.iter().filter(|(a, _)| *a == action).find(|(_, r)| matches_any_part(r, call)) {
                return Some(Matched { action, rule: r.1.text.clone() });
            }
        }
        let allows: Vec<&Rule> = self.rules.iter().filter(|(a, _)| *a == Action::Allow).map(|(_, r)| r).filter(|r| r.covers_tool(call)).collect();
        if allows.is_empty() {
            return None;
        }
        covered_by(&allows, call).map(|rule| Matched { action: Action::Allow, rule })
    }
}

/// Whether `rule` matches some part of the call (for deny and ask).
fn matches_any_part(rule: &Rule, call: &Subject<'_>) -> bool {
    if !rule.covers_tool(call) {
        return false;
    }
    match &rule.spec {
        Spec::Any => true,
        Spec::Command(p) => match (call.shell, call.code) {
            (Some(a), _) => a.commands.iter().any(|c| command_matches(p, &c.line)),
            (None, Some(code)) => command_matches(p, code.trim()),
            _ => false,
        },
        Spec::Path(g) => call.paths.iter().any(|p| path_matches(g, p, call.root, call.home)),
        Spec::Domain(d) => call.domain.as_deref().is_some_and(|h| domain_matches(d, h)),
    }
}

/// When the allows cover every part of the call, the rule text to report.
fn covered_by(allows: &[&Rule], call: &Subject<'_>) -> Option<String> {
    if let Some(r) = allows.iter().find(|r| r.spec == Spec::Any) {
        return Some(r.text.clone());
    }
    match call.kind {
        Kind::Shell => {
            let a = call.shell?;
            let cmds: Vec<_> = a.rule_commands().collect();
            if cmds.is_empty() {
                return None;
            }
            let mut first = None;
            for c in cmds {
                // A redirect into a file is an effect no command rule names.
                if c.writes_redirect {
                    return None;
                }
                // A destructive command is allowed only by a rule that names it exactly.
                let fits = |p: &str| command_matches(p, &c.line) && (c.dangerous.is_none() || is_exact(p));
                let r = allows.iter().find(|r| matches!(&r.spec, Spec::Command(p) if fits(p)))?;
                first.get_or_insert_with(|| r.text.clone());
            }
            first
        }
        _ if call.code.is_some() => {
            let code = call.code?.trim();
            allows.iter().find(|r| matches!(&r.spec, Spec::Command(p) if command_matches(p, code))).map(|r| r.text.clone())
        }
        Kind::Web => {
            let h = call.domain.as_deref()?;
            allows.iter().find(|r| matches!(&r.spec, Spec::Domain(d) if domain_matches(d, h))).map(|r| r.text.clone())
        }
        _ => {
            if call.paths.is_empty() {
                return None;
            }
            let mut first = None;
            for p in &call.paths {
                let r = allows.iter().find(|r| matches!(&r.spec, Spec::Path(g) if path_matches(g, p, call.root, call.home)))?;
                first.get_or_insert_with(|| r.text.clone());
            }
            first
        }
    }
}

/// `git push:*` (prefix at a word boundary), a glob with `*`, or an exact line. Whitespace runs
/// count as one space; the program word compares case-insensitively.
pub fn command_matches(pattern: &str, line: &str) -> bool {
    let norm = |s: &str| -> String {
        let mut words = s.split_whitespace();
        let first = words.next().map(|w| w.to_ascii_lowercase()).unwrap_or_default();
        std::iter::once(first).chain(words.map(str::to_string)).collect::<Vec<_>>().join(" ")
    };
    let line = norm(line);
    if let Some(prefix) = pattern.trim().strip_suffix(":*") {
        let prefix = norm(prefix);
        return line == prefix || line.starts_with(&format!("{prefix} "));
    }
    let pattern = norm(pattern);
    if pattern.contains('*') {
        return glob_chars(&pattern, &line);
    }
    line == pattern
}

/// A command pattern with no prefix (`:*`) or glob (`*`) part.
pub fn is_exact(pattern: &str) -> bool {
    !pattern.trim().ends_with(":*") && !pattern.contains('*')
}

/// `*` matches any run of characters (spaces included).
fn glob_chars(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut rest = text;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            match rest.strip_prefix(part) {
                Some(r) => rest = r,
                None => return false,
            }
        } else if i == parts.len() - 1 {
            return part.is_empty() || rest.ends_with(part);
        } else {
            match rest.find(part) {
                Some(at) => rest = &rest[at + part.len()..],
                None => return false,
            }
        }
    }
    rest.is_empty()
}

/// The host is `d` or below it; `*.d` only below.
pub fn domain_matches(d: &str, host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    match d.strip_prefix("*.") {
        Some(base) => host.ends_with(&format!(".{base}")),
        None => host == d || host.ends_with(&format!(".{d}")),
    }
}

/// A gitignore-style glob against an absolute path. `//abs` and `C:/abs` are absolute, `~/x` is
/// under home, `/x` and `x/y` are anchored at `root`, and a pattern with no `/` (like `.env` or
/// `*.pem`) matches a file name at any depth.
pub fn path_matches(glob: &str, path: &Path, root: Option<&Path>, home: Option<&Path>) -> bool {
    let g = glob.trim().replace('\\', "/");
    let p = slash(path);
    let (base, pat): (Option<String>, String) = if let Some(abs) = g.strip_prefix("//") {
        (None, abs.to_string())
    } else if g.len() >= 2 && g.as_bytes()[1] == b':' {
        (None, g.clone())
    } else if let Some(rest) = g.strip_prefix("~/") {
        match home {
            Some(h) => (Some(slash(h)), rest.to_string()),
            None => return false,
        }
    } else if let Some(rest) = g.strip_prefix('/') {
        match root {
            Some(r) => (Some(slash(r)), rest.to_string()),
            None => return false,
        }
    } else if !g.trim_end_matches("/**").trim_end_matches('/').contains('/') {
        // No slash: any depth (match the trailing segments).
        let pat = g.trim_start_matches("./").to_string();
        let segs: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
        let pat_segs: Vec<&str> = pat.split('/').filter(|s| !s.is_empty()).collect();
        if let Some(r) = root {
            if !within(r, path) {
                // Relative patterns only apply inside the workspace.
                return false;
            }
        }
        return (0..segs.len()).any(|i| match_segments(&pat_segs, &segs[i..]));
    } else {
        match root {
            Some(r) => (Some(slash(r)), g.trim_start_matches("./").to_string()),
            None => return false,
        }
    };
    let full = match base {
        Some(b) => format!("{}/{}", b.trim_end_matches('/'), pat),
        None => pat,
    };
    let pat_segs: Vec<&str> = full.split('/').filter(|s| !s.is_empty()).collect();
    let segs: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    match_segments(&pat_segs, &segs)
}

fn slash(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

/// Segment-wise glob: `**` spans any number of segments, `*` and `?` stay within one.
fn match_segments(pat: &[&str], segs: &[&str]) -> bool {
    match pat.split_first() {
        None => segs.is_empty(),
        Some((&"**", rest)) => (0..=segs.len()).any(|i| match_segments(rest, &segs[i..])),
        Some((first, rest)) => match segs.split_first() {
            Some((s, srest)) => match_one(&case(first), &case(s)) && match_segments(rest, srest),
            None => false,
        },
    }
}

fn case(s: &str) -> String {
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s.to_string()
    }
}

/// One segment against a pattern with `*` and `?`.
fn match_one(pat: &str, s: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pat.chars().collect(), s.chars().collect());
    let (mut pi, mut ti, mut star, mut mark) = (0usize, 0usize, None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// The rules to offer as "don't ask again" for an approved call: one per command for shells
/// (`bash(cargo test:*)`), the containing directory for file tools (`edit(src/**)`), the domain
/// for web tools, the exact name for MCP tools. Empty when nothing narrower than the bare tool
/// fits (opaque code, wrappers, deletes): that approval covers the one call.
pub fn propose(call: &Subject<'_>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |r: String| {
        if !out.contains(&r) {
            out.push(r);
        }
    };
    match call.kind {
        Kind::Shell => {
            let Some(a) = call.shell else { return Vec::new() };
            if a.deny.is_some() || !a.asks.is_empty() {
                return Vec::new();
            }
            for c in a.rule_commands() {
                if c.opaque || c.dynamic || c.writes_redirect {
                    return Vec::new();
                }
                if c.dangerous.is_some() || DANGEROUS_PREFIX.contains(&c.program.as_str()) {
                    // Only this exact command, never its neighbours (`rm -rf build ~`).
                    push(format!("{}({})", call.tool, c.line));
                    continue;
                }
                push(format!("{}({}:*)", call.tool, command_prefix(&c.program, &c.args)));
            }
        }
        Kind::FileWrite | Kind::FileRead => {
            let family = if call.kind == Kind::FileWrite { "edit" } else { "read" };
            for p in &call.paths {
                let dir = p.parent().unwrap_or(p);
                let glob = match call.root.filter(|r| within(r, dir)) {
                    Some(r) => {
                        let rel = dir.strip_prefix(r).map(|d| d.to_string_lossy().replace('\\', "/")).unwrap_or_default();
                        if rel.is_empty() {
                            "/**".to_string()
                        } else {
                            format!("{rel}/**")
                        }
                    }
                    None => format!("//{}/**", dir.to_string_lossy().replace('\\', "/").trim_end_matches('/')),
                };
                push(format!("{family}({glob})"));
            }
        }
        Kind::Web => {
            if let Some(h) = &call.domain {
                push(format!("{}(domain:{h})", call.tool));
            }
        }
        Kind::Mcp => push(call.tool.to_string()),
        Kind::Other => {}
    }
    out
}

/// Programs whose approval is never widened into a rule.
const DANGEROUS_PREFIX: &[&str] = &[
    "rm", "rmdir", "rd", "del", "erase", "remove-item", "ri", "sudo", "doas", "gsudo", "eval", "iex",
    "invoke-expression", "xargs", "env", "curl", "wget", "invoke-webrequest", "iwr", "invoke-restmethod", "irm",
    "ssh", "scp", "chmod", "chown", "dd", "start-process", "saps", "powershell", "pwsh", "bash", "sh", "cmd",
];

/// `cargo test --release -p x` -> `cargo test`; `npm run build` -> `npm run build`;
/// `python -m pytest -q` -> `python -m pytest`; `ls -la` -> `ls`.
pub fn command_prefix(program: &str, args: &[String]) -> String {
    let word = |s: &str| !s.is_empty() && !s.starts_with('-') && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.:@".contains(c)) && !s.contains("..");
    let mut parts = vec![program.to_string()];
    let runner = matches!(program, "npm" | "pnpm" | "yarn" | "bun" | "uv" | "poetry" | "pdm" | "hatch" | "dotnet" | "go" | "cargo" | "deno");
    match args.first() {
        Some(m) if m == "-m" && matches!(program, "python" | "python3" | "py") => {
            parts.push("-m".into());
            if let Some(module) = args.get(1).filter(|s| word(s)) {
                parts.push(module.clone());
            }
        }
        Some(sub) if word(sub) && !sub.contains('.') || (word(sub) && runner) => {
            parts.push(sub.clone());
            if runner && matches!(sub.as_str(), "run" | "exec" | "x" | "tool") {
                if let Some(target) = args.get(1).filter(|s| word(s)) {
                    parts.push(target.clone());
                }
            }
        }
        Some(script) if matches!(program, "python" | "python3" | "py" | "node" | "ruby" | "perl") && !script.starts_with('-') => {
            parts.push(script.clone());
        }
        _ => {}
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::{analyze, Dialect, ShellCtx};

    fn root() -> PathBuf {
        PathBuf::from(if cfg!(windows) { r"C:\ws\proj" } else { "/ws/proj" })
    }

    fn shell_eval(set: &RuleSet, cmd: &str) -> Option<Action> {
        let roots = [root()];
        let a = analyze(Dialect::Bash, cmd, &ShellCtx::workspace(root(), &roots));
        let mut s = Subject::new("bash", Kind::Shell);
        s.shell = Some(&a);
        set.evaluate(&s).map(|m| m.action)
    }

    fn set(rules: &[(Action, &str)]) -> RuleSet {
        let mut s = RuleSet::new();
        for (a, t) in rules {
            s.add_text(*a, t).unwrap();
        }
        s
    }

    #[test]
    fn parity_m7_precedence_table() {
        use Action::*;
        let r = set(&[
            (Allow, "bash(git:*)"),
            (Ask, "bash(git push:*)"),
            (Deny, "bash(git push --force:*)"),
            (Allow, "Bash(npm run test:*)"),
            (Allow, "edit(src/**)"),
            (Deny, "edit(src/secrets/**)"),
            (Ask, "Edit(*.lock)"),
            (Allow, "WebFetch(domain:docs.rs)"),
            (Deny, "web_fetch(domain:evil.example)"),
            (Allow, "mcp__github"),
            (Deny, "mcp__github__delete_repo"),
        ]);
        // (input, expected) rows: deny > ask > allow, whatever order the rules were written in.
        let table: &[(&str, Option<Action>)] = &[
            ("git status", Some(Allow)),
            ("git push origin main", Some(Ask)),
            ("git push --force origin main", Some(Deny)),
            ("npm run test -- --watch", Some(Allow)),
            ("npm run testx", None),
            ("npm run build", None),
            ("git status && npm run test", Some(Allow)),
            ("git status && cargo build", None),
            ("cargo build; git push", Some(Ask)),
            ("cd src && git log", Some(Allow)),
        ];
        for (cmd, want) in table {
            assert_eq!(shell_eval(&r, cmd), *want, "{cmd}");
        }
        let root = root();
        let file = |tool: &'static str, rel: &str| -> Option<Action> {
            let mut s = Subject::new(tool, if tool == "read" { Kind::FileRead } else { Kind::FileWrite });
            s.paths = vec![root.join(rel)];
            s.root = Some(&root);
            r.evaluate(&s).map(|m| m.action)
        };
        assert_eq!(file("edit", "src/main.rs"), Some(Allow));
        assert_eq!(file("write", "src/a/b.rs"), Some(Allow), "edit rules cover every file-writing tool");
        assert_eq!(file("multi_edit", "src/secrets/k.txt"), Some(Deny));
        assert_eq!(file("edit", "Cargo.lock"), Some(Ask));
        assert_eq!(file("edit", "src/Cargo.lock"), Some(Ask), "no-slash globs match at any depth");
        assert_eq!(file("edit", "README.md"), None);
        let web = |host: &str| {
            let mut s = Subject::new("web_fetch", Kind::Web);
            s.domain = Some(host.into());
            r.evaluate(&s).map(|m| m.action)
        };
        assert_eq!(web("docs.rs"), Some(Allow));
        assert_eq!(web("api.docs.rs"), Some(Allow));
        assert_eq!(web("evil.example"), Some(Deny));
        assert_eq!(web("example.com"), None);
        let mcp = |tool: &'static str| r.evaluate(&Subject::new(tool, Kind::Mcp)).map(|m| m.action);
        assert_eq!(mcp("mcp__github__list_issues"), Some(Allow));
        assert_eq!(mcp("mcp__github__delete_repo"), Some(Deny));
        assert_eq!(mcp("mcp__gitlab__list"), None);
    }

    #[test]
    fn rules_parse_both_spellings_and_reject_junk() {
        assert_eq!(Rule::parse("Bash(npm test:*)").unwrap().tool, ToolPat::Shell);
        assert_eq!(Rule::parse("PowerShell(Get-ChildItem:*)").unwrap().tool, ToolPat::Shell);
        assert_eq!(Rule::parse("MultiEdit(src/**)").unwrap().tool, ToolPat::FileWrite);
        assert_eq!(Rule::parse("WebFetch(domain:x.org)").unwrap().spec, Spec::Domain("x.org".into()));
        assert_eq!(Rule::parse("mcp__srv__*").unwrap().tool, ToolPat::McpServer("srv".into()));
        assert_eq!(Rule::parse("Task").unwrap().tool, ToolPat::Named("agent".into()));
        assert!(Rule::parse("bash(git").is_err());
        assert!(Rule::parse("web_fetch(x.org)").is_err());
        assert!(Rule::parse("").is_err());
        assert!(Rule::parse("rm -rf").is_err());
    }

    #[test]
    fn proposals_are_narrow_and_never_bare() {
        let roots = [root()];
        let ctx = ShellCtx::workspace(root(), &roots);
        let prop = |cmd: &str| {
            let a = analyze(Dialect::Bash, cmd, &ctx);
            let mut s = Subject::new("bash", Kind::Shell);
            s.shell = Some(&a);
            propose(&s)
        };
        assert_eq!(prop("cargo test --release -p x"), ["bash(cargo test:*)"]);
        assert_eq!(prop("cd sub && npm run build -- --prod"), ["bash(npm run build:*)"]);
        assert_eq!(prop("python -m pytest -q tests"), ["bash(python -m pytest:*)"]);
        assert_eq!(prop("cargo build && cargo test"), ["bash(cargo build:*)", "bash(cargo test:*)"]);
        assert_eq!(prop("rm -rf target"), ["bash(rm -rf target)"], "destructive: exact only");
        let mut exact = RuleSet::new();
        exact.add_text(Action::Allow, "bash(rm -rf target)").unwrap();
        assert_eq!(shell_eval(&exact, "rm -rf target"), Some(Action::Allow));
        assert_eq!(shell_eval(&exact, "rm -rf target src"), None);
        let mut wide = RuleSet::new();
        wide.add_text(Action::Allow, "bash(rm:*)").unwrap();
        assert_eq!(shell_eval(&wide, "rm -rf target"), None, "a prefix rule never covers a destructive command");
        assert_eq!(shell_eval(&wide, "rm old.txt"), Some(Action::Allow));
        assert!(prop("cargo test > log.txt").is_empty(), "redirects are never widened");
        assert!(prop("bash -c 'cargo test'").is_empty());
        assert!(prop("git push").is_empty(), "fixed approvals are never widened");
        // The proposal covers the call it came from, and not its neighbours.
        let mut allowed = RuleSet::new();
        for r in prop("cargo test --release") {
            allowed.add_text(Action::Allow, &r).unwrap();
        }
        assert_eq!(shell_eval(&allowed, "cargo test"), Some(Action::Allow));
        assert_eq!(shell_eval(&allowed, "cargo publish"), None);
        assert_eq!(shell_eval(&allowed, "cargo testx"), None);
        let root = root();
        let mut s = Subject::new("edit", Kind::FileWrite);
        s.paths = vec![root.join("src").join("lib.rs")];
        s.root = Some(&root);
        assert_eq!(propose(&s), ["edit(src/**)"]);
        let mut w = Subject::new("web_fetch", Kind::Web);
        w.domain = Some("docs.rs".into());
        assert_eq!(propose(&w), ["web_fetch(domain:docs.rs)"]);
    }

    #[test]
    fn globs() {
        let root = root();
        let r = Some(root.as_path());
        assert!(path_matches("src/**", &root.join("src/a/b.rs"), r, None));
        assert!(path_matches("/src/*.rs", &root.join("src/a.rs"), r, None));
        assert!(!path_matches("/src/*.rs", &root.join("src/a/b.rs"), r, None));
        assert!(path_matches(".env", &root.join("deep/.env"), r, None));
        assert!(path_matches("*.pem", &root.join("k/server.pem"), r, None));
        let abs = if cfg!(windows) { "//C:/ws/proj/**" } else { "//ws/proj/**" };
        assert!(path_matches(abs, &root.join("x"), r, None));
        assert!(command_matches("git push*", "git push origin"));
        assert!(command_matches("npm run *", "npm run build"));
        assert!(!command_matches("npm run *", "npm test"));
        assert!(command_matches("GIT status", "git status"));
    }
}
