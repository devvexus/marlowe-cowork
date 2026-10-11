//! The registration table: every built-in tool is one [`ToolDef`], declared in the file of the
//! group that owns it. [`crate::BuiltinTools`] only concatenates the groups and looks calls up by
//! name; it holds no per-tool `match`. A track adds a tool by adding a `ToolDef` to its own group
//! file and nothing else.
//!
//! | File | Group | Owner | Tools today |
//! |---|---|---|---|
//! | `files.rs` | files | A | read, write, edit |
//! | `search.rs` | search | B | glob, grep |
//! | `shell.rs` | shell | B | bash, powershell, python |
//! | `web.rs` | web | seam | web_fetch, arxiv, paper, web_search |
//! | `code.rs` | code | C | (symbols, definition, references, hover, diagnostics, rename_symbol) |
//! | `verify.rs` | verify | D | (project, service, http_check, browser) |
//! | `agent.rs` | agent | F | (todo, ask_user, skill, agent, send_message, exit_plan, worktrees) |
//! | `git.rs` | git | J | (git, gh, ci_status) |
//! | `ml.rs` | ml | J2 | (train_launch, experiments) |
//! | `mcp.rs` | mcp | K | (tool_search; `mcp__srv__tool` come from the MCP host) |
//! | `workflow.rs` | workflow | W | (workflow, structured_output, report_findings) |
//! | `tasks.rs` | tasks | N | (task_list, task_output, task_stop, monitor, cron_*, schedule_wakeup, list_agents) |
//! | `notify.rs` | notify | N | (notify, send_file, notices) |
//!
//! Order is the model-visible order (stable, so the prompt cache holds): groups in [`GROUPS`]
//! order, tools in their file's order. Schemas are flat, `additionalProperties: false`.
//!
//! A tool that needs state across calls (B's job table, C's LSP manager) keeps it in
//! [`crate::BuiltinTools::state`], a per-host typed map, so `ToolDef` stays plain `fn` pointers.
//! A tool that needs session state (F's todos) or a daemon service (`ask_user`, worktrees) gets
//! them from its [`CallCtx::scope`].
//!
//! Besides `tools()`, every group file declares four hooks, so its owner never edits this file,
//! `lib.rs` or the binary's `wire.rs`:
//! - `middleware(&BuiltinTools)`: loop middleware built next to the host's state (A's
//!   `FileState` checks, B's job notices, C's diagnostics). The host hands it to the agent
//!   through `ToolHost::middleware`, only while the group is exposed.
//! - `persona_block()`: `Some(include_str!(".../persona/blocks/<group>.md"))` once the owner
//!   writes the block; appended only while the group is exposed and has a live tool.
//! - `configure(BuiltinTools, &HostSetup)`: per-host setup from the profile home (LSP paths,
//!   the verify profile), called by the binary through [`configure`].
//! - `detect(root)`: whether a local model gets the group in this workspace (a git repo, a
//!   project profile). Frontier models get every group; see [`exposed_groups`].

pub mod agent;
pub mod code;
pub mod files;
pub mod git;
pub mod mcp;
pub mod ml;
pub mod notify;
pub mod search;
pub mod shell;
pub mod tasks;
pub mod verify;
pub mod web;
pub mod workflow;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use marlowe_agent::{
    CallCtx, Consequence, Effects, Exposure, ExposureProfile, Mutation, Profiles, SandboxView, ToolCallReq, ToolCtx,
    ToolGroup, ToolManifest, ToolMiddleware, ToolOutcome,
};
use marlowe_model::ToolSpec;
use serde_json::{json, Value};

use crate::BuiltinTools;

/// Runs the tool. `args` is a JSON object (checked before dispatch). `cx` derefs to the
/// [`ToolCtx`]; its `scope` carries the session's extensions and the daemon's services.
pub type CallFn = fn(&BuiltinTools, &Value, &CallCtx) -> ToolOutcome;
/// Whether the call may run without asking, given what the session was exposed to.
pub type ConsequenceFn = fn(&BuiltinTools, &ToolCallReq, &Exposure) -> Consequence;
/// The call's consequence inside a crew sandbox (`None` on the def: the plain consequence).
pub type CrewConsequenceFn = fn(&BuiltinTools, &ToolCallReq, &Exposure, &dyn SandboxView) -> Consequence;
/// The one line shown when the call starts, e.g. `read src/main.rs`.
pub type DescribeFn = fn(&ToolCallReq) -> String;
/// Whether running the call brings untrusted content into the conversation.
pub type TaintFn = fn(&BuiltinTools, &ToolCallReq) -> bool;
/// The network host the call reaches (recorded as approved when the user allows it).
pub type EgressFn = fn(&BuiltinTools, &ToolCallReq) -> Option<String>;
/// What the call will change, for `pre_mutation` middleware.
pub type MutationFn = fn(&BuiltinTools, &ToolCallReq, &ToolCtx) -> Mutation;
/// Whether the host offers the tool to the model (e.g. web_search needs a backend).
pub type EnabledFn = fn(&BuiltinTools) -> bool;

/// One registered tool.
pub struct ToolDef {
    pub spec: ToolSpec,
    /// Group, profiles, effects and path arguments.
    pub manifest: ToolManifest,
    pub call: CallFn,
    pub consequence: ConsequenceFn,
    pub crew_consequence: Option<CrewConsequenceFn>,
    pub describe: DescribeFn,
    pub taints: TaintFn,
    pub egress: EgressFn,
    pub mutation: MutationFn,
    pub enabled: EnabledFn,
}

impl ToolDef {
    /// A free, non-tainting, non-mutating tool in `group` with `effects`; adjust with the
    /// builder methods.
    pub fn new(spec: ToolSpec, group: ToolGroup, effects: Effects, call: CallFn) -> Self {
        Self {
            spec,
            manifest: ToolManifest { group, profiles: Profiles::ALL, effects, path_args: &[] },
            call,
            consequence: |_, _, _| Consequence::Free,
            crew_consequence: None,
            describe: describe_default,
            taints: |_, _| false,
            egress: |_, _| None,
            mutation: |_, _, _| Mutation::None,
            enabled: |_| true,
        }
    }

    pub fn name(&self) -> &str {
        &self.spec.name
    }

    pub fn profiles(mut self, profiles: Profiles) -> Self {
        self.manifest.profiles = profiles;
        self
    }

    pub fn path_args(mut self, args: &'static [&'static str]) -> Self {
        self.manifest.path_args = args;
        self
    }

    pub fn consequence(mut self, f: ConsequenceFn) -> Self {
        self.consequence = f;
        self
    }

    pub fn crew_consequence(mut self, f: CrewConsequenceFn) -> Self {
        self.crew_consequence = Some(f);
        self
    }

    pub fn describe(mut self, f: DescribeFn) -> Self {
        self.describe = f;
        self
    }

    pub fn taints(mut self, f: TaintFn) -> Self {
        self.taints = f;
        self
    }

    pub fn egress(mut self, f: EgressFn) -> Self {
        self.egress = f;
        self
    }

    pub fn mutation(mut self, f: MutationFn) -> Self {
        self.mutation = f;
        self
    }

    pub fn enabled(mut self, f: EnabledFn) -> Self {
        self.enabled = f;
        self
    }
}

/// What the binary knows when it builds a host, for each group's `configure`.
#[derive(Debug, Clone, Copy)]
pub struct HostSetup<'a> {
    /// The profile home (`%LOCALAPPDATA%\marlowe\<profile>`).
    pub home: &'a Path,
    /// The session's workspace root, if any.
    pub workspace: Option<&'a Path>,
}

/// Builds a group's middleware for one host.
pub type MiddlewareFn = fn(&BuiltinTools) -> Vec<Arc<dyn ToolMiddleware>>;
/// A group's persona block text, once it exists.
pub type PersonaBlockFn = fn() -> Option<&'static str>;
/// Configures a new host for a group.
pub type ConfigureFn = fn(BuiltinTools, &HostSetup<'_>) -> BuiltinTools;
/// Whether a local model gets the group in this workspace root.
pub type DetectFn = fn(Option<&Path>) -> bool;

/// One tool group: its tools and its four hooks, all declared in the group's own file.
pub struct Group {
    pub group: ToolGroup,
    pub tools: fn() -> Vec<ToolDef>,
    pub middleware: MiddlewareFn,
    pub persona_block: PersonaBlockFn,
    pub configure: ConfigureFn,
    pub detect: DetectFn,
}

macro_rules! group {
    ($group:ident, $file:ident) => {
        Group {
            group: ToolGroup::$group,
            tools: $file::tools,
            middleware: $file::middleware,
            persona_block: $file::persona_block,
            configure: $file::configure,
            detect: $file::detect,
        }
    };
}

/// The groups, in model-visible order.
pub const GROUPS: &[Group] = &[
    group!(Files, files),
    group!(Search, search),
    group!(Shell, shell),
    group!(Web, web),
    group!(Code, code),
    group!(Verify, verify),
    group!(Agent, agent),
    group!(Git, git),
    group!(Ml, ml),
    group!(Mcp, mcp),
    group!(Workflow, workflow),
    group!(Tasks, tasks),
    group!(Notify, notify),
];

/// The registration of `group`, when this crate has one (`memory` and `crew` are the daemon's).
pub fn group(group: ToolGroup) -> Option<&'static Group> {
    GROUPS.iter().find(|g| g.group == group)
}

/// Runs every group's `configure` over a new host, in [`GROUPS`] order. The binary calls this
/// once per host (`crates/marlowe/src/wire.rs`).
pub fn configure(tools: BuiltinTools, setup: &HostSetup<'_>) -> BuiltinTools {
    GROUPS.iter().fold(tools, |t, g| (g.configure)(t, setup))
}

/// The groups a session exposes for `profile` in `root`: every group for a frontier model; the
/// core groups plus the detected ones for a local model (design 3, fixed per session).
pub fn exposed_groups(profile: ExposureProfile, root: Option<&Path>) -> Vec<ToolGroup> {
    if let Ok(list) = std::env::var(GROUPS_OVERRIDE_ENV) {
        match parse_group_list(&list) {
            Ok(groups) => return groups,
            // A typo must not quietly run the full tool set in an ablation arm.
            Err(e) => eprintln!("marlowe: {GROUPS_OVERRIDE_ENV} ignored: {e}"),
        }
    }
    let detected: Vec<ToolGroup> = GROUPS.iter().filter(|g| (g.detect)(root)).map(|g| g.group).collect();
    marlowe_agent::select_groups(profile, &detected)
}

/// The eval ablation switch (`marlowe eval`, A3): a comma-separated group list (`shell`) replaces
/// the session's groups, so the shell-only arm runs the same binary, prompt and model with only a
/// shell to work with. Unset in every normal run.
pub const GROUPS_OVERRIDE_ENV: &str = "MARLOWE_TOOL_GROUPS";

/// `shell,files` to the groups it names; any unknown name rejects the whole list.
pub fn parse_group_list(list: &str) -> Result<Vec<ToolGroup>, String> {
    let mut out = Vec::new();
    for name in list.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        let group: ToolGroup = serde_json::from_value(serde_json::Value::String(name.to_string()))
            .map_err(|_| format!("unknown tool group {name:?}"))?;
        out.push(group);
    }
    if out.is_empty() {
        return Err("no tool groups named".into());
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// For group files: no middleware.
pub fn no_middleware(_tools: &BuiltinTools) -> Vec<Arc<dyn ToolMiddleware>> {
    Vec::new()
}

/// For group files: no configuration.
pub fn no_configure(tools: BuiltinTools, _setup: &HostSetup<'_>) -> BuiltinTools {
    tools
}

/// Every registered tool, concatenated once per process, with a name index.
pub struct Table {
    pub defs: Vec<ToolDef>,
    index: HashMap<String, usize>,
}

impl Table {
    pub fn get(&self, name: &str) -> Option<&ToolDef> {
        self.index.get(name).map(|&i| &self.defs[i])
    }
}

/// The table. Panics at first use if two tools share a name or a def sits in the wrong group's
/// file (a registration bug, caught by the first test that builds a host).
pub fn table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut defs = Vec::new();
        for g in GROUPS {
            for d in (g.tools)() {
                assert_eq!(d.manifest.group, g.group, "`{}` is registered in the {:?} file", d.name(), g.group);
                defs.push(d);
            }
        }
        let mut index = HashMap::new();
        for (i, d) in defs.iter().enumerate() {
            assert!(index.insert(d.spec.name.clone(), i).is_none(), "tool `{}` is registered twice", d.spec.name);
        }
        Table { defs, index }
    })
}

/// A flat object schema: `properties`, `required`, no additional properties.
pub fn spec(name: &str, description: &str, properties: Value, required: &[&str]) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        description: description.to_string(),
        input_schema: json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        }),
    }
}

/// `name detail`, where detail is the first line of the string argument `key` (falling back to
/// `path`), at most 60 bytes.
pub fn describe_by(call: &ToolCallReq, key: &str) -> String {
    let detail = crate::str_arg(&call.args, key)
        .or_else(|| crate::str_arg(&call.args, "path"))
        .map(|s| crate::first_line(s, 60))
        .unwrap_or_default();
    if detail.is_empty() {
        call.name.clone()
    } else {
        format!("{} {}", call.name, detail)
    }
}

/// The description for a tool with no key argument (also unknown tools).
pub fn describe_default(call: &ToolCallReq) -> String {
    describe_by(call, "")
}

/// Shortens `s` to `max` chars with an ellipsis (the crew's summary clip, so crew approvals read
/// as they did before the table).
pub fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{}…", cut.trim_end())
}
