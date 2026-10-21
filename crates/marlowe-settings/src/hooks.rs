//! Configured hooks (design 2.6 X1-X9): every Claude Code event, regex matchers (with Claude
//! Code's tool names accepted for Marlowe's: `Bash` matches `bash`, `Edit|Write` matches `edit`
//! and `write`, `mcp__.*` matches MCP tools), and project trust by digest: a project's hooks run
//! only after the user trusts the exact configuration they see (`<home>\trust.json`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use marlowe_proto::HookInfo;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::layers::Source;

/// Every hook event, by its Claude Code name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    PermissionRequest,
    UserPromptSubmit,
    Notification,
    Stop,
    SubagentStart,
    SubagentStop,
    PreCompact,
    PostCompact,
    SessionStart,
    SessionEnd,
}

impl HookEvent {
    pub const ALL: [HookEvent; 13] = [
        HookEvent::PreToolUse,
        HookEvent::PostToolUse,
        HookEvent::PostToolUseFailure,
        HookEvent::PermissionRequest,
        HookEvent::UserPromptSubmit,
        HookEvent::Notification,
        HookEvent::Stop,
        HookEvent::SubagentStart,
        HookEvent::SubagentStop,
        HookEvent::PreCompact,
        HookEvent::PostCompact,
        HookEvent::SessionStart,
        HookEvent::SessionEnd,
    ];

    pub fn name(self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::PostToolUseFailure => "PostToolUseFailure",
            HookEvent::PermissionRequest => "PermissionRequest",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::Notification => "Notification",
            HookEvent::Stop => "Stop",
            HookEvent::SubagentStart => "SubagentStart",
            HookEvent::SubagentStop => "SubagentStop",
            HookEvent::PreCompact => "PreCompact",
            HookEvent::PostCompact => "PostCompact",
            HookEvent::SessionStart => "SessionStart",
            HookEvent::SessionEnd => "SessionEnd",
        }
    }

    pub fn parse(name: &str) -> Option<HookEvent> {
        HookEvent::ALL.into_iter().find(|e| e.name() == name)
    }
}

/// What a hook runs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum HookKind {
    /// A shell command; the event's input arrives as JSON on stdin.
    Command(String),
    /// A prompt an LLM evaluates (`$ARGUMENTS` is replaced by the input JSON).
    Prompt(String),
}

/// One configured hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookEntry {
    pub event: HookEvent,
    pub matcher: Option<String>,
    pub kind: HookKind,
    /// Seconds.
    pub timeout: u64,
    pub source: Source,
    pub path: Option<PathBuf>,
}

/// Default hook timeout (seconds), as Claude Code.
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;

impl HookEntry {
    /// Whether `target` (a tool name, notification type, compaction trigger or start source)
    /// selects this hook.
    pub fn matches(&self, target: &str) -> bool {
        matcher_matches(self.matcher.as_deref(), target)
    }

    pub fn info(&self, trusted: bool) -> HookInfo {
        HookInfo {
            event: self.event.name().to_string(),
            matcher: self.matcher.clone(),
            command: match &self.kind {
                HookKind::Command(c) => c.clone(),
                HookKind::Prompt(p) => format!("prompt: {p}"),
            },
            scope: self.source.scope(),
            trusted: trusted || !self.source.project_scoped(),
        }
    }
}

/// Claude Code's name for a Marlowe tool, so a `.claude` matcher written for Claude Code works.
pub fn claude_tool_name(tool: &str) -> Option<&'static str> {
    Some(match tool {
        "bash" => "Bash",
        "powershell" => "PowerShell",
        "read" => "Read",
        "write" => "Write",
        "edit" => "Edit",
        "multi_edit" => "MultiEdit",
        "notebook_edit" => "NotebookEdit",
        "glob" => "Glob",
        "grep" => "Grep",
        "web_fetch" => "WebFetch",
        "web_search" => "WebSearch",
        "todo" => "TodoWrite",
        "agent" => "Task",
        "ask_user" => "AskUserQuestion",
        "exit_plan" => "ExitPlanMode",
        "enter_plan" => "EnterPlanMode",
        "send_message" => "SendMessage",
        "task_output" => "BashOutput",
        "task_stop" => "KillShell",
        "skill" => "Skill",
        _ => return None,
    })
}

/// A matcher against a target: none, empty or `*` match everything; otherwise a regex that must
/// match the whole target, tried against the Marlowe name and the Claude Code name, ignoring case.
pub fn matcher_matches(matcher: Option<&str>, target: &str) -> bool {
    let Some(m) = matcher.map(str::trim).filter(|m| !m.is_empty() && *m != "*") else { return true };
    let re = match Regex::new(&format!("(?i)^(?:{m})$")) {
        Ok(re) => re,
        // An invalid regex matches its literal text only.
        Err(_) => return m.eq_ignore_ascii_case(target),
    };
    // `team` was `workflow`: a matcher written for the old name still selects it.
    let names = [Some(target), claude_tool_name(target), (target == "team").then_some("workflow")];
    if names.iter().flatten().any(|n| re.is_match(n)) {
        return true;
    }
    // `Read*` written as a glob (no other regex syntax): `*` is any run of characters.
    if m.contains('*') && !m.contains(['.', '(', ')', '[', ']', '+', '?', '\\', '^', '$', '{', '}']) {
        let glob = m.split('|').map(|alt| alt.split('*').map(regex::escape).collect::<Vec<_>>().join(".*")).collect::<Vec<_>>().join("|");
        if let Ok(g) = Regex::new(&format!("(?i)^(?:{glob})$")) {
            return names.iter().flatten().any(|n| g.is_match(n));
        }
    }
    false
}

/// Which workspaces' hooks the user trusted, and at what digest; and the allow rules the user
/// granted through Marlowe ("always for this project", `/permissions add`) per workspace. A
/// granted rule found in a project file applies without trust and stays out of the digest: the
/// ledger lives in the user's home, so a repository cannot forge it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustStore {
    #[serde(default)]
    pub projects: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub granted: BTreeMap<String, Vec<String>>,
}

impl TrustStore {
    pub fn path(home: &Path) -> PathBuf {
        home.join("trust.json")
    }

    pub fn load(home: &Path) -> TrustStore {
        std::fs::read_to_string(Self::path(home)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    pub fn is_trusted(&self, workspace: &Path, digest: &str) -> bool {
        self.projects.get(&key(workspace)).is_some_and(|d| d == digest)
    }

    /// Records `digest` for `workspace` and saves (write to a temp file, then rename).
    pub fn trust(home: &Path, workspace: &Path, digest: &str) -> std::io::Result<()> {
        let mut store = Self::load(home);
        store.projects.insert(key(workspace), digest.to_string());
        store.save(home)
    }

    /// The allow rules the user granted for `workspace`.
    pub fn granted_for(&self, workspace: &Path) -> Vec<String> {
        self.granted.get(&key(workspace)).cloned().unwrap_or_default()
    }

    /// Records `rules` as the user's own grants for `workspace`.
    pub fn grant(home: &Path, workspace: &Path, rules: &[String]) -> std::io::Result<()> {
        let mut store = Self::load(home);
        let list = store.granted.entry(key(workspace)).or_default();
        for r in rules {
            let r = r.trim().to_string();
            if !list.contains(&r) {
                list.push(r);
            }
        }
        store.save(home)
    }

    /// Forgets a grant (the rule was removed from the project's file).
    pub fn revoke(home: &Path, workspace: &Path, rule: &str) -> std::io::Result<()> {
        let mut store = Self::load(home);
        let k = key(workspace);
        let Some(list) = store.granted.get_mut(&k) else { return Ok(()) };
        list.retain(|r| r != rule.trim());
        if list.is_empty() {
            store.granted.remove(&k);
        }
        store.save(home)
    }

    fn save(&self, home: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(home)?;
        let tmp = home.join("trust.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).unwrap_or_default())?;
        std::fs::rename(tmp, Self::path(home))
    }
}

/// A workspace path as a stable key (case-folded on Windows, forward slashes).
pub fn key(workspace: &Path) -> String {
    let s = workspace.to_string_lossy().replace('\\', "/").trim_end_matches('/').to_string();
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

/// The digest of what a project asks to be trusted with: its hooks and its widening settings
/// (allow rules, extra directories, project commands, default mode), in canonical JSON.
pub fn digest(hooks: &[HookEntry], allows: &[String], dirs: &[String], commands: &[String], mode: Option<&str>) -> String {
    let hooks: Vec<serde_json::Value> = hooks
        .iter()
        .map(|h| {
            serde_json::json!({
                "event": h.event.name(),
                "matcher": h.matcher,
                "command": match &h.kind { HookKind::Command(c) => c, HookKind::Prompt(p) => p },
                "prompt": matches!(h.kind, HookKind::Prompt(_)),
                "timeout": h.timeout,
            })
        })
        .collect();
    let doc = serde_json::json!({ "hooks": hooks, "allow": allows, "dirs": dirs, "commands": commands, "mode": mode });
    let bytes = serde_json::to_vec(&doc).unwrap_or_default();
    blake3::hash(&bytes).to_hex()[..32].to_string()
}
