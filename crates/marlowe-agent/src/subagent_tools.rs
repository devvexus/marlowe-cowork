//! Tools only a subagent sees (A16, A17), supplied by the subagent's tool view
//! ([`crate::subagent::FilteredTools`]) rather than the tool table, so the conductor's tool list
//! and prompt never carry them:
//!
//! - `handback{message}` (every subagent): the message is the agent's result, returned to the
//!   caller verbatim, and the agent ends. A model that never calls it (local models often just
//!   answer) returns its final text instead; both are counted, neither is repaired.
//! - `set_statusline{command, scope}` (the `statusline` helper): writes `statusline.command` into
//!   the chosen settings layer. A fixed approval every time: the command runs on every update.
//! - `create_agent{name, description, prompt, ...}` (the `agent-creator` helper): saves a drafted
//!   agent file in the project or user agents folder after the user has seen the whole draft; it
//!   must parse as an agent (A1) and never overwrites an existing file.
//!
//! Also here: the documentation roots and the path check that keep the `guide` helper to them.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use marlowe_model::ToolSpec;
use marlowe_proto::SettingsScope;
use serde_json::{json, Value};

use crate::tools::{CallCtx, Consequence, Effects, ToolCallReq, ToolOutcome};

/// The tool name a subagent hands its report back with.
pub const HANDBACK: &str = "handback";

/// Left in a subagent's extensions by `handback`; the runner reads it when the turn ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handback(pub String);

/// One tool a subagent view supplies.
pub trait SubagentTool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    fn effects(&self) -> Effects;
    /// Whether the call asks; a fixed ask is never lifted by a rule or mode.
    fn consequence(&self, _args: &Value) -> Consequence {
        Consequence::Free
    }
    fn call(&self, args: &Value, cx: &CallCtx) -> ToolOutcome;
}

fn tool_spec(name: &str, description: &str, properties: Value, required: &[&str]) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.into(),
        input_schema: json!({"type": "object", "properties": properties, "required": required, "additionalProperties": false}),
    }
}

// ---------------------------------------------------------------- handback

pub struct HandbackTool;

impl SubagentTool for HandbackTool {
    fn spec(&self) -> ToolSpec {
        tool_spec(
            HANDBACK,
            "Hand your final report back to the agent that started you, and finish. Call it once, when the task is done: \
             the message is returned exactly as you write it, and anything you write after it is dropped.",
            json!({"message": {"type": "string", "description": "The complete report: what you found or did, with file paths as evidence."}}),
            &["message"],
        )
    }

    fn effects(&self) -> Effects {
        Effects::NONE
    }

    fn call(&self, args: &Value, cx: &CallCtx) -> ToolOutcome {
        let Some(message) = args.get("message").or_else(|| args.get("report")).and_then(Value::as_str).map(str::trim).filter(|m| !m.is_empty()) else {
            return ToolOutcome::error("`message` is required: the report to hand back");
        };
        cx.scope.with_ext(|e| {
            e.insert(Handback(message.to_string()));
        });
        // The run ends after this batch: its own token (never the caller's) stops the loop.
        cx.cancel.cancel();
        ToolOutcome::ok("Report handed back. You are done.", "handed back")
    }
}

// ---------------------------------------------------------------- set_statusline

pub struct SetStatusline {
    pub home: Option<PathBuf>,
    pub workspace: Option<PathBuf>,
}

fn scope_of(args: &Value, default: SettingsScope) -> Result<SettingsScope, String> {
    match args.get("scope").and_then(Value::as_str).map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        None | Some("") => Ok(default),
        Some("user") => Ok(SettingsScope::User),
        Some("project") => Ok(SettingsScope::Project),
        Some("local") => Ok(SettingsScope::Local),
        Some(other) => Err(format!("scope `{other}` is not user, project or local")),
    }
}

impl SubagentTool for SetStatusline {
    fn spec(&self) -> ToolSpec {
        tool_spec(
            "set_statusline",
            "Set the command that prints the status line, in a settings layer. The user approves the exact command first.",
            json!({
                "command": {"type": "string", "description": "A shell command that reads the session JSON on stdin and prints one line."},
                "scope": {"type": "string", "enum": ["user", "project", "local"], "description": "user (default): all projects; project: shared with the repository; local: this project only."}
            }),
            &["command"],
        )
    }

    fn effects(&self) -> Effects {
        Effects::WRITE
    }

    fn consequence(&self, args: &Value) -> Consequence {
        let command = args.get("command").and_then(Value::as_str).unwrap_or("").lines().next().unwrap_or("").to_string();
        let scope = args.get("scope").and_then(Value::as_str).unwrap_or("user");
        Consequence::AskEvery { summary: format!("set the status line ({scope} settings) to run: {command}") }
    }

    fn call(&self, args: &Value, _cx: &CallCtx) -> ToolOutcome {
        let Some(command) = args.get("command").and_then(Value::as_str).map(str::trim).filter(|c| !c.is_empty()) else {
            return ToolOutcome::error("`command` is required");
        };
        let scope = match scope_of(args, SettingsScope::User) {
            Ok(s) => s,
            Err(e) => return ToolOutcome::error(e),
        };
        let Some(home) = self.home.as_deref() else { return ToolOutcome::error("no profile home to write settings in") };
        let paths = marlowe_settings::layers::Paths::new(home, self.workspace.as_deref());
        let Some(path) = paths.for_scope(scope) else {
            return ToolOutcome::error("that layer needs a workspace (project and local settings live in the project)");
        };
        let was_trusted = self.workspace.as_deref().is_some_and(|w| marlowe_settings::Settings::load(home, Some(w), None).trusted);
        if let Err(e) = marlowe_settings::edit::set_key(&path, "statusline.command", &Value::String(command.to_string())) {
            return ToolOutcome::error(format!("could not write {}: {e}", path.display()));
        }
        // The user approved this exact change: a trusted project stays trusted.
        if let (Some(ws), true) = (self.workspace.as_deref(), scope != SettingsScope::User) {
            let _ = marlowe_settings::Settings::retrust_after_edit(home, ws, was_trusted);
        }
        ToolOutcome::ok(format!("Status line set in {}: {command}", path.display()), "statusline set")
    }
}

// ---------------------------------------------------------------- create_agent

pub struct CreateAgent {
    pub home: Option<PathBuf>,
    pub workspace: Option<PathBuf>,
}

/// An agent name: lower-case words joined by hyphens.
pub fn valid_agent_name(n: &str) -> bool {
    let b = n.as_bytes();
    !b.is_empty()
        && b.len() <= 48
        && b[0].is_ascii_lowercase()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && !n.ends_with('-')
}

fn yaml_scalar(s: &str) -> String {
    // One line, quoted: a description may hold colons and quotes.
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace(['\r', '\n'], " "))
}

/// The agent file text for `create_agent{...}`.
pub fn draft_agent(args: &Value) -> Result<(String, String), String> {
    let s = |k: &str| args.get(k).and_then(Value::as_str).map(str::trim).filter(|x| !x.is_empty());
    let name = s("name").ok_or("`name` is required")?;
    if !valid_agent_name(name) {
        return Err(format!("name `{name}` must be lower-case words joined by hyphens (a-z, 0-9, -), at most 48 characters"));
    }
    let description = s("description").ok_or("`description` is required: when the main agent should use this agent")?;
    let prompt = s("prompt").ok_or("`prompt` is required: the agent's system prompt")?;
    let mut text = format!("---\nname: {name}\ndescription: {}\n", yaml_scalar(description));
    if let Some(tools) = args.get("tools").and_then(Value::as_array) {
        let list: Vec<&str> = tools.iter().filter_map(Value::as_str).map(str::trim).filter(|t| !t.is_empty()).collect();
        if !list.is_empty() {
            text.push_str(&format!("tools: [{}]\n", list.join(", ")));
        }
    }
    if let Some(m) = s("model") {
        text.push_str(&format!("model: {m}\n"));
    }
    if let Some(e) = s("effort") {
        text.push_str(&format!("effort: {e}\n"));
    }
    text.push_str(&format!("---\n{prompt}\n"));
    // It must load as an agent, or it is not saved.
    crate::subagent::parse_agent(&text, name, "draft", None)?;
    Ok((name.to_string(), text))
}

impl SubagentTool for CreateAgent {
    fn spec(&self) -> ToolSpec {
        tool_spec(
            "create_agent",
            "Save a drafted subagent as an agent file. The user reviews the whole draft first. Refuses to overwrite an existing agent.",
            json!({
                "name": {"type": "string", "description": "Lower-case words joined by hyphens."},
                "description": {"type": "string", "description": "When the main agent should use it."},
                "prompt": {"type": "string", "description": "The agent's system prompt."},
                "tools": {"type": "array", "items": {"type": "string"}, "description": "Tools it may use; leave out to inherit all."},
                "model": {"type": "string", "description": "sonnet, opus, haiku, inherit or a model id."},
                "effort": {"type": "string", "enum": ["low", "medium", "high", "xhigh", "max"]},
                "scope": {"type": "string", "enum": ["project", "user"], "description": "project (default): .marlowe/agents; user: the profile's agents folder."}
            }),
            &["name", "description", "prompt"],
        )
    }

    fn effects(&self) -> Effects {
        Effects::WRITE
    }

    fn consequence(&self, args: &Value) -> Consequence {
        let name = args.get("name").and_then(Value::as_str).unwrap_or("?");
        let scope = args.get("scope").and_then(Value::as_str).unwrap_or("project");
        Consequence::Ask { summary: format!("save the agent `{name}` ({scope} agents); the draft is in the details") }
    }

    fn call(&self, args: &Value, _cx: &CallCtx) -> ToolOutcome {
        let (name, text) = match draft_agent(args) {
            Ok(d) => d,
            Err(e) => return ToolOutcome::error(e),
        };
        let user = match args.get("scope").and_then(Value::as_str).map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            None | Some("") | Some("project") => false,
            Some("user") => true,
            Some(other) => return ToolOutcome::error(format!("scope `{other}` is not project or user")),
        };
        let dir = if user {
            self.home.as_ref().map(|h| h.join("agents"))
        } else {
            self.workspace.as_ref().map(|w| w.join(".marlowe").join("agents"))
        };
        let Some(dir) = dir else {
            return ToolOutcome::error(if user { "no profile home to save an agent in" } else { "a project agent needs a workspace" });
        };
        let path = dir.join(format!("{name}.md"));
        if path.exists() {
            return ToolOutcome::error(format!("{} already exists; choose another name or edit that file", path.display()));
        }
        if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, &text)) {
            return ToolOutcome::error(format!("could not save {}: {e}", path.display()));
        }
        ToolOutcome::ok(format!("Saved the agent `{name}` to {}.", path.display()), format!("saved {name}"))
    }
}

/// The tool views the helper types are given, by tool name.
pub fn extras_for(home: Option<&Path>, workspace: Option<&Path>) -> Vec<Arc<dyn SubagentTool>> {
    let (home, workspace) = (home.map(Path::to_path_buf), workspace.map(Path::to_path_buf));
    vec![
        Arc::new(HandbackTool),
        Arc::new(SetStatusline { home: home.clone(), workspace: workspace.clone() }),
        Arc::new(CreateAgent { home, workspace }),
    ]
}

// ---------------------------------------------------------------- the guide's documentation roots

/// The places Marlowe's documentation lives, for the `guide` helper: the profile's `docs`, a
/// `docs` folder next to the program (an install, or a build in the repository), and the
/// changelog and readme beside it. Each is a directory or a file; only existing ones are kept.
pub fn docs_roots(home: Option<&Path>) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(h) = home {
        candidates.extend([h.join("docs"), h.join("CHANGELOG.md")]);
    }
    if let Ok(exe) = std::env::current_exe() {
        for dir in exe.ancestors().skip(1).take(4) {
            candidates.extend([dir.join("docs"), dir.join("CHANGELOG.md"), dir.join("README.md")]);
        }
    }
    // A build in the repository (also what the tests see).
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
    candidates.extend([repo.join("docs"), repo.join("CHANGELOG.md"), repo.join("README.md")]);
    let mut out: Vec<PathBuf> = Vec::new();
    for c in candidates {
        if let Ok(real) = std::fs::canonicalize(&c) {
            if !out.contains(&real) {
                out.push(real);
            }
        }
    }
    out
}

/// Keeps a tool's paths under `roots`.
#[derive(Debug, Clone)]
pub struct PathGuard {
    pub roots: Vec<PathBuf>,
}

impl PathGuard {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }

    fn inside(&self, p: &Path) -> bool {
        let Ok(real) = std::fs::canonicalize(p) else { return false };
        self.roots.iter().any(|r| real.starts_with(r))
    }

    /// `Ok` when every path the call names (`path_args`; `path` when the manifest names none) is
    /// under a root, and its glob arguments cannot climb out of one. A search with no path would
    /// default to the workspace, so a path is required.
    pub fn check(&self, call: &ToolCallReq, path_args: &[&str]) -> Result<(), String> {
        let keys: &[&str] = if path_args.is_empty() { &["path"] } else { path_args };
        let list = || self.roots.iter().map(|r| r.display().to_string()).collect::<Vec<_>>().join(", ");
        let mut named = 0;
        for k in keys {
            if let Some(p) = call.args.get(*k).and_then(Value::as_str).map(str::trim).filter(|p| !p.is_empty()) {
                named += 1;
                if !self.inside(Path::new(p)) {
                    return Err(format!("`{p}` is outside the documentation; this agent reads only: {}", list()));
                }
            }
        }
        if named == 0 {
            return Err(format!("give `path` explicitly; this agent reads only: {}", list()));
        }
        for k in ["pattern", "glob"] {
            if let Some(g) = call.args.get(k).and_then(Value::as_str) {
                let climbs = g.replace('\\', "/").split('/').any(|seg| seg == "..");
                let absolute = g.starts_with('/') || g.starts_with('\\') || g.as_bytes().get(1) == Some(&b':');
                if k == "glob" || call.name == "glob" {
                    if climbs || absolute {
                        return Err(format!("the {k} `{g}` leaves the documentation; this agent reads only: {}", list()));
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, args: Value) -> ToolCallReq {
        ToolCallReq { id: "c".into(), name: name.into(), args }
    }

    #[test]
    fn agent_names_are_hyphenated_lower_case() {
        for ok in ["reviewer", "api-docs-2"] {
            assert!(valid_agent_name(ok), "{ok}");
        }
        for bad in ["", "Reviewer", "a b", "-a", "a-", "../x", "a_b"] {
            assert!(!valid_agent_name(bad), "{bad}");
        }
    }

    #[test]
    fn a_draft_parses_as_an_agent_and_quotes_awkward_descriptions() {
        let (name, text) = draft_agent(&json!({
            "name": "db-reviewer", "description": "Use for: schema \"reviews\"", "prompt": "You review schemas.",
            "tools": ["read", "grep"], "model": "haiku", "effort": "low"
        }))
        .unwrap();
        assert_eq!(name, "db-reviewer");
        let ty = crate::subagent::parse_agent(&text, "db-reviewer", "t", None).unwrap();
        assert_eq!((ty.name.as_str(), ty.model.as_deref()), ("db-reviewer", Some("haiku")));
        assert_eq!(ty.tools.as_deref(), Some(&["read".to_string(), "grep".to_string()][..]));
        assert_eq!(ty.description, "Use for: schema \"reviews\"");
        assert!(ty.prompt.starts_with("You review schemas"));
        assert!(draft_agent(&json!({"name": "Bad Name", "description": "d", "prompt": "p"})).is_err());
        assert!(draft_agent(&json!({"name": "x", "description": "d"})).is_err());
    }

    #[test]
    fn the_guide_path_guard_admits_the_docs_and_nothing_else() {
        let docs = docs_roots(None);
        assert!(!docs.is_empty(), "the repository's docs are found from a build in it");
        let guard = PathGuard::new(docs.clone());
        let in_docs = docs.iter().find(|d| d.join("ARCHITECTURE.md").is_file()).unwrap().join("ARCHITECTURE.md");
        assert!(guard.check(&call("read", json!({"path": in_docs.display().to_string()})), &["path"]).is_ok());
        let outside = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let e = guard.check(&call("read", json!({"path": outside.display().to_string()})), &["path"]).unwrap_err();
        assert!(e.contains("outside the documentation"), "{e}");
        assert!(guard.check(&call("grep", json!({"pattern": "x"})), &["path"]).unwrap_err().contains("give `path`"));
        let dir = docs.iter().find(|d| d.join("ARCHITECTURE.md").is_file()).unwrap().display().to_string();
        assert!(guard.check(&call("glob", json!({"pattern": "../**/*.rs", "path": dir.clone()})), &["path"]).is_err());
        assert!(guard.check(&call("glob", json!({"pattern": "**/*.md", "path": dir})), &["path"]).is_ok());
        let up = docs.iter().find(|d| d.join("ARCHITECTURE.md").is_file()).unwrap().join("..").join("Cargo.toml");
        assert!(guard.check(&call("read", json!({"path": up.display().to_string()})), &["path"]).is_err(), "a .. climb resolves outside");
    }
}
