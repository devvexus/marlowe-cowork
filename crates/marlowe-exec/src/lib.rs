//! The built-in tools behind [`marlowe_agent::ToolHost`]: read, write, edit, glob, grep, bash,
//! powershell, python, web_fetch, arxiv, paper and (with an injected backend) web_search.
//!
//! Every tool is one entry in the registration table ([`specs`]): `specs/<group>.rs` declares its
//! schema, manifest, call, consequence (plain and in a crew sandbox), description, taint, egress
//! and mutation, plus the group's middleware, persona block, host configuration and detection.
//! This file only looks calls up by name; it holds no per-tool `match`, so a track adds tools
//! without touching it.
//!
//! A host exposes a fixed set of groups for its session ([`BuiltinTools::with_exposure`], default
//! every group): `specs()` lists only exposed tools, and a call to a tool outside the set is
//! refused.
//!
//! The host is built per session with that session's workspace root. Relative paths resolve
//! against the root; reads are free anywhere, writes are free only inside the root, and a root
//! that is too broad (a drive root, the home directory, a system or app-data directory) frees
//! nothing. Shell tools always ask. Once untrusted content is in the conversation (a fetched
//! page, search results, a file from outside the root), web_search and web_fetch to a host not
//! yet approved this session ask too. Every child process has the credential variables from
//! [`marlowe_os::SCRUBBED_ENV`] removed from its environment (by `marlowe_os::child`, the only
//! spawner) and runs in its own kill-on-close Job Object, so a timeout or interrupt kills the
//! whole tree.

#![forbid(unsafe_code)]

pub mod corpus;
pub mod git;
pub mod jobs;
mod paper;
pub mod paths;
pub mod render;
pub mod research;
pub mod search;
pub mod shell;
pub mod specs;
pub mod summarize;
pub mod text;
mod web;

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use marlowe_agent::{
    CallCtx, Consequence, Exposure, ExposureProfile, Mutation, PersonaBlock, SandboxView, ToolCallReq, ToolCtx,
    ToolGroup, ToolHost, ToolManifest, ToolMiddleware, ToolOutcome,
};
use marlowe_model::{ModelInfo, ToolSpec};
use marlowe_os::workspace::{LocalFs, Workspace};
use serde_json::Value;

pub use shell::{run_bounded, run_script, Interpreter, Leftovers, ShellLimits, ShellRun, SpawnOpts};
pub use research::{ArxivEntry, ArxivFeed, HttpGet, HttpResponse, RateLimiter, ResearchConfig};
pub use web::{SearchBackend, SearchResponse, SearchResult};

/// Window assumed when the host is not told the model's; "large file" is measured against it.
pub const DEFAULT_CONTEXT_TOKENS: u32 = 32_768;

pub struct BuiltinTools {
    root: Option<PathBuf>,
    /// Writes inside `root` are free; false when there is no root or it is too broad.
    free_writes: bool,
    /// More folders whose files are the user's own: reading them does not taint.
    read_roots: Vec<PathBuf>,
    context_tokens: u32,
    /// Serialises write/edit read-modify-write cycles within a parallel batch.
    writes: Mutex<()>,
    search: Option<Arc<dyn SearchBackend>>,
    research: ResearchConfig,
    limits: ShellLimits,
    scrub: Vec<String>,
    /// Set on every child after the scrub (the crew's no-push `GIT_CONFIG_*` entries).
    child_env: Vec<(String, String)>,
    /// Pins shell children to this directory tree (a crew sandbox); `cwd` must stay inside it.
    shell_dir: Option<PathBuf>,
    leftovers: shell::Leftovers,
    held: shell::HeldJobs,
    /// Where file, search and code tools act (this machine, or a container from track K).
    port: Arc<dyn Workspace>,
    /// Per-host state for tools that keep it across calls, one value per type ([`Self::state`]).
    states: Mutex<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
    /// The model class this host's tools are offered to.
    profile: ExposureProfile,
    /// The groups this host exposes (sorted).
    groups: Vec<ToolGroup>,
}

impl BuiltinTools {
    /// A host rooted at `workspace` (`None`: relative paths use the process cwd and every write
    /// asks). A root that [`paths::is_broad_root`] refuses also makes every write ask.
    pub fn new(workspace: Option<PathBuf>) -> Self {
        let free_writes = workspace.as_deref().is_some_and(|r| !paths::is_broad_root(r));
        Self {
            root: workspace,
            free_writes,
            read_roots: Vec::new(),
            context_tokens: DEFAULT_CONTEXT_TOKENS,
            writes: Mutex::new(()),
            search: None,
            research: ResearchConfig::default(),
            limits: ShellLimits::production(),
            scrub: marlowe_brains::scrubbed_env().iter().map(|s| s.to_string()).collect(),
            child_env: Vec::new(),
            shell_dir: None,
            leftovers: shell::Leftovers::Release,
            held: shell::HeldJobs::default(),
            port: Arc::new(LocalFs),
            states: Mutex::new(HashMap::new()),
            profile: ExposureProfile::Frontier,
            groups: ToolGroup::ALL.to_vec(),
        }
    }

    /// A host rooted at a crew sandbox the caller created for this purpose (under the profile's
    /// `worktrees` or `scratch` folder). Writes inside it are free and reads inside it do not
    /// taint, even though the profile lives under app data, which [`paths::is_broad_root`] refuses.
    pub fn new_sandbox(root: PathBuf) -> Self {
        let mut tools = Self::new(Some(root));
        tools.free_writes = true;
        tools
    }

    /// Reads inside `dir` (the user's own workspace) do not taint. A broad `dir` is ignored.
    pub fn with_read_root(mut self, dir: PathBuf) -> Self {
        if !paths::is_broad_root(&dir) {
            self.read_roots.push(dir);
        }
        self
    }

    /// The model's context window in tokens.
    pub fn with_context_tokens(mut self, tokens: u32) -> Self {
        if tokens > 0 {
            self.context_tokens = tokens;
        }
        self
    }

    /// Enables the `web_search` tool.
    pub fn with_search(mut self, backend: Arc<dyn SearchBackend>) -> Self {
        self.search = Some(backend);
        self
    }

    /// Cache directory, rate limiter and HTTP port for `arxiv` and `paper`.
    pub fn with_research(mut self, cfg: ResearchConfig) -> Self {
        self.research = cfg;
        self
    }

    /// Overrides shell bounds (tests use short ones).
    pub fn with_shell_limits(mut self, limits: ShellLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Variables set on every child process, after the credential scrub.
    pub fn with_child_env(mut self, env: impl IntoIterator<Item = (String, String)>) -> Self {
        self.child_env.extend(env);
        self
    }

    /// Shell children start in `dir` instead of the workspace root, and a `cwd` argument must
    /// resolve inside it.
    pub fn with_shell_dir(mut self, dir: PathBuf) -> Self {
        self.shell_dir = Some(dir);
        self
    }

    /// What happens to background processes a finished command left running (default: they
    /// run on). [`shell::Leftovers::KillWithHost`] kills them when this host is dropped.
    pub fn with_leftovers(mut self, mode: shell::Leftovers) -> Self {
        self.leftovers = mode;
        self
    }

    /// Kills background processes held under [`shell::Leftovers::KillWithHost`].
    pub fn kill_leftovers(&self) {
        self.held.kill_all();
    }

    /// Background processes still held under [`shell::Leftovers::KillWithHost`].
    pub fn leftover_processes(&self) -> u32 {
        self.held.active()
    }

    pub fn workspace(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Where file, search and code tools act. [`LocalFs`] unless a port was injected.
    pub fn with_workspace_port(mut self, port: Arc<dyn Workspace>) -> Self {
        self.port = port;
        self
    }

    /// The workspace port tools should read, write, walk and exec through.
    pub fn workspace_port(&self) -> &dyn Workspace {
        self.port.as_ref()
    }

    /// This host's value of type `T`, created with `T::default()` on first use. For tools that
    /// keep state across calls (B's job table, C's LSP manager) without adding fields here.
    pub fn state<T: Default + Send + Sync + 'static>(&self) -> Arc<T> {
        let mut states = self.states.lock().unwrap_or_else(|p| p.into_inner());
        let entry = states.entry(TypeId::of::<T>()).or_insert_with(|| Arc::new(T::default()));
        Arc::clone(entry).downcast::<T>().unwrap_or_else(|_| unreachable!("keyed by TypeId"))
    }

    /// Installs this host's value of type `T` (configured state, e.g. an LSP manager with paths).
    pub fn with_state<T: Send + Sync + 'static>(self, value: T) -> Self {
        self.states.lock().unwrap_or_else(|p| p.into_inner()).insert(TypeId::of::<T>(), Arc::new(value));
        self
    }

    /// Fixes the tool set for this host's session: tools in `groups` that admit `profile`.
    /// The default is every group for a frontier model (everything registered).
    pub fn with_exposure(mut self, profile: ExposureProfile, groups: Vec<ToolGroup>) -> Self {
        let mut groups = groups;
        groups.sort();
        groups.dedup();
        self.profile = profile;
        self.groups = groups;
        self
    }

    /// [`Self::with_exposure`] for `model` in this host's workspace: every group for a frontier
    /// model; the core groups plus the detected ones for a local model.
    pub fn with_model_exposure(self, model: &ModelInfo) -> Self {
        let profile = marlowe_agent::exposure_profile(model);
        let groups = specs::exposed_groups(profile, self.root.as_deref());
        self.with_exposure(profile, groups)
    }

    /// The model class and groups this host exposes.
    pub fn exposure(&self) -> (ExposureProfile, &[ToolGroup]) {
        (self.profile, &self.groups)
    }

    /// Whether `def` is in this host's tool set (its group is exposed and it admits the profile).
    pub fn exposes(&self, def: &specs::ToolDef) -> bool {
        self.groups.contains(&def.manifest.group) && def.manifest.profiles.admits(self.profile)
    }

    /// The registered tool `name`.
    pub fn tool(&self, name: &str) -> Option<&'static specs::ToolDef> {
        specs::table().get(name)
    }

    fn root_for(&self, ctx: &ToolCtx) -> Option<PathBuf> {
        self.root.clone().or_else(|| ctx.workspace.clone())
    }

    /// [`Self::root_for`], borrowed.
    pub(crate) fn root_for_ref<'a>(&'a self, ctx: &'a ToolCtx) -> Option<&'a Path> {
        self.root.as_deref().or(ctx.workspace.as_deref())
    }

    /// Where shell `cwd` and script paths resolve: the pinned shell dir, else the root.
    pub(crate) fn shell_root(&self, ctx: &ToolCtx) -> Option<PathBuf> {
        self.shell_dir.clone().or_else(|| self.root_for(ctx))
    }

    /// The plain (non-sandbox) consequence of `call` under `exposure`.
    pub(crate) fn consequence_of(&self, call: &ToolCallReq, exposure: &Exposure) -> Consequence {
        match self.tool(&call.name) {
            Some(def) => (def.consequence)(self, call, exposure),
            None => Consequence::Free,
        }
    }

    /// Whether `raw` lies inside a root that frees writes.
    pub(crate) fn inside_root(&self, raw: &str) -> bool {
        match &self.root {
            Some(root) if self.free_writes => paths::inside(root, &paths::resolve(Some(root), raw)),
            _ => false,
        }
    }

    /// Whether reading `raw` stays within the user's own files: the write root or a read root.
    pub(crate) fn trusted_read(&self, raw: &str) -> bool {
        if self.inside_root(raw) {
            return true;
        }
        let abs = paths::resolve(self.root.as_deref(), raw);
        self.read_roots.iter().any(|r| paths::inside(r, &abs))
    }
}

pub(crate) fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// The `paper` tool's input, under any of its accepted names.
pub(crate) fn research_input(args: &Value) -> &str {
    str_arg(args, "url_or_arxiv_id").or_else(|| str_arg(args, "url")).or_else(|| str_arg(args, "id")).unwrap_or("")
}

pub(crate) fn int_arg(args: &Value, key: &str) -> Option<i64> {
    match args.get(key)? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// ` (+N lines)` when `s` has lines beyond its first, so a one-line summary never hides a script.
pub(crate) fn more_lines(s: &str) -> String {
    match s.trim().lines().count().saturating_sub(1) {
        0 => String::new(),
        1 => " (+1 line)".into(),
        n => format!(" (+{n} lines)"),
    }
}

pub(crate) fn first_line(s: &str, max: usize) -> String {
    let line = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    let cut = text::floor_boundary(line, max);
    if cut < line.len() {
        format!("{}…", &line[..cut])
    } else {
        line.to_string()
    }
}

impl ToolHost for BuiltinTools {
    fn specs(&self) -> Vec<ToolSpec> {
        specs::table().defs.iter().filter(|d| self.exposes(d) && (d.enabled)(self)).map(|d| d.spec.clone()).collect()
    }

    fn call(&self, call: &ToolCallReq, ctx: &ToolCtx) -> ToolOutcome {
        self.call_scoped(call, &CallCtx::detached(ctx.clone()))
    }

    fn call_scoped(&self, call: &ToolCallReq, cx: &CallCtx) -> ToolOutcome {
        if cx.cancel.is_cancelled() {
            return ToolOutcome::error("interrupted before the tool ran");
        }
        let args = &call.args;
        if !args.is_object() {
            return ToolOutcome::error(format!(
                "`{}` expects a JSON object of arguments, got: {}",
                call.name,
                first_line(&args.to_string(), 80)
            ));
        }
        match self.tool(&call.name) {
            Some(def) if self.exposes(def) => (def.call)(self, args, cx),
            Some(_) => ToolOutcome::error(format!("`{}` is not available in this session", call.name)),
            None => ToolOutcome::error(format!("unknown tool `{}`", call.name)),
        }
    }

    fn middleware(&self) -> Vec<Arc<dyn ToolMiddleware>> {
        specs::GROUPS.iter().filter(|g| self.groups.contains(&g.group)).flat_map(|g| (g.middleware)(self)).collect()
    }

    fn persona_blocks(&self) -> Vec<PersonaBlock> {
        let table = specs::table();
        specs::GROUPS
            .iter()
            .filter(|g| self.groups.contains(&g.group))
            .filter(|g| table.defs.iter().any(|d| d.manifest.group == g.group && self.exposes(d) && (d.enabled)(self)))
            .filter_map(|g| (g.persona_block)().map(|text| PersonaBlock::Tool { group: g.group, text }))
            .collect()
    }

    fn consequence(&self, call: &ToolCallReq) -> Consequence {
        self.consequence_of(call, &Exposure::default())
    }

    fn consequence_exposed(&self, call: &ToolCallReq, exposure: &Exposure) -> Consequence {
        self.consequence_of(call, exposure)
    }

    fn consequence_in_sandbox(
        &self,
        call: &ToolCallReq,
        exposure: &Exposure,
        sandbox: &dyn SandboxView,
    ) -> Option<Consequence> {
        let f = self.tool(&call.name)?.crew_consequence?;
        Some(f(self, call, exposure, sandbox))
    }

    fn taints(&self, call: &ToolCallReq) -> bool {
        self.tool(&call.name).is_some_and(|d| (d.taints)(self, call))
    }

    fn egress_host(&self, call: &ToolCallReq) -> Option<String> {
        (self.tool(&call.name)?.egress)(self, call)
    }

    fn manifest(&self, name: &str) -> Option<ToolManifest> {
        self.tool(name).map(|d| d.manifest.clone())
    }

    fn mutation(&self, call: &ToolCallReq, ctx: &ToolCtx) -> Mutation {
        match self.tool(&call.name) {
            Some(def) => (def.mutation)(self, call, ctx),
            None => Mutation::None,
        }
    }

    fn describe(&self, call: &ToolCallReq) -> String {
        match self.tool(&call.name) {
            Some(def) => (def.describe)(call),
            None => specs::describe_default(call),
        }
    }
}
