//! The tool port. Implemented by `marlowe-exec::BuiltinTools` (a registration table: one
//! `ToolDef` per tool in `marlowe-exec/src/specs/<group>.rs`), wrapped by the daemon's memory and
//! crew hosts and by the crew's `CrewTools`.
//!
//! Beyond `call`/`consequence`, a host answers three seam questions with defaults that keep old
//! hosts working: [`ToolHost::manifest`] (group, effects and path arguments, for E's rules and
//! modes, F's agent tool filters and hook matchers), [`ToolHost::mutation`] (what a call is about
//! to change, so `pre_mutation` middleware can snapshot it first: F2's checkpoints, A's
//! `FileState`), and [`ToolHost::consequence_in_sandbox`] (the crew's per-tool consequence inside a
//! sandbox, which seam-crew switches `CrewTools` onto).
//!
//! A host also hands the loop what belongs to its tools: [`ToolHost::call_scoped`] (a call with
//! its [`SessionScope`]), [`ToolHost::middleware`] and [`ToolHost::persona_blocks`]. Every wrapper
//! forwards all three.

use std::any::{Any, TypeId};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use marlowe_model::{CancelToken, ModelInfo, ResultPart, ToolSpec};
use marlowe_proto::SessionId;

pub use marlowe_tools::{select_groups, ExposureProfile, Profiles, ToolGroup};

use crate::middleware::{Extensions, ToolMiddleware};
use crate::system::PersonaBlock;

/// One tool call as the loop hands it to the host (id already assigned).
pub type ToolCallReq = marlowe_model::ToolCall;

/// Per-call context.
///
/// Build it with [`ToolCtx::new`]. The fields stay public because the crew's tests build it as a
/// literal (seam-crew moves them to `new`). What a call needs of its session beyond these (the
/// conversation's extensions, daemon services) arrives in [`CallCtx::scope`].
#[derive(Debug, Clone)]
pub struct ToolCtx {
    pub session: SessionId,
    pub turn: u32,
    /// The session's workspace root, if any.
    pub workspace: Option<PathBuf>,
    /// Fires when the turn is interrupted; long-running tools must honour it.
    pub cancel: CancelToken,
    /// Untrusted content had entered the conversation when this batch of calls started.
    pub tainted: bool,
}

impl ToolCtx {
    pub fn new(session: SessionId, turn: u32, workspace: Option<PathBuf>, cancel: CancelToken, tainted: bool) -> Self {
        Self { session, turn, workspace, cancel, tainted }
    }
}

/// Daemon-lifetime services a tool or middleware may need: F's `ask_user` channel, subagent
/// runner and `WorktreeProvider`, E's hook runner, B's job notices. A typed map with one value per
/// type. Each track declares its service type in its own file, and the daemon's `stack.rs`
/// installs it through that track's `services` constructor, so no track edits a shared trait.
/// Cheap to clone.
#[derive(Clone, Default)]
pub struct Services {
    map: Arc<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
}

impl Services {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds (or replaces) the service of type `T`.
    pub fn with<T: Any + Send + Sync>(mut self, value: T) -> Self {
        Arc::make_mut(&mut self.map).insert(TypeId::of::<T>(), Arc::new(value));
        self
    }

    pub fn get<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.map.get(&TypeId::of::<T>()).and_then(|v| v.downcast_ref())
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

impl fmt::Debug for Services {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Services({})", self.map.len())
    }
}

/// What a call can reach of its session beyond [`ToolCtx`]: the conversation's [`Extensions`]
/// (A's `FileState`, F's todos, E's session rules) and the daemon's [`Services`].
///
/// The loop lends `Conversation::ext` into `ext` for one batch of calls and takes it back when the
/// batch ends. A tool must not keep the `Arc` past its call; a background job that needs session
/// state later reaches it through a service.
#[derive(Clone, Default)]
pub struct SessionScope {
    pub ext: Arc<Mutex<Extensions>>,
    pub services: Services,
}

impl SessionScope {
    /// Empty extensions and no services: for calls made outside a session (the crew today,
    /// tests, [`ToolHost::call`]).
    pub fn detached() -> Self {
        Self::default()
    }

    /// Runs `f` on the lent extensions. A poisoned lock is recovered, so one panicking tool cannot
    /// wedge the session.
    pub fn with_ext<R>(&self, f: impl FnOnce(&mut Extensions) -> R) -> R {
        let mut ext = self.ext.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut ext)
    }
}

impl fmt::Debug for SessionScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionScope").field("services", &self.services).finish_non_exhaustive()
    }
}

/// A call's whole context: [`ToolCtx`] (it derefs to it) plus its [`SessionScope`]. This is what
/// [`ToolHost::call_scoped`], the registration table's call functions and call-level middleware
/// receive.
#[derive(Debug, Clone)]
pub struct CallCtx {
    pub tool: ToolCtx,
    pub scope: SessionScope,
}

impl CallCtx {
    pub fn new(tool: ToolCtx, scope: SessionScope) -> Self {
        Self { tool, scope }
    }

    /// `tool` with a [`SessionScope::detached`] scope.
    pub fn detached(tool: ToolCtx) -> Self {
        Self { tool, scope: SessionScope::detached() }
    }
}

impl Deref for CallCtx {
    type Target = ToolCtx;
    fn deref(&self) -> &ToolCtx {
        &self.tool
    }
}

/// The model class a session's tool set is chosen for: local for a model that runs on this
/// machine (Ollama, GGUF), frontier otherwise.
pub fn exposure_profile(model: &ModelInfo) -> ExposureProfile {
    if model.local || model.provider.is_local() {
        ExposureProfile::Local
    } else {
        ExposureProfile::Frontier
    }
}

/// What a tool call produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    /// Sent to the model as the tool result.
    pub content: String,
    pub is_error: bool,
    /// One dim line for the UI, e.g. `41 lines` or `exit 1`.
    pub summary: String,
    /// Images and other non-text output (a PNG `read`, a screenshot); becomes
    /// `ToolResult::parts`.
    pub parts: Vec<ResultPart>,
}

impl ToolOutcome {
    pub fn ok(content: impl Into<String>, summary: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: false, summary: summary.into(), parts: Vec::new() }
    }

    pub fn error(message: impl Into<String>) -> Self {
        let m = message.into();
        let summary = m.lines().next().unwrap_or("").chars().take(80).collect();
        Self { content: m, is_error: true, summary, parts: Vec::new() }
    }

    /// Adds a non-text part.
    pub fn with_part(mut self, part: ResultPart) -> Self {
        self.parts.push(part);
        self
    }
}

/// What kinds of effect a tool can have. Modes and rules key on these, never on tool names: plan
/// mode blocks `writes` and `runs`; `accept_edits` frees `writes` inside the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Effects {
    pub reads: bool,
    /// Changes files (write, edit, notebook_edit, rename_symbol...).
    pub writes: bool,
    /// Runs code or commands (shells, project commands, services, train_launch).
    pub runs: bool,
    /// Reaches the network beyond localhost.
    pub network: bool,
}

impl Effects {
    pub const READ: Effects = Effects { reads: true, writes: false, runs: false, network: false };
    pub const WRITE: Effects = Effects { reads: true, writes: true, runs: false, network: false };
    pub const RUN: Effects = Effects { reads: true, writes: true, runs: true, network: true };
    pub const NETWORK: Effects = Effects { reads: false, writes: false, runs: false, network: true };
    pub const NONE: Effects = Effects { reads: false, writes: false, runs: false, network: false };
}

/// The static facts about one tool that policy needs (E's rules engine and modes, hook matchers,
/// F's agent-type tool filters, exposure).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolManifest {
    pub group: ToolGroup,
    /// Model classes it is offered to within its group.
    pub profiles: Profiles,
    pub effects: Effects,
    /// Argument names holding a path the call reads or writes (`edit(src/**)` rules match these).
    pub path_args: &'static [&'static str],
}

/// What a call is about to change, asked before it runs so `pre_mutation` middleware can snapshot
/// first.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Mutation {
    /// Changes nothing on disk.
    #[default]
    None,
    /// Changes exactly these files (absolute paths).
    Paths(Vec<PathBuf>),
    /// May change anything under this directory (a shell command's cwd).
    Under(PathBuf),
}

/// What a crew sandbox decides, as the tool table sees it. Implemented by the crew's `Sandbox`
/// (seam-crew); [`ToolHost::consequence_in_sandbox`] takes it so the per-tool crew consequences
/// live beside each tool, not in a match in `marlowe-crew/src/tools.rs`.
pub trait SandboxView: Send + Sync {
    /// The sandbox root (worktree or scratch dir).
    fn root(&self) -> &Path;
    /// Whether writing `path` (as the model gave it) stays inside the sandbox.
    fn allows_write(&self, path: &Path) -> bool;
    /// A shell command's consequence in this sandbox, given the run's taint. The summary names
    /// what tripped, without the tool name.
    fn classify_shell(&self, command: &str, tainted: bool) -> Consequence;
    /// Inline code's consequence (python).
    fn classify_code(&self, code: &str) -> Consequence;
}

/// Whether a call may run without asking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Consequence {
    Free,
    /// Needs approval; `summary` is the one line shown in the approval modal.
    Ask { summary: String },
    /// A fixed approval, asked every time: no session allow, rule, hook or mode frees it (a
    /// secret-shaped file, an off-machine path, a push, a pull request or a comment on one).
    AskEvery { summary: String },
}

impl Consequence {
    /// The approval line, when the call asks.
    pub fn summary(&self) -> Option<&str> {
        match self {
            Consequence::Free => None,
            Consequence::Ask { summary } | Consequence::AskEvery { summary } => Some(summary),
        }
    }

    /// The call needs approval (either kind).
    pub fn asks(&self) -> bool {
        !matches!(self, Consequence::Free)
    }
}

/// What a session has been exposed to. It changes which calls need approval.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Exposure {
    /// Untrusted content (a fetched page, search results, a file from outside the workspace) has
    /// entered the conversation.
    pub tainted: bool,
    /// Network hosts the user allowed in this session.
    pub approved_hosts: HashSet<String>,
}

/// Executes tools. Hosts may be called from several threads at once.
pub trait ToolHost: Send + Sync {
    /// The tools exposed to the model.
    fn specs(&self) -> Vec<ToolSpec>;

    /// Runs one call. Unknown tools and bad args return an error outcome, never panic.
    fn call(&self, call: &ToolCallReq, ctx: &ToolCtx) -> ToolOutcome;

    /// Runs one call with its session scope. The loop calls this one. The default drops the
    /// scope and calls [`ToolHost::call`]; hosts whose tools need session state override it
    /// (`BuiltinTools`), and wrappers forward it.
    fn call_scoped(&self, call: &ToolCallReq, cx: &CallCtx) -> ToolOutcome {
        self.call(call, cx)
    }

    /// Middleware that belongs to this host's tools, built next to their state (A's `FileState`
    /// checks, B's job notices, C's diagnostics). [`crate::Agent::new`] seeds
    /// `Agent::middleware` with it. Wrappers forward it.
    fn middleware(&self) -> Vec<Arc<dyn ToolMiddleware>> {
        Vec::new()
    }

    /// Persona blocks for the tool groups this host exposes (`persona/blocks/<group>.md`), so a
    /// block is in the prompt only when its group is live and exposed. Wrappers forward it.
    fn persona_blocks(&self) -> Vec<PersonaBlock> {
        Vec::new()
    }

    /// The spec the model was shown for tool `name`, or that it could call by name without being
    /// shown (an MCP tool behind `tool_search`). The forgiving layer ([`crate::args`]) validates
    /// calls against it. Wrappers whose specs differ from their inner host's forward it.
    fn tool_spec(&self, name: &str) -> Option<ToolSpec> {
        self.specs().into_iter().find(|s| s.name == name)
    }

    /// What tool `name` needs beyond its schema (aliases, op-dependent requirements); `None`:
    /// the generic rules only. Wrappers forward it.
    fn arg_rules(&self, _name: &str) -> Option<crate::args::ArgRules> {
        None
    }

    /// One-line facts about this host's tools the prompt's tail should carry (the MCP hint).
    /// Wrappers forward and concatenate.
    fn prompt_hints(&self) -> Vec<String> {
        Vec::new()
    }

    /// Whether `call` needs approval.
    fn consequence(&self, call: &ToolCallReq) -> Consequence;

    /// Whether `call` needs approval given what the session has been exposed to. The loop calls
    /// this one; the default ignores exposure.
    fn consequence_exposed(&self, call: &ToolCallReq, _exposure: &Exposure) -> Consequence {
        self.consequence(call)
    }

    /// The text the approval modal shows for `call` when it is not the call's arguments (a file the
    /// tool will write is shown as the file). `None`: the pretty-printed arguments.
    fn approval_detail(&self, _call: &ToolCallReq) -> Option<String> {
        None
    }

    /// Whether running `call` brings untrusted content into the conversation.
    fn taints(&self, _call: &ToolCallReq) -> bool {
        false
    }

    /// The network host `call` reaches, recorded as approved when the user allows the call.
    fn egress_host(&self, _call: &ToolCallReq) -> Option<String> {
        None
    }

    /// Static facts about tool `name`, when this host knows them.
    fn manifest(&self, _name: &str) -> Option<ToolManifest> {
        None
    }

    /// What `call` will change if it runs. The loop calls this before `pre_mutation` middleware.
    fn mutation(&self, _call: &ToolCallReq, _ctx: &ToolCtx) -> Mutation {
        Mutation::None
    }

    /// `call`'s consequence inside a crew sandbox, when the tool defines one; `None` falls back to
    /// [`ToolHost::consequence_exposed`].
    fn consequence_in_sandbox(
        &self,
        _call: &ToolCallReq,
        _exposure: &Exposure,
        _sandbox: &dyn SandboxView,
    ) -> Option<Consequence> {
        None
    }

    /// The one-line description shown when the call starts, e.g. `read src/main.rs`.
    fn describe(&self, call: &ToolCallReq) -> String {
        let first = call
            .args
            .as_object()
            .and_then(|o| o.values().find_map(|v| v.as_str()))
            .unwrap_or("");
        let first: String = first.lines().next().unwrap_or("").chars().take(60).collect();
        if first.is_empty() {
            call.name.clone()
        } else {
            format!("{} {}", call.name, first)
        }
    }
}
