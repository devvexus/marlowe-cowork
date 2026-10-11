//! Subagents (design 2.2 A1-A8, A10, A11, A13): the `agent` and `send_message` tools.
//!
//! An agent type is a Claude Code-format markdown file (`.marlowe/agents`, `<home>\agents`,
//! `<home>\roles`, `.claude/agents`, plugins' `agents/` as `<plugin>:<name>`) or a built-in
//! (`explore`, `plan`, `general`). Every Claude Code frontmatter key is read: `tools`,
//! `disallowedTools`, `model` (`sonnet`/`opus`/`haiku`/`inherit` or a model id, resolved to a
//! library slot by the [`ModelResolver`]), `effort`, `permissionMode`, `skills`, `mcpServers`,
//! `hooks`, `maxTurns`, `isolation`, `color`, `initialPrompt`; unknown keys are warnings.
//! Claude Code tool names map onto Marlowe's (`Read` -> `read`, `Task` -> `agent`).
//!
//! A subagent runs in a clean context (its own `Conversation`) on the caller's turn thread, with
//! the caller's tools minus the ones a subagent never gets (depth 1: no `agent`), filtered by its
//! type; read-only types lose every tool that writes or runs. Calls in one step run in parallel,
//! bounded by slots (one local model stream, several cloud ones). `background: true` returns at
//! once and wakes the conductor when the agent finishes ([`SessionWake`]); it has its own local
//! slot (a foreground agent never waits behind it), and the daemon's [`BackgroundLedger`] records
//! it while it runs (a restart tells the session it was lost) and stops it at shutdown.
//! `isolation: worktree` runs it in a git worktree from the [`WorktreeProvider`]. A finished
//! subagent's transcript is kept, so `send_message` continues it with its context. A subagent
//! that read untrusted content taints its caller ([`ReportTainted`]).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use marlowe_model::{Effort, ModelInfo, Provider, ToolSpec};
use marlowe_os::workspace::{WorktreeProvider, WorktreeRequest};
use marlowe_proto::{AgentTypeInfo, SessionId, TurnStop};
use serde_json::Value;

use crate::agent::Agent;
use crate::approval::ApprovalGate;
use crate::convo::Conversation;
use crate::instructions::yaml::{self, Yaml};
use crate::instructions::{self, Sources};
use crate::middleware::ToolMiddleware;
use crate::system::{Environment, PersonaBlock, PromptSections, SystemPrompt};
use crate::tools::{
    CallCtx, Consequence, Exposure, Mutation, SandboxView, ToolCallReq, ToolCtx, ToolHost, ToolManifest, ToolOutcome,
};

/// Tools a subagent never gets: no nesting (depth 1), no questions to the user, no session-level
/// moves, no crew dispatch.
pub const NEVER_FOR_SUBAGENTS: &[&str] = &[
    "agent", "send_message", "ask_user", "enter_worktree", "exit_worktree", "exit_plan", "dispatch",
    // Background work and the user's attention belong to the session that conducts (track N).
    "task_list", "task_output", "task_stop", "monitor", "cron_create", "cron_list", "cron_delete", "schedule_wakeup",
    "list_agents", "watch", "notify", "send_file", "notices",
];

/// Read-only tools by name, for hosts that do not publish manifests.
const READ_ONLY_NAMES: &[&str] = &[
    "read", "glob", "grep", "symbols", "definition", "references", "hover", "diagnostics", "web_fetch", "web_search",
    "arxiv", "paper", "recall", "todo", "job_output",
];

/// Tools a read-only agent never gets, whatever their manifest says: a skill can run its
/// pre-exec shell commands and fork an agent with the full toolset.
const NEVER_READ_ONLY: &[&str] = &["skill"];

/// Model calls a subagent may make in one task unless its type says otherwise.
pub const SUBAGENT_MAX_STEPS: u32 = 200;
/// Concurrent subagents on cloud models.
pub const CLOUD_SLOTS: usize = 4;

/// Frontmatter keys Claude Code defines for agents (plus Marlowe role-card keys).
const KNOWN_KEYS: &[&str] = &[
    "name", "description", "tools", "disallowedTools", "disallowed-tools", "model", "effort", "permissionMode",
    "permission-mode", "skills", "mcpServers", "mcp-servers", "hooks", "maxTurns", "max-turns", "isolation", "color",
    "initialPrompt", "background", "memory", "slot", "budget", "sandbox",
];

/// One agent type.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentType {
    pub name: String,
    pub description: String,
    /// Marlowe tool names it may use; `None` inherits the caller's.
    pub tools: Option<Vec<String>>,
    pub disallowed_tools: Vec<String>,
    /// `sonnet`, `opus`, `haiku`, `inherit` or a model id.
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub permission_mode: Option<String>,
    pub skills: Vec<String>,
    pub mcp_servers: Vec<String>,
    /// The `hooks:` block as JSON, for track E.
    pub hooks: Option<Value>,
    pub max_turns: Option<u32>,
    /// `worktree` runs it in a git worktree.
    pub isolation: Option<String>,
    pub color: Option<String>,
    pub initial_prompt: Option<String>,
    /// Loses every tool that writes or runs.
    pub read_only: bool,
    /// The type's system prompt (the file body).
    pub prompt: String,
    /// `builtin` or the file it came from.
    pub source: String,
    pub warnings: Vec<String>,
}

impl AgentType {
    pub fn info(&self) -> AgentTypeInfo {
        AgentTypeInfo {
            name: self.name.clone(),
            description: self.description.clone(),
            model: self.model.clone(),
            tools: self.tools.clone().unwrap_or_default(),
            source: self.source.clone(),
        }
    }
}

/// Claude Code tool names (and Marlowe's own) to Marlowe tool names.
pub fn map_tool(name: &str) -> Vec<String> {
    let base = name.split('(').next().unwrap_or(name).trim();
    let v: &[&str] = match base {
        "Read" | "NotebookRead" => &["read"],
        "Write" => &["write"],
        "Edit" => &["edit"],
        "MultiEdit" => &["multi_edit"],
        "NotebookEdit" => &["notebook_edit"],
        "Glob" | "LS" => &["glob"],
        "Grep" => &["grep"],
        "Bash" => &["bash"],
        "PowerShell" => &["powershell"],
        "BashOutput" | "TaskOutput" => &["job_output"],
        "KillShell" | "KillBash" | "TaskStop" => &["job_kill"],
        "WebFetch" => &["web_fetch"],
        "WebSearch" => &["web_search"],
        "TodoWrite" | "TaskCreate" | "TaskUpdate" | "TaskList" | "TaskGet" => &["todo"],
        "Task" | "Agent" => &["agent"],
        "AskUserQuestion" => &["ask_user"],
        "Skill" | "SlashCommand" => &["skill"],
        "ExitPlanMode" => &["exit_plan"],
        "EnterWorktree" => &["enter_worktree"],
        "ExitWorktree" => &["exit_worktree"],
        "LSP" => &["symbols", "definition", "references", "hover", "diagnostics"],
        _ => return vec![if base.starts_with("mcp__") { base.to_string() } else { base.to_lowercase() }],
    };
    v.iter().map(|s| s.to_string()).collect()
}

fn map_tools(list: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in list {
        for m in map_tool(t) {
            if !out.contains(&m) {
                out.push(m);
            }
        }
    }
    out
}

fn parse_effort(s: &str) -> Option<Effort> {
    Some(match s.trim().to_ascii_lowercase().as_str() {
        "off" | "none" => Effort::Off,
        "low" | "minimal" => Effort::Low,
        "medium" => Effort::Medium,
        "high" => Effort::High,
        "xhigh" | "max" => Effort::Max,
        _ => return None,
    })
}

/// Parses an agent file in Claude Code format. `namespace` is a plugin name.
pub fn parse_agent(text: &str, stem: &str, source: &str, namespace: Option<&str>) -> Result<AgentType, String> {
    let (front, body) = yaml::parse_document(text)?;
    let mut warnings: Vec<String> = front
        .keys()
        .into_iter()
        .filter(|k| !KNOWN_KEYS.contains(k))
        .map(|k| format!("{source}: unknown frontmatter key `{k}` ignored"))
        .collect();
    let s = |keys: &[&str]| front.get_any(keys).and_then(|v| v.as_str().map(|s| s.trim().to_string())).filter(|s| !s.is_empty());
    let base = s(&["name"]).unwrap_or_else(|| stem.to_string());
    let name = match namespace {
        Some(ns) => format!("{ns}:{base}"),
        None => base,
    };
    let tools = front.get("tools").map(|t| map_tools(&t.list())).filter(|t| !t.is_empty());
    let effort = s(&["effort"]).and_then(|e| {
        let p = parse_effort(&e);
        if p.is_none() {
            warnings.push(format!("{source}: effort `{e}` not understood"));
        }
        p
    });
    let permission_mode = s(&["permissionMode", "permission-mode"]);
    Ok(AgentType {
        description: s(&["description"]).unwrap_or_default(),
        tools,
        disallowed_tools: front.get_any(&["disallowedTools", "disallowed-tools"]).map(|t| map_tools(&t.list())).unwrap_or_default(),
        model: s(&["model"]).filter(|m| m != "inherit"),
        effort,
        read_only: permission_mode.as_deref() == Some("plan"),
        permission_mode,
        skills: front.get("skills").map(Yaml::list).unwrap_or_default(),
        mcp_servers: front.get_any(&["mcpServers", "mcp-servers"]).map(|v| match v {
            Yaml::Map(_) => v.keys().into_iter().map(str::to_string).collect(),
            other => other.list(),
        }).unwrap_or_default(),
        hooks: front.get("hooks").map(Yaml::to_json),
        max_turns: front.get_any(&["maxTurns", "max-turns"]).and_then(Yaml::as_u32),
        isolation: s(&["isolation"]),
        color: s(&["color"]),
        initial_prompt: s(&["initialPrompt"]),
        prompt: body.trim().to_string(),
        source: source.to_string(),
        warnings,
        name,
    })
}

/// The built-in types.
pub fn builtin_types() -> Vec<AgentType> {
    let mk = |name: &str, description: &str, tools: Option<&[&str]>, model: Option<&str>, read_only: bool, prompt: &str| AgentType {
        name: name.into(),
        description: description.into(),
        tools: tools.map(|t| t.iter().map(|s| s.to_string()).collect()),
        disallowed_tools: Vec::new(),
        model: model.map(str::to_string),
        effort: None,
        permission_mode: read_only.then(|| "plan".to_string()),
        skills: Vec::new(),
        mcp_servers: Vec::new(),
        hooks: None,
        max_turns: None,
        isolation: None,
        color: None,
        initial_prompt: None,
        read_only,
        prompt: prompt.into(),
        source: "builtin".into(),
        warnings: Vec::new(),
    };
    vec![
        mk(
            "explore",
            "Fast read-only search of the codebase: finds files, traces how something works, answers where/how questions. \
             Say how thorough to be.",
            Some(&["read", "glob", "grep", "symbols", "definition", "references", "hover", "diagnostics"]),
            Some("haiku"),
            true,
            "You search and read code to answer one question. You never change anything. Search broadly first (glob, \
             grep), then read the files that matter. Answer concisely, with file paths and line numbers for every claim.",
        ),
        mk(
            "plan",
            "Read-only planner: studies the code and returns a step-by-step implementation plan with the files to change.",
            None,
            None,
            true,
            "You plan one change. You never change anything. Read the relevant code, then return: the approach in a few \
             sentences, numbered steps, the files to create or change with what changes in each, the tests to add or \
             run, and the risks.",
        ),
        mk(
            "general",
            "General-purpose agent for multi-step tasks: research, search and changes, with the caller's tools.",
            None,
            None,
            false,
            "You complete one task end to end with the tools you have. Verify what you change. Report what you did and \
             what you found, with file paths.",
        ),
    ]
}

/// Agent type directories for `src`, lowest precedence first: user (`<home>\agents`,
/// `<home>\roles`, `~/.claude/agents`), then each root's `.claude/agents` and `.marlowe/agents`.
fn type_dirs(src: &Sources) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(h) = &src.home {
        v.push(h.join("agents"));
        v.push(h.join("roles"));
    }
    if let Some(u) = &src.user_home {
        v.push(u.join(".claude").join("agents"));
    }
    for r in &src.roots {
        v.push(r.join(".claude").join("agents"));
        v.push(r.join(".marlowe").join("agents"));
    }
    v
}

/// Built-in types that are read-only by construction. A file may replace one (its prompt, its
/// model), but never makes it able to write or run: plan mode and the permission gate rely on it.
pub const READ_ONLY_BUILTINS: &[&str] = &["explore", "plan"];

/// Built-ins, then files (later replaces earlier by name), then plugins' agents (namespaced). A
/// file named after a read-only built-in stays read-only, with a warning when it asked for more.
pub fn discover_types(src: &Sources) -> (Vec<AgentType>, Vec<String>) {
    let mut types = builtin_types();
    let mut warnings = Vec::new();
    let add = |path: &Path, ns: Option<&str>, types: &mut Vec<AgentType>, warnings: &mut Vec<String>| {
        let Some(text) = instructions::read_file(path, warnings) else { return };
        let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        match parse_agent(&text, &stem, &path.display().to_string(), ns) {
            Ok(mut t) => {
                if let Some(b) = READ_ONLY_BUILTINS.iter().find(|b| t.name.eq_ignore_ascii_case(b)) {
                    if !t.read_only {
                        t.warnings.push(format!(
                            "{}: replaces the built-in read-only `{b}` agent, which stays read-only (its write and shell tools are not offered)",
                            path.display()
                        ));
                        t.read_only = true;
                    }
                }
                warnings.extend(t.warnings.iter().cloned());
                types.retain(|x| x.name != t.name && !(t.name == "general" && x.name == "general-purpose"));
                types.push(t);
            }
            Err(e) => warnings.push(format!("{}: {e}", path.display())),
        }
    };
    for d in type_dirs(src) {
        let mut files = Vec::new();
        instructions::md_files(&d, &mut files);
        for f in files {
            add(&f, None, &mut types, &mut warnings);
        }
    }
    let (plugins, pw) = crate::commands::plugin::discover(src);
    warnings.extend(pw);
    for p in &plugins {
        for d in &p.agent_paths {
            let mut files = Vec::new();
            if d.is_dir() {
                instructions::md_files(d, &mut files);
            } else {
                files.push(d.clone());
            }
            for f in files {
                add(&f, Some(&p.name), &mut types, &mut warnings);
            }
        }
    }
    (types, warnings)
}

/// The agent type `name` resolves to in a session with profile `home` and `workspace`, as
/// [`Subagents::types`] resolves it. The permission gate decides on its `read_only`, never on the
/// name (a repository may define its own `explore`).
pub fn resolve_type(name: &str, home: Option<&Path>, workspace: Option<&Path>) -> Option<AgentType> {
    let (types, _) = discover_types(&Sources::for_session(home, workspace));
    find_type(&types, name).cloned()
}

/// Whether running agent type `name` only reads (plan mode may allow it; track E). An unknown
/// type is refused by the tool, so it changes nothing.
pub fn read_only_type(name: &str, home: Option<&Path>, workspace: Option<&Path>) -> bool {
    resolve_type(name, home, workspace).is_none_or(|t| t.read_only)
}

/// Finds `name` among `types`: exact (case-insensitive), `general-purpose` for `general`, or the
/// one namespaced type whose last part matches.
pub fn find_type<'a>(types: &'a [AgentType], name: &str) -> Option<&'a AgentType> {
    let n = name.trim();
    let n = if n.eq_ignore_ascii_case("general-purpose") { "general" } else { n };
    if let Some(t) = types.iter().rev().find(|t| t.name.eq_ignore_ascii_case(n)) {
        return Some(t);
    }
    let mut hits = types.iter().filter(|t| t.name.rsplit_once(':').is_some_and(|(_, b)| b.eq_ignore_ascii_case(n)));
    let first = hits.next()?;
    hits.next().is_none().then_some(first)
}

// ---------------------------------------------------------------- the filtered host

/// The caller's tool host as a subagent of `ty` sees it.
pub struct FilteredTools {
    inner: Arc<dyn ToolHost>,
    allow: Option<HashSet<String>>,
    deny: HashSet<String>,
    read_only: bool,
}

impl FilteredTools {
    pub fn new(inner: Arc<dyn ToolHost>, ty: &AgentType) -> Self {
        Self {
            inner,
            allow: ty.tools.as_ref().map(|t| t.iter().cloned().collect()),
            deny: ty.disallowed_tools.iter().cloned().collect(),
            read_only: ty.read_only,
        }
    }

    /// Whether the subagent may use `name`.
    pub fn admits(&self, name: &str) -> bool {
        if NEVER_FOR_SUBAGENTS.contains(&name) || self.deny.contains(name) {
            return false;
        }
        if let Some(allow) = &self.allow {
            let server_grant = name.starts_with("mcp__") && allow.iter().any(|a| a.starts_with("mcp__") && !a[5..].contains("__") && name.starts_with(&format!("{a}__")));
            if !allow.contains(name) && !server_grant {
                return false;
            }
        }
        if self.read_only {
            if NEVER_READ_ONLY.contains(&name) {
                return false;
            }
            return match self.inner.manifest(name) {
                Some(m) => !m.effects.writes && !m.effects.runs,
                None => READ_ONLY_NAMES.contains(&name),
            };
        }
        true
    }

    fn refuse(name: &str) -> ToolOutcome {
        ToolOutcome::error(format!("`{name}` is not available to this agent"))
    }
}

impl ToolHost for FilteredTools {
    fn specs(&self) -> Vec<ToolSpec> {
        self.inner.specs().into_iter().filter(|s| self.admits(&s.name)).collect()
    }
    fn call(&self, call: &ToolCallReq, ctx: &ToolCtx) -> ToolOutcome {
        if !self.admits(&call.name) {
            return Self::refuse(&call.name);
        }
        self.inner.call(call, ctx)
    }
    fn call_scoped(&self, call: &ToolCallReq, cx: &CallCtx) -> ToolOutcome {
        if !self.admits(&call.name) {
            return Self::refuse(&call.name);
        }
        self.inner.call_scoped(call, cx)
    }
    fn middleware(&self) -> Vec<Arc<dyn ToolMiddleware>> {
        self.inner.middleware()
    }
    fn persona_blocks(&self) -> Vec<PersonaBlock> {
        Vec::new()
    }
    fn consequence(&self, call: &ToolCallReq) -> Consequence {
        self.inner.consequence(call)
    }
    fn consequence_exposed(&self, call: &ToolCallReq, exposure: &Exposure) -> Consequence {
        self.inner.consequence_exposed(call, exposure)
    }
    fn taints(&self, call: &ToolCallReq) -> bool {
        self.inner.taints(call)
    }
    fn egress_host(&self, call: &ToolCallReq) -> Option<String> {
        self.inner.egress_host(call)
    }
    fn manifest(&self, name: &str) -> Option<ToolManifest> {
        self.inner.manifest(name)
    }
    fn mutation(&self, call: &ToolCallReq, ctx: &ToolCtx) -> Mutation {
        self.inner.mutation(call, ctx)
    }
    fn consequence_in_sandbox(&self, call: &ToolCallReq, exposure: &Exposure, sandbox: &dyn SandboxView) -> Option<Consequence> {
        self.inner.consequence_in_sandbox(call, exposure, sandbox)
    }
    fn describe(&self, call: &ToolCallReq) -> String {
        self.inner.describe(call)
    }
}

// ---------------------------------------------------------------- services

/// A model ready to call.
#[derive(Clone)]
pub struct ResolvedModel {
    pub provider: Arc<dyn Provider>,
    pub info: ModelInfo,
    pub effort: Effort,
}

impl std::fmt::Debug for ResolvedModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedModel").field("info", &self.info).field("effort", &self.effort).finish()
    }
}

/// Resolves an agent's `model` (an alias, a model id, or `None`/`inherit` for the caller's) to
/// a library slot (design A10: `sonnet/opus/haiku/inherit` through the settings map).
pub trait ModelResolver: Send + Sync {
    fn resolve(&self, model: Option<&str>) -> Result<ResolvedModel, String>;
}

/// Starts a turn in an idle session, or steers a busy one: background agents finishing, job
/// exits (track B), `until_regex` matches. The daemon's implementation relays the text; the flag
/// says it carries untrusted content (the session is tainted before acting on it).
#[derive(Clone)]
pub struct SessionWake(pub Arc<dyn Fn(&SessionId, String, bool) + Send + Sync>);

/// Left in the caller's `Conversation::ext` by a call whose subagent read untrusted content; the
/// loop taints the caller when the batch ends (the report carries that content).
#[derive(Debug, Clone, Copy, Default)]
pub struct ReportTainted;

/// Background agents as the daemon keeps them: recorded while they run, so a restart can tell
/// the session one was lost, and stopped when the daemon shuts down.
pub trait BackgroundLedger: Send + Sync {
    fn started(&self, session: &SessionId, id: &str, label: &str);
    /// It finished or failed on its own; the record goes.
    fn finished(&self, session: &SessionId, id: &str);
    /// True once background agents must stop (shutdown). The record stays for the restart.
    fn stopping(&self) -> bool;
}

/// What an agent's `model:` names (design A10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelTarget {
    /// The caller's model (`inherit`, or none given).
    Inherit,
    /// A library slot: `conductor`, `quick`, `reflection`, `crew:<role>`.
    Slot(String),
    /// A model id (`qwen3:8b`, `ollama/qwen3:8b`).
    Model(String),
}

/// Claude Code's aliases and their default slots: `haiku` is the cheapest capable slot
/// (`quick`), `sonnet` and `opus` the conductor's. `aliases` (from settings) overrides.
pub fn model_target(model: Option<&str>, aliases: &HashMap<String, String>) -> ModelTarget {
    let Some(m) = model.map(str::trim).filter(|m| !m.is_empty()) else { return ModelTarget::Inherit };
    let lower = m.to_ascii_lowercase();
    let mapped = aliases.get(&lower).cloned().unwrap_or_else(|| match lower.as_str() {
        "haiku" => "quick".into(),
        "sonnet" | "opus" => "conductor".into(),
        _ => m.to_string(),
    });
    let l = mapped.to_ascii_lowercase();
    if l == "inherit" {
        ModelTarget::Inherit
    } else if ["conductor", "quick", "reflection"].contains(&l.as_str()) || l.starts_with("crew:") {
        ModelTarget::Slot(l)
    } else {
        ModelTarget::Model(mapped)
    }
}

/// `[model_aliases]` in a settings TOML (`haiku = "quick"`), lower-cased keys.
pub fn read_aliases(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut inside = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            inside = line == "[model_aliases]" || line == "[agents.model_aliases]";
            continue;
        }
        if let (true, Some((k, v))) = (inside, line.split_once('=')) {
            let v = v.trim().trim_matches('"').trim_matches('\'').to_string();
            out.insert(k.trim().trim_matches('"').to_ascii_lowercase(), v);
        }
    }
    out
}

/// Builds the tool host a subagent starts from (the caller's tools for a workspace).
pub type HostFactory = Arc<dyn Fn(Option<&Path>) -> Arc<dyn ToolHost> + Send + Sync>;

/// Builds the approval gate for one subagent run, given that run's cancel token: an interrupt
/// (or shutdown, for a background agent) must end a pending approval at once, as the
/// conductor's own gate does.
pub type ApprovalFactory = Arc<dyn Fn(&marlowe_model::CancelToken) -> Arc<dyn ApprovalGate> + Send + Sync>;

/// One gate for every run (tests, and gates that never wait).
pub fn same_gate(gate: Arc<dyn ApprovalGate>) -> ApprovalFactory {
    Arc::new(move |_: &marlowe_model::CancelToken| gate.clone())
}

/// What the subagent service is built with.
#[derive(Clone)]
pub struct SubagentConfig {
    pub resolver: Arc<dyn ModelResolver>,
    pub tools: HostFactory,
    pub approvals: ApprovalFactory,
    pub worktrees: Arc<dyn WorktreeProvider>,
    /// Where types and instructions come from (roots are replaced by the call's workspace).
    pub sources: Sources,
    pub wake: Option<SessionWake>,
    /// Middleware every subagent runs with (hooks, the working set).
    pub middleware: Vec<Arc<dyn ToolMiddleware>>,
    pub cloud_slots: usize,
    pub ledger: Option<Arc<dyn BackgroundLedger>>,
}

/// One `agent` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRequest {
    pub agent_type: String,
    pub description: String,
    pub prompt: String,
    pub isolation: Option<String>,
    pub background: bool,
    /// Overrides the type's model.
    pub model: Option<String>,
}

impl AgentRequest {
    pub fn from_args(args: &Value) -> Result<Self, String> {
        let s = |k: &str| args.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
        Ok(Self {
            agent_type: s("type").or_else(|| s("subagent_type")).unwrap_or_else(|| "general".into()),
            description: s("description").unwrap_or_default(),
            prompt: s("prompt").ok_or("`prompt` is required: the full task, since the agent cannot see this conversation")?,
            isolation: s("isolation"),
            background: args.get("background").or_else(|| args.get("run_in_background")).and_then(Value::as_bool).unwrap_or(false),
            model: s("model"),
        })
    }
}

struct Finished {
    convo: Conversation,
    ty: AgentType,
    model: Option<String>,
}

/// Which pool a run takes its slot from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Lane {
    /// One foreground local stream at a time.
    Local,
    /// One background local agent at a time, apart from the foreground one.
    LocalBackground,
    /// `cap` cloud streams.
    Cloud,
}

impl Lane {
    fn of(model: &ModelInfo, background: bool) -> Lane {
        match (model.local || model.provider.is_local(), background) {
            (true, false) => Lane::Local,
            (true, true) => Lane::LocalBackground,
            (false, _) => Lane::Cloud,
        }
    }
}

#[derive(Default)]
struct Slots {
    used: Mutex<HashMap<Lane, usize>>,
    cv: Condvar,
}

impl Slots {
    /// Waits for a slot in `lane` (`cap` for cloud, one otherwise). False when cancelled first.
    fn acquire(&self, lane: Lane, cap: usize, cancelled: &dyn Fn() -> bool) -> bool {
        let limit = if lane == Lane::Cloud { cap.max(1) } else { 1 };
        let mut g = self.used.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            let n = g.entry(lane).or_insert(0);
            if *n < limit {
                *n += 1;
                return true;
            }
            if cancelled() {
                return false;
            }
            g = self.cv.wait_timeout(g, std::time::Duration::from_millis(25)).unwrap_or_else(|p| p.into_inner()).0;
        }
    }

    fn release(&self, lane: Lane) {
        let mut g = self.used.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(n) = g.get_mut(&lane) {
            *n = n.saturating_sub(1);
        }
        self.cv.notify_all();
    }
}

#[derive(Default)]
struct State {
    slots: Slots,
    finished: Mutex<HashMap<String, Finished>>,
    running: Mutex<HashSet<String>>,
    /// Background agents' cancel tokens, by id.
    background: Mutex<HashMap<String, marlowe_model::CancelToken>>,
    /// Every agent this service started: id -> (label, started in the background).
    labels: Mutex<HashMap<String, (String, bool)>>,
    next: AtomicU64,
}

/// One subagent as `list_agents` and the task list show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSummary {
    pub id: String,
    pub label: String,
    pub background: bool,
    pub running: bool,
}

/// The subagent service in `Services` (one per session).
#[derive(Clone)]
pub struct Subagents {
    cfg: SubagentConfig,
    state: Arc<State>,
}

/// Appended to every subagent's system prompt.
const SUBAGENT_FRAME: &str = "<subagent>\nYou are a subagent doing one task for the main agent. You cannot see its \
conversation: everything you need is in the task. Work on your own; nobody can answer questions. When you are done, \
reply with a concise report of what you found or did, with file paths and line numbers as evidence. Your final \
message is returned to the main agent as it is.\n</subagent>";

impl Subagents {
    pub fn new(cfg: SubagentConfig) -> Self {
        Self { cfg, state: Arc::new(State::default()) }
    }

    /// The agent types for `workspace`.
    pub fn types(&self, workspace: Option<&Path>) -> Vec<AgentType> {
        discover_types(&self.sources_for(workspace)).0
    }

    /// The type `name` resolves to for `workspace` (see [`resolve_type`]).
    pub fn resolve(&self, name: &str, workspace: Option<&Path>) -> Option<AgentType> {
        find_type(&self.types(workspace), name).cloned()
    }

    fn sources_for(&self, workspace: Option<&Path>) -> Sources {
        let mut s = self.cfg.sources.clone();
        if let Some(w) = workspace {
            if !s.roots.iter().any(|r| r == w) {
                s.roots.insert(0, w.to_path_buf());
            }
            s.cwd = Some(w.to_path_buf());
        }
        s
    }

    fn build_agent(
        &self,
        ty: &AgentType,
        model: Option<&str>,
        workspace: Option<&Path>,
        cancel: &marlowe_model::CancelToken,
    ) -> Result<Agent, String> {
        let m = self.cfg.resolver.resolve(model.or(ty.model.as_deref()))?;
        let host: Arc<dyn ToolHost> = Arc::new(FilteredTools::new((self.cfg.tools)(workspace), ty));
        let instructions = instructions::Instructions::load(&self.sources_for(workspace)).render();
        let core = format!("{}\n\n{SUBAGENT_FRAME}", ty.prompt.trim());
        let env = Environment::current(workspace.map(|w| w.display().to_string()), m.info.model_ref().to_string());
        let system = SystemPrompt::with_core(core).sections(PromptSections { instructions, ..Default::default() }).environment(env);
        let mut agent = Agent::new(m.provider, m.info, host, system);
        agent.effort = ty.effort.unwrap_or(m.effort);
        agent.approvals = (self.cfg.approvals)(cancel);
        agent.budget.max_steps = ty.max_turns.unwrap_or(SUBAGENT_MAX_STEPS);
        agent.middleware.extend(self.cfg.middleware.iter().cloned());
        Ok(agent)
    }

    fn run_agent(
        &self,
        agent: &Agent,
        convo: &mut Conversation,
        prompt: &str,
        cancel: &marlowe_model::CancelToken,
        background: bool,
    ) -> Result<String, String> {
        let lane = Lane::of(&agent.model, background);
        let c = cancel.clone();
        if !self.state.slots.acquire(lane, self.cfg.cloud_slots, &|| c.is_cancelled()) {
            return Err("interrupted before the agent started".into());
        }
        let (_tx, rx) = crossbeam_channel::unbounded();
        let out = agent.run_turn(convo, prompt, cancel, &rx, &mut |_| {});
        self.state.slots.release(lane);
        match out.stop {
            TurnStop::Done => Ok(out.text),
            TurnStop::StepLimit | TurnStop::Budget => Ok(format!("{}\n\n[the agent stopped at its step budget]", out.text.trim())),
            TurnStop::Interrupted => Err(format!("interrupted; partial report: {}", out.text.trim())),
            TurnStop::Error => Err(out.error.unwrap_or_else(|| "the agent failed".into())),
        }
    }

    /// Runs an `agent` call for the caller in `cx`.
    pub fn run(&self, req: AgentRequest, cx: &CallCtx) -> ToolOutcome {
        let types = self.types(cx.workspace.as_deref());
        let Some(ty) = find_type(&types, &req.agent_type).cloned() else {
            let names: Vec<&str> = types.iter().map(|t| t.name.as_str()).collect();
            return ToolOutcome::error(format!("no agent type `{}`; available: {}", req.agent_type, names.join(", ")));
        };
        let n = self.state.next.fetch_add(1, Ordering::SeqCst) + 1;
        let id = format!("agent-{n}");
        let isolation = req.isolation.clone().or_else(|| ty.isolation.clone());
        let mut workspace = cx.workspace.clone();
        let mut worktree_note = String::new();
        if isolation.as_deref() == Some("worktree") {
            let Some(repo) = cx.workspace.clone() else {
                return ToolOutcome::error("isolation `worktree` needs a workspace");
            };
            match self.cfg.worktrees.create(&WorktreeRequest { repo, name: id.clone(), base: None, parent: None }) {
                Ok(h) => {
                    worktree_note = format!("\n[worktree: {} on branch {}; kept for review]", h.root.display(), h.branch);
                    workspace = Some(h.root);
                }
                Err(e) => return ToolOutcome::error(format!("could not create a worktree: {e}")),
            }
        }
        // A background agent has its own token (the caller's turn ends at once); a foreground one
        // stops with the caller's.
        let cancel = if req.background { marlowe_model::CancelToken::new() } else { cx.cancel.clone() };
        let mut agent = match self.build_agent(&ty, req.model.as_deref(), workspace.as_deref(), &cancel) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::error(format!("could not start the {} agent: {e}", ty.name)),
        };
        // The caller's session decides the subagent's calls too (track E): its mode and rules
        // through the Permissions service; SubagentStart hooks (their context goes to the agent)
        // and SubagentStop through a hook middleware of the agent's own.
        let mut prompt = req.prompt.clone();
        if let Some(p) = cx.scope.services.get::<crate::approval::Permissions>() {
            agent.services = agent.services.clone().with(p.clone());
        }
        if let Some(runner) = cx.scope.services.get::<Arc<crate::hooks::HookRunner>>() {
            if let Some(c) = runner.subagent_start(&id, &ty.name).context_text() {
                prompt = format!("{prompt}\n\n<system-reminder>\n{c}\n</system-reminder>");
            }
            agent.middleware.push(Arc::new(crate::hooks::HookMiddleware::for_subagent(runner.clone(), &id, &ty.name)));
        }
        let session = SessionId(format!("{}/{id}", cx.session));
        let mut convo = Conversation::new(session, workspace.clone());
        convo.exposure.tainted = cx.tainted;
        let label = if req.description.is_empty() { ty.name.clone() } else { format!("{}: {}", ty.name, req.description) };
        self.state.labels.lock().unwrap_or_else(|p| p.into_inner()).insert(id.clone(), (label.clone(), req.background));

        if req.background {
            let me = self.clone();
            let parent = cx.session.clone();
            let prompt = prompt.clone();
            let model = req.model.clone();
            let ty2 = ty.clone();
            let id2 = id.clone();
            let label2 = label.clone();
            let note = worktree_note.clone();
            self.state.running.lock().unwrap_or_else(|p| p.into_inner()).insert(id.clone());
            self.state.background.lock().unwrap_or_else(|p| p.into_inner()).insert(id.clone(), cancel.clone());
            if let Some(l) = &self.cfg.ledger {
                l.started(&parent, &id, &label);
            }
            let spawned = std::thread::Builder::new().name(format!("marlowe-{id}")).spawn(move || {
                let done = Arc::new(AtomicBool::new(false));
                // Shutdown stops the agent; the ledger keeps its record for the restart.
                if let Some(l) = me.cfg.ledger.clone() {
                    let (done, cancel) = (done.clone(), cancel.clone());
                    let _ = std::thread::Builder::new().name(format!("marlowe-{id2}-watch")).spawn(move || {
                        while !done.load(Ordering::SeqCst) && !cancel.is_cancelled() {
                            if l.stopping() {
                                cancel.cancel();
                                break;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(100));
                        }
                    });
                }
                let result = me.run_agent(&agent, &mut convo, &prompt, &cancel, true);
                done.store(true, Ordering::SeqCst);
                let tainted = convo.exposure.tainted;
                me.state.background.lock().unwrap_or_else(|p| p.into_inner()).remove(&id2);
                me.state.running.lock().unwrap_or_else(|p| p.into_inner()).remove(&id2);
                me.state.finished.lock().unwrap_or_else(|p| p.into_inner()).insert(id2.clone(), Finished { convo, ty: ty2, model });
                if me.cfg.ledger.as_ref().is_some_and(|l| l.stopping()) {
                    return;
                }
                if let Some(l) = &me.cfg.ledger {
                    l.finished(&parent, &id2);
                }
                let text = match result {
                    Ok(t) => format!("[agent {id2} · {label2} finished]\n{}{note}", t.trim()),
                    Err(e) => format!("[agent {id2} · {label2} failed] {e}"),
                };
                if let Some(w) = &me.cfg.wake {
                    (w.0)(&parent, text, tainted);
                }
            });
            if let Err(e) = spawned {
                self.state.running.lock().unwrap_or_else(|p| p.into_inner()).remove(&id);
                self.state.background.lock().unwrap_or_else(|p| p.into_inner()).remove(&id);
                if let Some(l) = &self.cfg.ledger {
                    l.finished(&cx.session, &id);
                }
                return ToolOutcome::error(format!("could not start the agent: {e}"));
            }
            return ToolOutcome::ok(
                format!("Started {label} in the background as {id}. You will be told when it finishes; do not wait or poll for it."),
                format!("{id} started"),
            );
        }

        let result = self.run_agent(&agent, &mut convo, &prompt, &cancel, false);
        if convo.exposure.tainted {
            cx.scope.with_ext(|e| e.insert(ReportTainted));
        }
        self.state.finished.lock().unwrap_or_else(|p| p.into_inner()).insert(id.clone(), Finished { convo, ty, model: req.model });
        match result {
            Ok(text) => ToolOutcome::ok(
                format!("{}{worktree_note}\n\n[agent_id: {id}; send_message continues this agent]", text.trim()),
                format!("{label} done"),
            ),
            Err(e) => ToolOutcome::error(format!("the {label} agent did not finish: {e}")),
        }
    }

    /// Continues a finished subagent with `text` (its earlier context included).
    pub fn send(&self, agent_id: &str, text: &str, cx: &CallCtx) -> ToolOutcome {
        if self.state.running.lock().unwrap_or_else(|p| p.into_inner()).contains(agent_id) {
            return ToolOutcome::error(format!("{agent_id} is still running; you will be told when it finishes"));
        }
        let Some(mut f) = self.state.finished.lock().unwrap_or_else(|p| p.into_inner()).remove(agent_id) else {
            return ToolOutcome::error(format!("no finished agent `{agent_id}` in this session"));
        };
        let workspace = f.convo.workspace.clone();
        let result = match self.build_agent(&f.ty, f.model.as_deref(), workspace.as_deref(), &cx.cancel) {
            Ok(agent) => self.run_agent(&agent, &mut f.convo, text, &cx.cancel, false),
            Err(e) => Err(e),
        };
        if f.convo.exposure.tainted {
            cx.scope.with_ext(|e| e.insert(ReportTainted));
        }
        self.state.finished.lock().unwrap_or_else(|p| p.into_inner()).insert(agent_id.to_string(), f);
        match result {
            Ok(t) => ToolOutcome::ok(format!("{}\n\n[agent_id: {agent_id}]", t.trim()), format!("{agent_id} replied")),
            Err(e) => ToolOutcome::error(format!("{agent_id} did not finish: {e}")),
        }
    }
}

impl Subagents {
    /// Every agent this session started, in the order it started them.
    pub fn agents(&self) -> Vec<AgentSummary> {
        let running = self.state.running.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let labels = self.state.labels.lock().unwrap_or_else(|p| p.into_inner());
        let mut out: Vec<AgentSummary> = labels
            .iter()
            .map(|(id, (label, background))| AgentSummary {
                id: id.clone(),
                label: label.clone(),
                background: *background,
                running: running.contains(id),
            })
            .collect();
        out.sort_by_key(|a| a.id.trim_start_matches("agent-").parse::<u64>().unwrap_or(u64::MAX));
        out
    }

    /// Whether `id` names an agent this service started (running or finished).
    pub fn knows(&self, id: &str) -> bool {
        self.state.labels.lock().unwrap_or_else(|p| p.into_inner()).contains_key(id)
    }

    /// Cancels one running background agent; it still reports, as interrupted. False when `id`
    /// is not a running background agent.
    pub fn cancel_one(&self, id: &str) -> bool {
        match self.state.background.lock().unwrap_or_else(|p| p.into_inner()).get(id) {
            Some(c) => {
                c.cancel();
                true
            }
            None => false,
        }
    }

    /// The background agents still running.
    pub fn background_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.state.background.lock().unwrap_or_else(|p| p.into_inner()).keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Cancels every running background agent; each still reports, as interrupted.
    pub fn cancel_background(&self) -> usize {
        let map = self.state.background.lock().unwrap_or_else(|p| p.into_inner());
        map.values().for_each(|c| c.cancel());
        map.len()
    }
}

/// The `agent` tool.
pub fn agent_call(args: &Value, cx: &CallCtx) -> ToolOutcome {
    let req = match AgentRequest::from_args(args) {
        Ok(r) => r,
        Err(e) => return ToolOutcome::error(e),
    };
    match cx.scope.services.get::<Subagents>() {
        Some(s) => s.run(req, cx),
        None => ToolOutcome::error("subagents are not available in this session"),
    }
}

/// The `send_message` tool.
pub fn send_message_call(args: &Value, cx: &CallCtx) -> ToolOutcome {
    let id = args.get("agent_id").or_else(|| args.get("to")).and_then(Value::as_str).map(str::trim).unwrap_or("");
    let text = args.get("text").or_else(|| args.get("message")).and_then(Value::as_str).unwrap_or("");
    if id.is_empty() || text.trim().is_empty() {
        return ToolOutcome::error("`agent_id` and `text` are required");
    }
    // A subagent this session started continues here; anything else (a crew run, a workflow
    // agent, `session:<id>`) goes to the daemon's messenger (track N).
    let subagents = cx.scope.services.get::<Subagents>();
    if subagents.is_some_and(|s| s.knows(id)) || id.starts_with("agent-") {
        return match subagents {
            Some(s) => s.send(id, text, cx),
            None => ToolOutcome::error("subagents are not available in this session"),
        };
    }
    match cx.scope.services.get::<crate::background::Background>() {
        Some(b) => match b.0.send_message(&cx.session, id, text, cx.tainted, &cx.cancel) {
            Ok(said) => ToolOutcome::ok(said, format!("sent to {id}")),
            Err(e) => ToolOutcome::error(e),
        },
        None => ToolOutcome::error("messages to other agents and sessions are not available here"),
    }
}

/// Whether running agent type `name` may bring untrusted content back (its tools reach the
/// web or MCP), so the caller is tainted by its report. Types resolve as [`Subagents::types`]
/// does for a daemon with profile `home` (its agents, roles and plugins included). A run that
/// did read untrusted content also taints the caller afterwards ([`ReportTainted`]).
pub fn may_taint(name: &str, home: Option<&Path>, workspace: Option<&Path>) -> bool {
    let (types, _) = discover_types(&Sources::for_session(home, workspace));
    match find_type(&types, name) {
        Some(t) => match &t.tools {
            None => true,
            Some(tools) => tools.iter().any(|x| x.starts_with("web_") || x.starts_with("mcp__") || x == "arxiv" || x == "paper"),
        },
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_code_tool_names_map() {
        assert_eq!(map_tool("Read"), ["read"]);
        assert_eq!(map_tool("Bash(git log:*)"), ["bash"]);
        assert_eq!(map_tool("Agent(a, b)"), ["agent"]);
        assert_eq!(map_tool("mcp__github__get_issue"), ["mcp__github__get_issue"]);
        assert_eq!(map_tool("LSP").len(), 5);
    }

    #[test]
    fn builtins_and_lookup() {
        let t = builtin_types();
        assert!(find_type(&t, "Explore").is_some_and(|x| x.read_only));
        assert_eq!(find_type(&t, "general-purpose").map(|x| x.name.as_str()), Some("general"));
        assert!(find_type(&t, "nope").is_none());
    }
}
