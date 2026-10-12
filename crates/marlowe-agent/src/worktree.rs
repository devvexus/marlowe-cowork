//! `enter_worktree` / `exit_worktree` (design 2.2 A13): the conductor moves its session into a
//! git worktree from the [`Worktrees`] service and back. The tools record the switch in
//! `Conversation::ext`; [`WorktreeMiddleware`] applies it at the step boundary (the session's
//! workspace and instruction roots move), so the next batch of calls runs in the new directory.
//! Leaving keeps the branch; `keep: true` also keeps the directory, and a worktree with
//! uncommitted or untracked changes is only removed with `discard_changes: true` (which asks).
//!
//! The session's tool host is built once, rooted at the original workspace, and `BuiltinTools`
//! resolves paths against that root rather than the call's workspace. So the loop runs each
//! step's calls on [`step_host`]: with the daemon's [`WorkspaceHosts`] installed, the session's
//! own tools are rebuilt for the worktree, and an absolute path under an `/add-dir` root goes to
//! tools rooted there (design 2.4 C12: the added root is workspace, not outside it).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use marlowe_model::{Message, ToolSpec};
pub use marlowe_os::workspace::{NoWorktrees, WorktreeError, WorktreeHandle, WorktreeProvider, WorktreeRequest};
use serde_json::Value;

use crate::convo::Conversation;
use crate::instructions::{self, InstructionState, WorkspaceRoots};
use crate::middleware::{StepInfo, ToolMiddleware};
use crate::system::PersonaBlock;
use crate::tools::{
    CallCtx, Consequence, Exposure, Mutation, SandboxView, Services, ToolCallReq, ToolCtx, ToolHost, ToolManifest, ToolOutcome,
};

/// The worktree service in `Services`.
#[derive(Clone)]
pub struct Worktrees(pub Arc<dyn WorktreeProvider>);

/// Where the session is working.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeState {
    /// The workspace before `enter_worktree`.
    pub original: Option<PathBuf>,
    pub handle: Option<WorktreeHandle>,
    /// A workspace switch the next step boundary applies.
    pub switch_to: Option<PathBuf>,
}

/// `enter_worktree{name}`.
pub fn enter_call(args: &Value, cx: &CallCtx) -> ToolOutcome {
    let Some(provider) = cx.scope.services.get::<Worktrees>() else {
        return ToolOutcome::error("worktrees are not available in this session");
    };
    let Some(repo) = cx.workspace.clone() else {
        return ToolOutcome::error("enter_worktree needs a workspace that is a git repository");
    };
    if cx.scope.with_ext(|e| e.get::<WorktreeState>().is_some_and(|s| s.handle.is_some())) {
        return ToolOutcome::error("already in a worktree; exit_worktree first");
    }
    let name = args.get("name").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
        .unwrap_or_else(|| format!("session-{}", cx.session.as_str().chars().take(8).collect::<String>()));
    match provider.0.create(&WorktreeRequest { repo: repo.clone(), name, base: None, parent: None }) {
        Ok(h) => {
            let msg = format!(
                "Working in worktree {} on branch {} (from {}). Relative paths and commands now run there; exit_worktree \
                 returns to {}.",
                h.root.display(),
                h.branch,
                h.base,
                repo.display()
            );
            let summary = format!("worktree {}", h.branch);
            cx.scope.with_ext(|e| {
                *e.get_or_default::<WorktreeState>() = WorktreeState { original: Some(repo), switch_to: Some(h.root.clone()), handle: Some(h) };
            });
            ToolOutcome::ok(msg, summary)
        }
        Err(e) => ToolOutcome::error(format!("could not create the worktree: {e}")),
    }
}

/// The files `git status --porcelain` reports in `root` (uncommitted and untracked). A missing
/// directory has none; a directory git cannot read is an error, never "clean".
pub fn uncommitted(root: &Path) -> Result<Vec<String>, String> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut c = std::process::Command::new("git");
    c.args(["status", "--porcelain", "--untracked-files=all"]);
    let spec = marlowe_os::child::ChildSpec::new().with_creation_flags(marlowe_os::child::CREATE_NO_WINDOW).with_cwd(root);
    let limits = marlowe_os::child::RunLimits { timeout: Duration::from_secs(30), max_output_bytes: 256 * 1024 };
    let out = marlowe_os::child::run(c, &spec, limits, &|| false).map_err(|e| e.to_string())?;
    if out.timed_out || out.code() != 0 {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("git status failed: {}", err.lines().next().unwrap_or("timed out").trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.trim().is_empty()).map(|l| l.trim_end().to_string()).collect())
}

/// `exit_worktree{keep, discard_changes}`.
pub fn exit_call(args: &Value, cx: &CallCtx) -> ToolOutcome {
    let Some(provider) = cx.scope.services.get::<Worktrees>() else {
        return ToolOutcome::error("worktrees are not available in this session");
    };
    let keep_dir = args.get("keep").and_then(Value::as_bool).unwrap_or(false);
    let discard = args.get("discard_changes").and_then(Value::as_bool).unwrap_or(false);
    let state = cx.scope.with_ext(|e| e.get::<WorktreeState>().cloned()).unwrap_or_default();
    let (Some(h), Some(original)) = (state.handle.clone(), state.original.clone()) else {
        return ToolOutcome::error("not in a worktree");
    };
    if !keep_dir && !discard {
        // As Claude Code's ExitWorktree: uncommitted work is never removed unless asked to.
        let refuse = |why: String| {
            ToolOutcome::error(format!(
                "{why}\nThe worktree was not removed and you are still in it. Commit what you want to keep, then call \
                 exit_worktree again; or pass keep: true to leave the directory in place; or discard_changes: true to \
                 delete the changes (this asks the user)."
            ))
        };
        match uncommitted(&h.root) {
            Ok(files) if files.is_empty() => {}
            Ok(files) => {
                let n = files.len();
                let shown: Vec<&str> = files.iter().take(20).map(String::as_str).collect();
                let more = if n > shown.len() { format!("\n... and {} more", n - shown.len()) } else { String::new() };
                return refuse(format!("{} has {n} uncommitted or untracked file(s):\n{}{more}", h.root.display(), shown.join("\n")));
            }
            Err(e) => return refuse(format!("could not check {} for uncommitted changes: {e}", h.root.display())),
        }
    }
    if !keep_dir {
        if let Err(e) = provider.0.remove(&h, true) {
            return ToolOutcome::error(format!("could not remove the worktree: {e}"));
        }
    }
    cx.scope.with_ext(|e| {
        *e.get_or_default::<WorktreeState>() = WorktreeState { original: None, handle: None, switch_to: Some(original.clone()) };
    });
    let kept = if keep_dir { format!("the worktree stays at {}", h.root.display()) } else { "the worktree directory was removed".into() };
    ToolOutcome::ok(format!("Back in {}. Branch {} is kept; {kept}.", original.display(), h.branch), format!("left {}", h.branch))
}

/// `exit_worktree`'s consequence: discarding uncommitted work asks.
pub fn exit_consequence(call: &ToolCallReq) -> Consequence {
    let flag = |k: &str| call.args.get(k).and_then(Value::as_bool).unwrap_or(false);
    if flag("discard_changes") && !flag("keep") {
        Consequence::Ask { summary: "remove the worktree and discard its uncommitted changes".into() }
    } else {
        Consequence::Free
    }
}

/// Applies a recorded workspace switch at the step boundary.
#[derive(Debug, Default)]
pub struct WorktreeMiddleware;

impl WorktreeMiddleware {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

/// Moves `convo` to `dir`: its workspace and its instruction roots.
pub fn switch_workspace(convo: &mut Conversation, dir: PathBuf) {
    let old = convo.workspace.replace(dir.clone());
    if let Some(st) = convo.ext.get_mut::<InstructionState>() {
        match (old, st.sources.roots.first_mut()) {
            (Some(o), Some(first)) if *first == o => *first = dir.clone(),
            _ => st.sources.roots.insert(0, dir.clone()),
        }
        st.sources.cwd = Some(dir);
        instructions::refresh(convo);
    }
}

impl ToolMiddleware for WorktreeMiddleware {
    fn name(&self) -> &str {
        "worktree"
    }

    fn after_step(&self, _info: StepInfo, convo: &mut Conversation) -> Vec<Message> {
        if let Some(dir) = convo.ext.get_mut::<WorktreeState>().and_then(|s| s.switch_to.take()) {
            switch_workspace(convo, dir);
        }
        Vec::new()
    }
}

// ---------------------------------------------------------------- tools for another root

/// One root's tools and the names they answer.
#[derive(Clone)]
struct Rooted {
    host: Arc<dyn ToolHost>,
    names: Arc<HashSet<String>>,
}

/// Builds the session's own tools for another root (the daemon installs it with the factory
/// that built the session's tools). Each root is built once per session.
#[derive(Clone)]
pub struct WorkspaceHosts {
    build: Arc<dyn Fn(&Path) -> Arc<dyn ToolHost> + Send + Sync>,
    built: Arc<Mutex<HashMap<PathBuf, Rooted>>>,
}

impl WorkspaceHosts {
    pub fn new(build: impl Fn(&Path) -> Arc<dyn ToolHost> + Send + Sync + 'static) -> Self {
        Self { build: Arc::new(build), built: Arc::default() }
    }

    fn rooted(&self, root: &Path) -> Rooted {
        let mut built = self.built.lock().unwrap_or_else(|p| p.into_inner());
        built
            .entry(root.to_path_buf())
            .or_insert_with(|| {
                let host = (self.build)(root);
                let names = host.specs().into_iter().map(|s| s.name).collect();
                Rooted { host, names: Arc::new(names) }
            })
            .clone()
    }
}

/// The host one step's calls run on: `base`, unless the session moved into a worktree or added
/// roots and the daemon's [`WorkspaceHosts`] can build tools rooted there.
pub fn step_host(base: &Arc<dyn ToolHost>, services: &Services, convo: &Conversation) -> Arc<dyn ToolHost> {
    let Some(hosts) = services.get::<WorkspaceHosts>() else { return base.clone() };
    let moved = convo
        .ext
        .get::<WorktreeState>()
        .and_then(|s| s.handle.as_ref())
        .map(|h| h.root.clone())
        .filter(|r| convo.workspace.as_deref() == Some(r.as_path()));
    let extra = convo.ext.get::<WorkspaceRoots>().map(|r| r.extra.clone()).unwrap_or_default();
    if moved.is_none() && extra.is_empty() {
        return base.clone();
    }
    Arc::new(Rerooted {
        base: base.clone(),
        primary: moved.map(|r| hosts.rooted(&r)),
        extra: extra
            .into_iter()
            .map(|r| {
                let h = hosts.rooted(&r);
                (r, h)
            })
            .collect(),
    })
}

/// The session's host with calls routed by root: an absolute path under an added root goes to
/// that root's tools; anything else the worktree's tools offer goes there; the rest (memory,
/// MCP, dispatch) stays on the session's host. The model sees the session's tool list unchanged.
struct Rerooted {
    base: Arc<dyn ToolHost>,
    primary: Option<Rooted>,
    extra: Vec<(PathBuf, Rooted)>,
}

impl Rerooted {
    fn pick(&self, call: &ToolCallReq) -> &dyn ToolHost {
        let raw = instructions::path_values(&call.args);
        if !raw.is_empty() && raw.iter().all(|p| Path::new(p).is_absolute()) {
            let paths: Vec<PathBuf> = raw.iter().map(|p| instructions::normalize(Path::new(p))).collect();
            let hit = self
                .extra
                .iter()
                .find(|(root, r)| r.names.contains(&call.name) && paths.iter().all(|p| instructions::inside(root, p)));
            if let Some((_, r)) = hit {
                return r.host.as_ref();
            }
        }
        match &self.primary {
            Some(r) if r.names.contains(&call.name) => r.host.as_ref(),
            _ => self.base.as_ref(),
        }
    }
}

impl ToolHost for Rerooted {
    fn specs(&self) -> Vec<ToolSpec> {
        self.base.specs()
    }
    fn call(&self, call: &ToolCallReq, ctx: &ToolCtx) -> ToolOutcome {
        self.pick(call).call(call, ctx)
    }
    fn call_scoped(&self, call: &ToolCallReq, cx: &CallCtx) -> ToolOutcome {
        self.pick(call).call_scoped(call, cx)
    }
    fn middleware(&self) -> Vec<Arc<dyn ToolMiddleware>> {
        self.base.middleware()
    }
    fn persona_blocks(&self) -> Vec<PersonaBlock> {
        self.base.persona_blocks()
    }
    fn consequence(&self, call: &ToolCallReq) -> Consequence {
        self.pick(call).consequence(call)
    }
    fn consequence_exposed(&self, call: &ToolCallReq, exposure: &Exposure) -> Consequence {
        self.pick(call).consequence_exposed(call, exposure)
    }
    fn approval_detail(&self, call: &ToolCallReq) -> Option<String> {
        self.pick(call).approval_detail(call)
    }
    fn taints(&self, call: &ToolCallReq) -> bool {
        self.pick(call).taints(call)
    }
    fn egress_host(&self, call: &ToolCallReq) -> Option<String> {
        self.pick(call).egress_host(call)
    }
    fn manifest(&self, name: &str) -> Option<ToolManifest> {
        self.base.manifest(name)
    }
    fn mutation(&self, call: &ToolCallReq, ctx: &ToolCtx) -> Mutation {
        self.pick(call).mutation(call, ctx)
    }
    fn consequence_in_sandbox(&self, call: &ToolCallReq, exposure: &Exposure, sandbox: &dyn SandboxView) -> Option<Consequence> {
        self.pick(call).consequence_in_sandbox(call, exposure, sandbox)
    }
    fn describe(&self, call: &ToolCallReq) -> String {
        self.pick(call).describe(call)
    }
}
