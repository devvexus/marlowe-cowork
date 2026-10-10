//! Slash commands, skills and output styles in Claude Code's formats (design 2.4 C4/C13, 2.6
//! X10/X11/X17).
//!
//! - Commands: `*.md` under `<home>\commands`, `~/.claude/commands`, `.claude/commands`,
//!   `.marlowe/commands` and plugins' `commands/` (named `<plugin>:<name>`). Frontmatter:
//!   `description`, `argument-hint`, `allowed-tools`, `model`, `arguments` (named positionals),
//!   `disable-model-invocation`. The body takes `$ARGUMENTS`, `$ARGUMENTS[N]` (0-based), `$1..$N`,
//!   named `$arg`s, `${CLAUDE_PLUGIN_ROOT}`; `` !`cmd` `` and ```` ```! ```` blocks run first, only
//!   when `allowed-tools` grants `Bash(...)` for them; `@file` references attach the file.
//! - Skills: `<dir>/SKILL.md` under the same roots' `skills/`. Also slash commands (unless
//!   `user-invocable: false`), indexed for the model (unless `disable-model-invocation`), loaded
//!   on use through the `skill` tool; `context: fork` runs one in a subagent; `${CLAUDE_SKILL_DIR}`
//!   / `${SKILL_DIR}` expand to its directory.
//! - Output styles: `output-styles/*.md` at the same roots, chosen by `output_style` in
//!   `.marlowe\settings*.toml` / `<home>\settings.toml` or `outputStyle` in Claude Code's
//!   `settings*.json`; the active one is appended last in the stable prefix.
//!
//! [`CommandsMiddleware`] expands a prompt that starts with `/name` before the turn runs; `/init`
//! is built in.

pub mod plugin;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use marlowe_proto::CommandInfo;
use serde_json::Value;

use crate::convo::Conversation;
use crate::instructions::yaml::{self, Yaml};
use crate::instructions::{self, InstructionState, Sources};
use crate::middleware::{ToolMiddleware, TurnFlow};
use crate::tools::{CallCtx, ToolOutcome};

/// What a catalog entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Command,
    Skill,
}

/// A slash command or skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// Invocation name: `commit`, `feature-dev:feature-dev`.
    pub name: String,
    pub kind: Kind,
    pub description: String,
    pub argument_hint: Option<String>,
    /// Claude Code tool patterns (`Bash(git add:*)`, `Read`).
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Named positionals (`arguments: system` -> `$system`).
    pub arguments: Vec<String>,
    pub disable_model_invocation: bool,
    pub user_invocable: bool,
    /// `context: fork`: runs in a subagent.
    pub fork: bool,
    /// The agent type a forked skill runs as.
    pub agent: Option<String>,
    pub body: String,
    pub path: PathBuf,
    /// The skill's directory (or the command file's).
    pub dir: PathBuf,
    pub plugin_root: Option<PathBuf>,
    /// `user`, `project` or `plugin:<name>`.
    pub scope: String,
}

/// An output style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputStyle {
    pub name: String,
    pub description: String,
    pub body: String,
    pub keep_coding_instructions: bool,
    pub source: String,
}

impl OutputStyle {
    /// The block appended last in the stable prefix.
    pub fn render(&self) -> String {
        format!("<output_style name=\"{}\">\n{}\n</output_style>", self.name, self.body.trim())
    }
}

/// Built-in output styles (Claude Code's names).
fn builtin_styles() -> Vec<OutputStyle> {
    let mk = |name: &str, description: &str, body: &str| OutputStyle {
        name: name.into(),
        description: description.into(),
        body: body.into(),
        keep_coding_instructions: true,
        source: "builtin".into(),
    };
    vec![
        mk(
            "Explanatory",
            "Explains implementation choices and codebase patterns while working",
            "While you work, add short \"Insight\" notes that explain why you chose an approach, the codebase patterns \
             you are following and the trade-offs you weighed. Keep them brief and specific to this code.",
        ),
        mk(
            "Learning",
            "Collaborative learn-by-doing: leaves small pieces for the user to write",
            "Work with the user as a teacher would. Explain your reasoning, and for small, meaningful pieces of code \
             (five to ten lines of real decision-making) leave a clearly marked TODO(human) and ask the user to write \
             it, then review what they wrote.",
        ),
    ]
}

/// Commands the harness implements itself (the client turns most into requests).
pub const BUILTINS: &[(&str, &str, Option<&str>)] = &[
    ("init", "Write MARLOWE.md for this repository from its layout, manifests and history", None),
    ("compact", "Condense the conversation now, optionally focused", Some("[focus]")),
    ("clear", "Start a new session (memory is kept)", None),
    ("memory", "Edit the instruction files, then reload them", None),
    ("add-dir", "Add a directory to this session's workspace", Some("<path>")),
    ("model", "Switch the model for this session", Some("[model]")),
    ("loop", "Run a prompt or command on an interval", Some("[interval] <prompt>")),
    ("agents", "List agent types", None),
    ("context", "Show what is in the context window", None),
    ("cost", "Tokens and cost of this session", None),
    ("resume", "Resume an earlier session", None),
    ("rewind", "Restore code and/or conversation to an earlier turn", None),
    ("diff", "Show the changes made this session", Some("[turn]")),
    ("export", "Export this conversation", None),
    ("permissions", "Allow, ask and deny rules", None),
    ("hooks", "Configured hooks", None),
    ("mcp", "MCP servers", None),
    ("config", "Settings", None),
    ("doctor", "Check the installation", None),
    ("review", "Review a pull request or the pending diff", Some("[pr]")),
    ("security-review", "Security review of the pending changes", None),
    ("todos", "Show the todo list", None),
    ("help", "List commands", None),
];

/// Everything discovered for a session.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    /// Commands and skills; a later entry with the same name replaced an earlier one.
    pub entries: Vec<Command>,
    pub plugins: Vec<plugin::Plugin>,
    pub styles: Vec<OutputStyle>,
    pub warnings: Vec<String>,
}

fn front_str(y: &Yaml, keys: &[&str]) -> Option<String> {
    y.get_any(keys).and_then(|v| match v {
        Yaml::Str(s) => Some(s.trim().to_string()),
        Yaml::Seq(_) => Some(v.list().join(", ")),
        _ => None,
    })
}

fn md_files_flat(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    instructions::md_files(dir, &mut out);
    out
}

impl Catalog {
    /// Discovers commands, skills, plugins and output styles for `src`. User-level entries come
    /// first; project entries (`.claude`, then `.marlowe`) override them by name; plugin entries
    /// are namespaced.
    pub fn load(src: &Sources) -> Self {
        let mut me = Catalog::default();
        let (plugins, pw) = plugin::discover(src);
        me.warnings.extend(pw);

        let mut user_dirs: Vec<PathBuf> = Vec::new();
        if let Some(h) = &src.home {
            user_dirs.push(h.clone());
        }
        if let Some(u) = &src.user_home {
            user_dirs.push(u.join(".claude"));
        }
        let project_dirs: Vec<PathBuf> =
            src.roots.iter().flat_map(|r| [r.join(".claude"), r.join(".marlowe")]).collect();

        for (dirs, scope) in [(&user_dirs, "user"), (&project_dirs, "project")] {
            for d in dirs {
                for f in md_files_flat(&d.join("commands")) {
                    me.add_command_file(&f, scope, None, None);
                }
                me.add_skills_in(&d.join("skills"), scope, None, None);
                me.add_styles_in(&d.join("output-styles"), scope);
            }
        }
        for p in &plugins {
            let scope = format!("plugin:{}", p.name);
            for c in &p.command_paths {
                let files = if c.is_dir() { md_files_flat(c) } else { vec![c.clone()] };
                for f in files {
                    me.add_command_file(&f, &scope, Some(&p.name), Some(&p.root));
                }
            }
            for s in &p.skill_paths {
                me.add_skills_in(s, &scope, Some(&p.name), Some(&p.root));
            }
            for s in &p.output_style_paths {
                me.add_styles_in(s, &scope);
            }
        }
        me.plugins = plugins;
        me
    }

    fn push(&mut self, c: Command) {
        if let Some(i) = self.entries.iter().position(|e| e.name == c.name) {
            if self.entries[i].scope == c.scope {
                self.warnings.push(format!("/{} is defined twice in {}; {} wins", c.name, c.scope, c.path.display()));
            }
            self.entries.remove(i);
        }
        self.entries.push(c);
    }

    fn add_command_file(&mut self, f: &Path, scope: &str, plugin: Option<&str>, plugin_root: Option<&Path>) {
        let Some(text) = instructions::read_file(f, &mut self.warnings) else { return };
        let stem = f.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        match parse_entry(&text, Kind::Command, &stem, f, scope, plugin, plugin_root) {
            Ok(c) => self.push(c),
            Err(e) => self.warnings.push(format!("{}: {e}", f.display())),
        }
    }

    fn add_skills_in(&mut self, dir: &Path, scope: &str, plugin: Option<&str>, plugin_root: Option<&Path>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        let mut dirs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        dirs.sort();
        for d in dirs {
            let f = d.join("SKILL.md");
            let Some(text) = instructions::read_file(&f, &mut self.warnings) else { continue };
            let stem = d.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            match parse_entry(&text, Kind::Skill, &stem, &f, scope, plugin, plugin_root) {
                Ok(c) => self.push(c),
                Err(e) => self.warnings.push(format!("{}: {e}", f.display())),
            }
        }
    }

    fn add_styles_in(&mut self, dir: &Path, scope: &str) {
        for f in md_files_flat(dir) {
            let Some(text) = instructions::read_file(&f, &mut self.warnings) else { continue };
            let (front, body) = yaml::parse_document(&text).unwrap_or((Yaml::Null, text.as_str()));
            let stem = f.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let name = front_str(&front, &["name"]).unwrap_or(stem);
            let style = OutputStyle {
                description: front_str(&front, &["description"]).unwrap_or_default(),
                keep_coding_instructions: front.get_any(&["keep-coding-instructions", "keepCodingInstructions"]).and_then(Yaml::as_bool).unwrap_or(false),
                body: body.trim().to_string(),
                source: format!("{scope} {}", f.display()),
                name,
            };
            self.styles.retain(|s| !s.name.eq_ignore_ascii_case(&style.name));
            self.styles.push(style);
        }
    }

    /// The entry invoked as `/name`: an exact name, else the one plugin entry whose part after
    /// `:` is `name`.
    pub fn find(&self, name: &str) -> Option<&Command> {
        if let Some(c) = self.entries.iter().find(|c| c.name == name) {
            return Some(c);
        }
        let mut hits = self.entries.iter().filter(|c| c.name.rsplit_once(':').is_some_and(|(_, n)| n == name));
        let first = hits.next()?;
        hits.next().is_none().then_some(first)
    }

    /// Skills the model may invoke.
    pub fn model_skills(&self) -> impl Iterator<Item = &Command> {
        self.entries.iter().filter(|c| c.kind == Kind::Skill && !c.disable_model_invocation)
    }

    /// The skills index for the prefix (empty when there are none).
    pub fn skills_index(&self) -> String {
        let lines: Vec<String> = self
            .model_skills()
            .map(|c| format!("- {}: {}", c.name, c.description.lines().next().unwrap_or("").trim()))
            .collect();
        if lines.is_empty() {
            return String::new();
        }
        format!(
            "<skills>\nSkills you can load with the `skill` tool when a task matches one (load it before you start that \
             work):\n{}\n</skills>",
            lines.join("\n")
        )
    }

    /// Every command the user can type: built-ins, then commands and user-invocable skills.
    pub fn command_infos(&self) -> Vec<CommandInfo> {
        let mut out: Vec<CommandInfo> = BUILTINS
            .iter()
            .map(|(n, d, h)| CommandInfo { name: n.to_string(), description: d.to_string(), argument_hint: h.map(str::to_string), source: "builtin".into() })
            .collect();
        for c in self.entries.iter().filter(|c| c.user_invocable) {
            out.retain(|b| b.name != c.name);
            out.push(CommandInfo {
                name: c.name.clone(),
                description: c.description.clone(),
                argument_hint: c.argument_hint.clone(),
                source: c.path.display().to_string(),
            });
        }
        out
    }
}

#[allow(clippy::too_many_arguments)]
fn parse_entry(
    text: &str,
    kind: Kind,
    stem: &str,
    path: &Path,
    scope: &str,
    plugin: Option<&str>,
    plugin_root: Option<&Path>,
) -> Result<Command, String> {
    let (front, body) = yaml::parse_document(text)?;
    let base = match kind {
        Kind::Skill => front_str(&front, &["name"]).filter(|n| !n.is_empty()).unwrap_or_else(|| stem.to_string()),
        Kind::Command => stem.to_string(),
    };
    let name = match plugin {
        Some(p) => format!("{p}:{base}"),
        None => base,
    };
    let mut description = front_str(&front, &["description"]).unwrap_or_default();
    if description.is_empty() {
        // Claude Code falls back to the first line of the body.
        description = body.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").trim_start_matches('#').trim().to_string();
    }
    let bool_of = |keys: &[&str]| front.get_any(keys).and_then(Yaml::as_bool);
    Ok(Command {
        name,
        kind,
        description,
        argument_hint: front_str(&front, &["argument-hint", "argumentHint"]),
        allowed_tools: front.get_any(&["allowed-tools", "allowedTools", "tools"]).map(Yaml::list).unwrap_or_default(),
        disallowed_tools: front.get_any(&["disallowed-tools", "disallowedTools"]).map(Yaml::list).unwrap_or_default(),
        model: front_str(&front, &["model"]).filter(|m| !m.is_empty()),
        effort: front_str(&front, &["effort"]),
        arguments: front.get("arguments").map(Yaml::list).unwrap_or_default().iter().flat_map(|a| a.split_whitespace().map(str::to_string).collect::<Vec<_>>()).collect(),
        disable_model_invocation: bool_of(&["disable-model-invocation", "disableModelInvocation"]).unwrap_or(false)
            || bool_of(&["hide-from-slash-command-tool"]).unwrap_or(false),
        user_invocable: bool_of(&["user-invocable", "userInvocable"]).unwrap_or(true),
        fork: front_str(&front, &["context"]).is_some_and(|c| c == "fork"),
        agent: front_str(&front, &["agent"]),
        body: body.trim_start_matches(['\r', '\n']).to_string(),
        path: path.to_path_buf(),
        dir: path.parent().map(Path::to_path_buf).unwrap_or_default(),
        plugin_root: plugin_root.map(Path::to_path_buf),
        scope: scope.to_string(),
    })
}

/// The output style chosen in settings for `src`, if any (`default` is none).
pub fn active_output_style(src: &Sources, catalog: &Catalog) -> Option<OutputStyle> {
    let name = chosen_style(src)?;
    if name.eq_ignore_ascii_case("default") {
        return None;
    }
    catalog.styles.iter().rev().chain(builtin_styles().iter()).find(|s| s.name.eq_ignore_ascii_case(&name)).cloned()
}

/// `output_style = "x"` in TOML settings or `"outputStyle": "x"` in Claude Code JSON settings;
/// local over project over user.
fn chosen_style(src: &Sources) -> Option<String> {
    let toml = |p: PathBuf| -> Option<String> {
        let text = std::fs::read_to_string(p).ok()?;
        text.lines().find_map(|l| {
            let (k, v) = l.split_once('=')?;
            (k.trim() == "output_style").then(|| v.trim().trim_matches('"').trim_matches('\'').to_string())
        })
    };
    let json = |p: PathBuf| -> Option<String> {
        let v: Value = serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()?;
        v.get("outputStyle").and_then(Value::as_str).map(str::to_string)
    };
    for r in &src.roots {
        let found = toml(r.join(".marlowe").join("settings.local.toml"))
            .or_else(|| json(r.join(".claude").join("settings.local.json")))
            .or_else(|| toml(r.join(".marlowe").join("settings.toml")))
            .or_else(|| json(r.join(".claude").join("settings.json")));
        if found.is_some() {
            return found;
        }
    }
    src.home.as_ref().and_then(|h| toml(h.join("settings.toml"))).or_else(|| src.user_home.as_ref().and_then(|u| json(u.join(".claude").join("settings.json"))))
}

// ---------------------------------------------------------------- expansion

/// Runs a pre-exec shell command (`` !`cmd` ``) in the workspace; `Err` is shown in its place.
pub type ShellRunner<'a> = &'a dyn Fn(&str, Option<&Path>) -> Result<String, String>;

/// A command or skill made ready to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expanded {
    pub prompt: String,
    pub allowed_tools: Vec<String>,
    pub model: Option<String>,
    pub fork: bool,
    pub agent: Option<String>,
    /// Pre-exec commands that were not run, and why.
    pub warnings: Vec<String>,
}

/// Splits arguments like a shell: whitespace-separated, quotes group.
pub fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut any = false;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                any = true;
            }
            None if c.is_whitespace() => {
                if !cur.is_empty() || any {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            None => cur.push(c),
        }
    }
    if !cur.is_empty() || any {
        out.push(cur);
    }
    out
}

/// Substitutes `$ARGUMENTS[N]`, `$ARGUMENTS`, `$1..$N` and named `$arg`s. Returns the text and
/// whether any placeholder was present.
pub fn substitute_args(body: &str, args: &str, names: &[String]) -> (String, bool) {
    let positional = split_args(args);
    let get = |i: usize| positional.get(i).cloned().unwrap_or_default();
    let mut out = String::with_capacity(body.len());
    let mut used = false;
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < body.len() {
        if bytes[i] == b'$' {
            let rest = &body[i + 1..];
            if let Some(after) = rest.strip_prefix("ARGUMENTS") {
                if let Some(idx_end) = after.strip_prefix('[').and_then(|a| a.find(']').map(|e| (e, &a[..e]))) {
                    if let Ok(n) = idx_end.1.trim().parse::<usize>() {
                        out.push_str(&get(n));
                        used = true;
                        i += 1 + "ARGUMENTS".len() + 1 + idx_end.0 + 1;
                        continue;
                    }
                }
                out.push_str(args.trim());
                used = true;
                i += 1 + "ARGUMENTS".len();
                continue;
            }
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() {
                let n: usize = digits.parse().unwrap_or(0);
                if n >= 1 {
                    out.push_str(&get(n - 1));
                    used = true;
                    i += 1 + digits.len();
                    continue;
                }
            }
            let ident: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
            if let Some(pos) = names.iter().position(|n| *n == ident) {
                out.push_str(&get(pos));
                used = true;
                i += 1 + ident.len();
                continue;
            }
        }
        let ch = body[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    (out, used)
}

/// Read-only commands a pre-exec may run without a grant, as Claude Code allows them: git
/// queries and plain listings, with no redirection.
const READ_ONLY_PREFIXES: &[&str] = &[
    "git status", "git diff", "git log", "git show", "git branch --show-current", "git branch", "git rev-parse",
    "git remote -v", "git ls-files", "git blame", "ls", "pwd", "date", "whoami",
];

fn read_only(part: &str) -> bool {
    let words: Vec<&str> = part.split_whitespace().collect();
    if part.contains(['>', '<', '&']) || words.iter().any(|w| w.starts_with("--output")) {
        return false;
    }
    // `git branch <name>` creates a branch: only listing flags pass.
    if words.starts_with(&["git", "branch"]) {
        return words[2..].iter().all(|w| ["--show-current", "-a", "-v", "-vv", "--list", "-r"].contains(w));
    }
    READ_ONLY_PREFIXES.iter().any(|p| part == *p || part.starts_with(&format!("{p} ")))
}

/// Whether `allowed` (Claude Code patterns) lets a pre-exec run `cmd`: `Bash` allows anything;
/// `Bash(git add:*)` a prefix; `Bash(ls *)` a glob; `Bash(git status)` exactly; read-only git
/// queries and listings need no grant. Every part of a compound command must be allowed;
/// command substitution needs a bare `Bash`.
pub fn shell_allowed(cmd: &str, allowed: &[String]) -> bool {
    let specs: Vec<&str> = allowed
        .iter()
        .filter_map(|a| {
            let a = a.trim();
            if a == "Bash" || a == "bash" {
                Some("*")
            } else {
                a.strip_prefix("Bash(").or_else(|| a.strip_prefix("bash(")).and_then(|r| r.strip_suffix(')'))
            }
        })
        .collect();
    if specs.contains(&"*") {
        return true;
    }
    if cmd.contains("$(") || cmd.contains('`') {
        return false;
    }
    let mut parts: Vec<&str> = vec![cmd];
    for sep in ["&&", "||", ";", "|", "
"] {
        parts = parts.into_iter().flat_map(|p| p.split(sep)).collect();
    }
    parts.iter().map(|p| p.trim()).filter(|p| !p.is_empty()).all(|part| {
        read_only(part)
            || specs.iter().any(|spec| {
                let spec = spec.trim();
                if let Some(prefix) = spec.strip_suffix(":*") {
                    part == prefix || part.starts_with(&format!("{prefix} "))
                } else if spec.contains('*') {
                    wildcard(spec, part)
                } else {
                    part == spec
                }
            })
    })
}

/// `*` matches any run of characters (including none).
fn wildcard(pat: &str, s: &str) -> bool {
    let parts: Vec<&str> = pat.split('*').collect();
    let mut rest = s;
    for (i, p) in parts.iter().enumerate() {
        if i == 0 {
            match rest.strip_prefix(p) {
                Some(r) => rest = r,
                None => return false,
            }
        } else if i == parts.len() - 1 {
            return rest.ends_with(p);
        } else {
            match rest.find(p) {
                Some(at) => rest = &rest[at + p.len()..],
                None => return false,
            }
        }
    }
    rest.is_empty() || parts.last().is_some_and(|l| l.is_empty())
}

const PREEXEC_MAX_CHARS: usize = 20_000;
const ATTACH_MAX_CHARS: usize = 50_000;

/// Prepares `cmd` with `args`: arguments, roots, pre-exec output, attached files.
pub fn expand(cmd: &Command, args: &str, workspace: Option<&Path>, run: ShellRunner<'_>) -> Expanded {
    let mut text = cmd.body.clone();
    if let Some(root) = &cmd.plugin_root {
        text = plugin::expand_root(&text, root);
    }
    let dir = cmd.dir.display().to_string();
    text = text.replace("${CLAUDE_SKILL_DIR}", &dir).replace("${SKILL_DIR}", &dir);
    let (mut text, used) = substitute_args(&text, args, &cmd.arguments);
    if !used && !args.trim().is_empty() {
        text.push_str(&format!("\n\nARGUMENTS: {}", args.trim()));
    }
    let allowed: Vec<String> = cmd
        .allowed_tools
        .iter()
        .map(|a| cmd.plugin_root.as_ref().map_or_else(|| a.clone(), |r| plugin::expand_root(a, r)))
        .collect();
    let mut warnings = Vec::new();
    let text = preexec(&text, &allowed, workspace, run, &mut warnings);
    let text = attach_files(&text, workspace);
    Expanded { prompt: text, allowed_tools: allowed, model: cmd.model.clone(), fork: cmd.fork, agent: cmd.agent.clone(), warnings }
}

fn run_one(cmd: &str, allowed: &[String], workspace: Option<&Path>, run: ShellRunner<'_>, warnings: &mut Vec<String>) -> String {
    let cmd = cmd.trim();
    if !shell_allowed(cmd, allowed) {
        warnings.push(format!("`{cmd}` was not run: allowed-tools does not grant it"));
        return format!("[not run: `{cmd}` is not granted by this command's allowed-tools]");
    }
    match run(cmd, workspace) {
        Ok(out) => {
            let out = out.trim_end();
            if out.chars().count() > PREEXEC_MAX_CHARS {
                let cut: String = out.chars().take(PREEXEC_MAX_CHARS).collect();
                format!("{cut}\n[output cut at {PREEXEC_MAX_CHARS} characters]")
            } else {
                out.to_string()
            }
        }
        Err(e) => format!("[`{cmd}` failed: {e}]"),
    }
}

/// Runs `` !`cmd` `` spans and ```` ```! ```` blocks, replacing each with its output.
fn preexec(text: &str, allowed: &[String], workspace: Option<&Path>, run: ShellRunner<'_>, warnings: &mut Vec<String>) -> String {
    let mut out = String::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        if line.trim_start().starts_with("```!") {
            let mut block = Vec::new();
            for l in lines.by_ref() {
                if l.trim_start().starts_with("```") {
                    break;
                }
                block.push(l);
            }
            out.push_str(&run_one(&block.join("\n"), allowed, workspace, run, warnings));
            out.push('\n');
            continue;
        }
        let mut rest = line;
        while let Some(at) = rest.find("!`") {
            let after = &rest[at + 2..];
            let Some(end) = after.find('`') else { break };
            out.push_str(&rest[..at]);
            out.push_str(&run_one(&after[..end], allowed, workspace, run, warnings));
            rest = &after[end + 1..];
        }
        out.push_str(rest);
        out.push('\n');
    }
    if !text.ends_with('\n') {
        out.pop();
    }
    out
}

/// Appends the contents of every `@file` the text names that exists in the workspace.
fn attach_files(text: &str, workspace: Option<&Path>) -> String {
    let Some(ws) = workspace else { return text.to_string() };
    let mut out = text.to_string();
    let mut seen = std::collections::BTreeSet::new();
    for raw in instructions::find_imports(text) {
        let p = if Path::new(&raw).is_absolute() { PathBuf::from(&raw) } else { ws.join(&raw) };
        let p = instructions::normalize(&p);
        if !p.is_file() || !seen.insert(p.clone()) {
            continue;
        }
        if let Ok(body) = std::fs::read_to_string(&p) {
            let body: String = body.chars().take(ATTACH_MAX_CHARS).collect();
            out.push_str(&format!("\n\n<file path=\"{}\">\n{}\n</file>", p.display(), body.trim_end()));
        }
    }
    out
}

/// Git Bash on Windows (`MARLOWE_BASH` overrides), `bash` elsewhere.
pub fn bash_path() -> PathBuf {
    if let Some(p) = std::env::var_os("MARLOWE_BASH") {
        return PathBuf::from(p);
    }
    if cfg!(windows) {
        for c in [r"C:\Program Files\Git\bin\bash.exe", r"C:\Program Files\Git\usr\bin\bash.exe", r"C:\Program Files (x86)\Git\bin\bash.exe"] {
            if Path::new(c).is_file() {
                return PathBuf::from(c);
            }
        }
    }
    PathBuf::from("bash")
}

/// Runs `cmd` under Git Bash in `cwd` through `marlowe_os::child` (scrubbed environment, Job
/// Object), 30 s, output capped.
pub fn run_bash(cmd: &str, cwd: Option<&Path>) -> Result<String, String> {
    let mut c = std::process::Command::new(bash_path());
    c.arg("-c").arg(cmd);
    let mut spec = marlowe_os::child::ChildSpec::new().with_creation_flags(marlowe_os::child::CREATE_NO_WINDOW);
    if let Some(d) = cwd.filter(|d| d.is_dir()) {
        spec = spec.with_cwd(d);
    }
    let limits = marlowe_os::child::RunLimits { timeout: Duration::from_secs(30), max_output_bytes: 256 * 1024 };
    let out = marlowe_os::child::run(c, &spec, limits, &|| false).map_err(|e| e.to_string())?;
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    let err = String::from_utf8_lossy(&out.stderr);
    if !err.trim().is_empty() {
        text.push_str(&err);
    }
    if out.timed_out {
        return Err("timed out after 30 s".into());
    }
    if out.code() != 0 {
        text.push_str(&format!("\n[exit {}]", out.code()));
    }
    Ok(text)
}

// ---------------------------------------------------------------- built-ins

/// `/init`: the prompt that has the model write `MARLOWE.md` from what the repository shows.
pub fn init_prompt(workspace: Option<&Path>, repo_map: &str, run: ShellRunner<'_>) -> String {
    let mut facts: Vec<String> = Vec::new();
    if let Some(ws) = workspace {
        let mut tree: Vec<String> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(ws) {
            let mut names: Vec<(bool, String)> =
                rd.flatten().map(|e| (e.path().is_dir(), e.file_name().to_string_lossy().to_string())).filter(|(_, n)| n != ".git").collect();
            names.sort();
            for (is_dir, n) in names.into_iter().take(80) {
                tree.push(if is_dir { format!("{n}/") } else { n });
            }
        }
        if !tree.is_empty() {
            facts.push(format!("<top_level>\n{}\n</top_level>", tree.join("\n")));
        }
        for m in [
            "Cargo.toml", "package.json", "pyproject.toml", "setup.py", "requirements.txt", "go.mod", "CMakeLists.txt",
            "Makefile", "justfile", "pom.xml", "build.gradle", "tsconfig.json", "README.md", "MARLOWE.md", "CLAUDE.md",
            "AGENTS.md", ".cursorrules", ".github/copilot-instructions.md",
        ] {
            let p = ws.join(m);
            if let Ok(t) = std::fs::read_to_string(&p) {
                let head: Vec<&str> = t.lines().take(60).collect();
                facts.push(format!("<file path=\"{m}\">\n{}\n</file>", head.join("\n")));
            }
        }
        if let Ok(log) = run("git log --oneline -20", Some(ws)) {
            if !log.trim().is_empty() && !log.contains("not a git repository") {
                facts.push(format!("<git_log>\n{}\n</git_log>", log.trim()));
            }
        }
    }
    if !repo_map.trim().is_empty() {
        facts.push(repo_map.trim().to_string());
    }
    format!(
        "Analyse this repository and write a MARLOWE.md at its root for future sessions working in it. If a \
         MARLOWE.md, CLAUDE.md or AGENTS.md exists, improve it instead of starting over, and keep what is still true.\n\n\
         Include:\n1. The commands to build, test (all tests and a single test), lint and format, as this repository \
         runs them.\n2. The high-level architecture: the parts that need several files read to understand, how they \
         connect, and where the entry points are.\n3. Conventions the code follows that a newcomer would get wrong.\n\n\
         Leave out generic advice, anything obvious from a file listing, and made-up sections. Keep it short.\n\n\
         What the repository shows:\n\n{}",
        facts.join("\n\n")
    )
}

/// `/loop [interval] <prompt>`: the interval in seconds (`30s`, `5m`, `2h`, `1d`; default 10
/// minutes) and the prompt.
pub fn parse_loop(args: &str) -> Option<(u64, String)> {
    let args = args.trim();
    let (first, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    let interval = parse_interval(first);
    let (secs, text) = match interval {
        Some(s) => (s, rest.trim()),
        None => (600, args),
    };
    (!text.is_empty() && secs > 0).then(|| (secs, text.to_string()))
}

fn parse_interval(s: &str) -> Option<u64> {
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit())?);
    let n: u64 = num.parse().ok()?;
    let mult = match unit {
        "s" | "sec" | "secs" => 1,
        "m" | "min" | "mins" => 60,
        "h" | "hr" | "hrs" => 3_600,
        "d" => 86_400,
        _ => return None,
    };
    Some(n * mult)
}

// ---------------------------------------------------------------- loop wiring

/// What a slash command granted for the turn it expanded (track E's rules read
/// `allowed_tools`; the loop applies `model`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandGrant {
    pub command: String,
    pub allowed_tools: Vec<String>,
    pub model: Option<String>,
}

/// The session's sources, from its instruction state or the call's workspace.
pub fn sources_of(convo_ext: &crate::middleware::Extensions, workspace: Option<&Path>) -> Sources {
    convo_ext
        .get::<InstructionState>()
        .map(|s| s.sources.clone())
        .unwrap_or_else(|| Sources::for_session(None, workspace))
}

/// Adds a `/loop` schedule for a session: `(session, every_secs, prompt)` -> the schedule's id.
pub type ScheduleFn = Arc<dyn Fn(&marlowe_proto::SessionId, u64, &str) -> Result<String, String> + Send + Sync>;

/// Expands `/name args` prompts before the turn: `/init`, `/loop`, then files and skills.
/// Unknown names pass through unchanged.
pub struct CommandsMiddleware {
    sources: Sources,
    run: Arc<dyn Fn(&str, Option<&Path>) -> Result<String, String> + Send + Sync>,
    schedule: Option<ScheduleFn>,
}

impl CommandsMiddleware {
    /// `run` executes pre-exec commands (the daemon passes [`run_bash`]).
    pub fn new(sources: Sources, run: Arc<dyn Fn(&str, Option<&Path>) -> Result<String, String> + Send + Sync>) -> Arc<Self> {
        Arc::new(Self { sources, run, schedule: None })
    }

    /// With `/loop` (design X19): `schedule` stores the schedule; the prompt also runs now.
    pub fn with_schedule(
        sources: Sources,
        run: Arc<dyn Fn(&str, Option<&Path>) -> Result<String, String> + Send + Sync>,
        schedule: ScheduleFn,
    ) -> Arc<Self> {
        Arc::new(Self { sources, run, schedule: Some(schedule) })
    }
}

/// `5m` / `2h` / `90s` for a schedule's interval.
pub fn describe_interval(secs: u64) -> String {
    match secs {
        s if s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s % 3_600 == 0 => format!("{}h", s / 3_600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// Splits `/name rest` (name: letters, digits, `-`, `_`, `:`, `.`).
pub fn parse_slash(text: &str) -> Option<(&str, &str)> {
    let t = text.trim_start().strip_prefix('/')?;
    let end = t.find(char::is_whitespace).unwrap_or(t.len());
    let name = &t[..end];
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.')) {
        return None;
    }
    Some((name, t[end..].trim()))
}

impl ToolMiddleware for CommandsMiddleware {
    fn name(&self) -> &str {
        "commands"
    }

    fn before_turn(&self, convo: &mut Conversation, text: &mut String) -> TurnFlow {
        let Some((name, args)) = parse_slash(text) else { return TurnFlow::Continue };
        let (name, args) = (name.to_string(), args.to_string());
        let ws = convo.workspace.clone();
        let run = |c: &str, d: Option<&Path>| (self.run)(c, d);
        if name == "loop" {
            let Some(schedule) = &self.schedule else {
                return TurnFlow::Block("/loop needs the daemon's scheduler".into());
            };
            let Some((secs, prompt)) = parse_loop(&args) else {
                return TurnFlow::Block("usage: /loop [interval like 5m, 2h, 30s] <prompt or /command>".into());
            };
            return match schedule(&convo.session, secs, &prompt) {
                Ok(id) => {
                    // The first run is now; the schedule fires it again every interval.
                    *text = prompt;
                    let flow = self.before_turn(convo, text);
                    text.push_str(&format!(
                        "

[harness: scheduled as {id}, every {}; this is the first run]",
                        describe_interval(secs)
                    ));
                    flow
                }
                Err(e) => TurnFlow::Block(format!("/loop was not scheduled: {e}")),
            };
        }
        if name == "init" {
            let map = convo.ext.get::<crate::system::PromptSections>().map(|s| s.repo_map.clone()).unwrap_or_default();
            *text = format!("<command-name>/init</command-name>\n\n{}", init_prompt(ws.as_deref(), &map, &run));
            return TurnFlow::Continue;
        }
        let sources = if convo.ext.get::<InstructionState>().is_some() { sources_of(&convo.ext, ws.as_deref()) } else {
            let mut s = self.sources.clone();
            if s.roots.is_empty() {
                s.roots.extend(ws.clone());
            }
            s
        };
        let catalog = Catalog::load(&sources);
        let Some(cmd) = catalog.find(&name).filter(|c| c.user_invocable) else { return TurnFlow::Continue };
        let ex = expand(cmd, &args, ws.as_deref(), &run);
        let mut out = format!("<command-name>/{}</command-name>\n", cmd.name);
        if !args.is_empty() {
            out.push_str(&format!("<command-args>{args}</command-args>\n"));
        }
        out.push('\n');
        if ex.fork {
            out.push_str(&format!(
                "Run the `{}` skill in a forked subagent: call the `skill` tool with name `{}` and args `{}`, then report its result.",
                cmd.name, cmd.name, args
            ));
        } else {
            out.push_str(&ex.prompt);
        }
        for w in &ex.warnings {
            out.push_str(&format!("\n[harness: {w}]"));
        }
        convo.ext.insert(CommandGrant { command: cmd.name.clone(), allowed_tools: ex.allowed_tools.clone(), model: ex.model.clone() });
        *text = out;
        TurnFlow::Continue
    }
}

/// The `skill` tool: loads a skill's instructions into the conversation (or, for `context:
/// fork`, runs it in a subagent and returns the result).
pub fn skill_call(args: &Value, cx: &CallCtx) -> ToolOutcome {
    let Some(name) = args.get("name").or_else(|| args.get("skill")).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) else {
        return ToolOutcome::error("`name` is required: one of the skills in the skills index");
    };
    let sargs = args.get("args").or_else(|| args.get("arguments")).and_then(Value::as_str).unwrap_or("");
    let sources = cx.scope.with_ext(|e| sources_of(e, cx.workspace.as_deref()));
    let catalog = Catalog::load(&sources);
    let Some(skill) = catalog.find(name).filter(|c| c.kind == Kind::Skill) else {
        let names: Vec<&str> = catalog.model_skills().map(|c| c.name.as_str()).collect();
        return ToolOutcome::error(format!("no skill named `{name}`; available: {}", if names.is_empty() { "none".into() } else { names.join(", ") }));
    };
    if skill.disable_model_invocation {
        return ToolOutcome::error(format!("`{}` can only be run by the user (as /{})", skill.name, skill.name));
    }
    let ex = expand(skill, sargs, cx.workspace.as_deref(), &run_bash);
    if ex.fork {
        let Some(sub) = cx.scope.services.get::<crate::subagent::Subagents>() else {
            return ToolOutcome::error("this skill runs in a subagent, and subagents are not available in this session");
        };
        let req = crate::subagent::AgentRequest {
            agent_type: ex.agent.clone().unwrap_or_else(|| "general".into()),
            description: format!("skill {}", skill.name),
            prompt: ex.prompt,
            isolation: None,
            background: false,
            model: ex.model.clone(),
        };
        return sub.run(req, cx);
    }
    let mut files: Vec<String> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&skill.dir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n != "SKILL.md" {
                files.push(if e.path().is_dir() { format!("{n}/") } else { n });
            }
        }
    }
    files.sort();
    let mut content = format!("<skill name=\"{}\" dir=\"{}\">\n{}\n</skill>", skill.name, skill.dir.display(), ex.prompt.trim());
    if !files.is_empty() {
        content.push_str(&format!("\nSupporting files in {} (read them when the instructions point to them): {}", skill.dir.display(), files.join(", ")));
    }
    for w in &ex.warnings {
        content.push_str(&format!("\n[harness: {w}]"));
    }
    ToolOutcome::ok(format!("Loaded skill `{}`. Follow its instructions:\n\n{content}", skill.name), format!("skill {}", skill.name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_substitute_like_claude_code() {
        let names = vec!["system".to_string()];
        let (t, used) = substitute_args("all=$ARGUMENTS first=$1 second=$2 zero=$ARGUMENTS[0] named=$system $10", "a \"b c\"", &names);
        assert!(used);
        assert_eq!(t, "all=a \"b c\" first=a second=b c zero=a named=a ");
        let (t, used) = substitute_args("plain text, $HOME stays", "", &[]);
        assert_eq!((t.as_str(), used), ("plain text, $HOME stays", false));
    }

    #[test]
    fn allowed_tools_gate_preexec() {
        let allowed = vec!["Bash(git add:*)".to_string(), "Bash(git status:*)".to_string(), "Bash(ls *)".into(), "Read".into()];
        assert!(shell_allowed("git status", &allowed));
        assert!(shell_allowed("git add -A && git status --short", &allowed));
        assert!(shell_allowed("ls -la", &allowed));
        assert!(!shell_allowed("git push", &allowed));
        assert!(!shell_allowed("git status; rm -rf x", &allowed));
        assert!(!shell_allowed("git status $(rm x)", &allowed));
        assert!(shell_allowed("anything at all", &["Bash".to_string()]));
        assert!(shell_allowed("git diff HEAD", &["Read".to_string()]), "read-only git needs no grant");
        assert!(!shell_allowed("git branch -D main", &[]));
        assert!(!shell_allowed("git branch topic", &[]));
        assert!(!shell_allowed("git diff --output=x.patch", &[]));
        assert!(!shell_allowed("ls > out.txt", &[]));
        assert!(!shell_allowed("rm -rf x", &["Read".to_string()]));
    }

    #[test]
    fn loop_intervals() {
        assert_eq!(parse_loop("5m /babysit"), Some((300, "/babysit".into())));
        assert_eq!(parse_loop("check the deploy"), Some((600, "check the deploy".into())));
        assert_eq!(parse_loop("30s ping"), Some((30, "ping".into())));
        assert_eq!(parse_loop("2h"), None);
    }

    #[test]
    fn slash_names_parse() {
        assert_eq!(parse_slash("/commit -m x"), Some(("commit", "-m x")));
        assert_eq!(parse_slash("/feature-dev:feature-dev add a cache"), Some(("feature-dev:feature-dev", "add a cache")));
        assert_eq!(parse_slash("/ not"), None);
        assert_eq!(parse_slash("C:/path/x"), None);
    }
}
