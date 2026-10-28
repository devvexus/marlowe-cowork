//! The settings file schema. Unknown keys are kept (other tracks' settings such as
//! `statusline.command` or `editor_url` live in the same files) and survive every write.
//!
//! ```toml
//! [permissions]
//! default_mode = "accept_edits"
//! allow = ["bash(cargo test:*)", "edit(src/**)"]
//! ask = ["bash(git commit:*)"]
//! deny = ["read(.env)", "bash(curl:*)"]
//! additional_directories = ["../shared"]
//! project_commands = ["bash(just test:*)"]   # freed by accept_edits
//! protected_paths = ["D:/datasets"]          # never written or deleted
//! protected_branches = ["stable"]            # never deleted by a push
//!
//! [network]
//! allow_hosts = ["crates.io", "pypi.org"]
//!
//! [[hooks.PreToolUse]]
//! matcher = "bash|powershell"
//! hooks = [{ type = "command", command = "python .marlowe/hooks/check.py", timeout = 30 }]
//!
//! [teams]
//! default = false         # new sessions start with Teams on
//! size = 10               # specialists a team task stays under
//!
//! [projects.model]
//! employee = "marlowe-mini:4b-super"   # project employees; default: the conversation's model
//! lead = "marlowe-dawn:9b-super"       # the project's lead steps; default: the conversation's model
//!
//! [import]
//! claude = true          # read .claude/settings.json (default)
//! claude_user = false    # also ~/.claude/settings.json
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One settings file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SettingsFile {
    #[serde(default)]
    pub permissions: Permissions,
    #[serde(default)]
    pub network: Network,
    /// Event name (`PreToolUse`, ...) -> matcher groups.
    #[serde(default)]
    pub hooks: BTreeMap<String, Vec<MatcherGroup>>,
    /// Claude Code's `disableAllHooks`.
    #[serde(default)]
    pub disable_all_hooks: bool,
    #[serde(default)]
    pub import: Import,
    #[serde(default)]
    pub teams: Teams,
    #[serde(default)]
    pub projects: Projects,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Permissions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ask: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_directories: Vec<String>,
    /// Rule patterns `accept_edits` frees (the project profile's build/test/lint commands).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub project_commands: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub protected_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub protected_branches: Vec<String>,
}

/// Teams mode: whether new sessions start with it on, and the team-size guideline.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Teams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<bool>,
    /// Specialists a team task should stay under (default 10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u32>,
}

/// Project settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Projects {
    #[serde(default)]
    pub model: ProjectModels,
}

/// Which models a project's steps run on; unset means the conversation's model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectModels {
    /// The model of the employees (`projects.model.employee`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub employee: Option<String>,
    /// The model of the lead steps (`projects.model.lead`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lead: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Network {
    /// Hosts whose first contact does not ask.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_hosts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Import {
    /// Read the project's `.claude/settings.json` and `.claude/settings.local.json`.
    #[serde(default = "yes")]
    pub claude: bool,
    /// Also read `~/.claude/settings.json` (user scope).
    #[serde(default)]
    pub claude_user: bool,
}

impl Default for Import {
    fn default() -> Self {
        Import { claude: true, claude_user: false }
    }
}

fn yes() -> bool {
    true
}

/// Hooks for one event whose tool (or notification type, compaction trigger, start source)
/// matches `matcher` (a regex; empty or `*` matches all).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatcherGroup {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    #[serde(default)]
    pub hooks: Vec<HookCommand>,
}

/// One hook: a shell command, or a prompt an LLM evaluates (`type = "prompt"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookCommand {
    #[serde(rename = "type", default = "command_type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Seconds; default 60.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
}

fn command_type() -> String {
    "command".into()
}

impl SettingsFile {
    /// Parses TOML; unknown keys are ignored here (they stay in the file).
    pub fn from_toml(text: &str) -> Result<SettingsFile, String> {
        toml::from_str(text).map_err(|e| e.message().to_string())
    }

    /// The command-line layer for headless flags (`--allowed-tools`, `--disallowed-tools`,
    /// `--permission-mode`); rule patterns use either spelling (`Bash(git:*)`, `bash(git:*)`).
    pub fn from_headless(o: &marlowe_proto::HeadlessOptions) -> SettingsFile {
        let mut f = SettingsFile::default();
        f.permissions.allow = o.allowed_tools.clone();
        f.permissions.deny = o.disallowed_tools.clone();
        f.permissions.default_mode = o.permission_mode.map(|m| m.as_str().to_string());
        f
    }
}
