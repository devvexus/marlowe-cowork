//! `enter_worktree` / `exit_worktree` (design 2.2 A13): the conductor moves its session into a
//! git worktree from the [`Worktrees`] service and back. The tools record the switch in
//! `Conversation::ext`; [`WorktreeMiddleware`] applies it at the step boundary (the session's
//! workspace and instruction roots move), so the next batch of calls runs in the new directory.
//! Leaving keeps the branch; `keep: true` also keeps the directory.

use std::path::PathBuf;
use std::sync::Arc;

use marlowe_model::Message;
pub use marlowe_os::workspace::{NoWorktrees, WorktreeError, WorktreeHandle, WorktreeProvider, WorktreeRequest};
use serde_json::Value;

use crate::convo::Conversation;
use crate::instructions::{self, InstructionState};
use crate::middleware::{StepInfo, ToolMiddleware};
use crate::tools::{CallCtx, ToolOutcome};

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

/// `exit_worktree{keep}`.
pub fn exit_call(args: &Value, cx: &CallCtx) -> ToolOutcome {
    let Some(provider) = cx.scope.services.get::<Worktrees>() else {
        return ToolOutcome::error("worktrees are not available in this session");
    };
    let keep_dir = args.get("keep").and_then(Value::as_bool).unwrap_or(false);
    let state = cx.scope.with_ext(|e| e.get::<WorktreeState>().cloned()).unwrap_or_default();
    let (Some(h), Some(original)) = (state.handle.clone(), state.original.clone()) else {
        return ToolOutcome::error("not in a worktree");
    };
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
