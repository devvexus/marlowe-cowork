//! The git group (track J): git{op} and gh{op}, CI included (`marlowe-exec/src/git/`). Reads are
//! free, writes ask in default mode and are free in a crew worktree; force pushes, hook skips and
//! protected-branch deletes are refused in every mode by the group's middleware
//! (`git::guard`). The `gh` child gets `GH_TOKEN` only through `ChildSpec::env_allow`. Push and PR
//! creation go through the daemon (`Request::OpenPr`) for a crew run, never from a crew shell.
//!
//! The tools are offered only on a host `configure`d with the profile home (`GitState`), and `gh`
//! only when the GitHub CLI exists (or a test points the state at a fake).

use std::sync::Arc;

use marlowe_agent::{Effects, ToolGroup, ToolMiddleware};
use super::ToolDef;
use crate::git::{self, GitState};
use crate::BuiltinTools;

pub fn tools() -> Vec<ToolDef> {
    let git_effects = Effects { reads: true, writes: true, runs: false, network: true };
    let gh_effects = Effects { reads: true, writes: true, runs: false, network: true };
    vec![
        ToolDef::new(git_spec(), ToolGroup::Git, git_effects, git::git_call)
            .path_args(&["cwd"])
            .consequence(git::git_consequence)
            .crew_consequence(git::git_crew_consequence)
            .describe(git::git_describe)
            .mutation(git::git_mutation)
            .enabled(|t| t.state::<GitState>().is_configured()),
        ToolDef::new(gh_spec(), ToolGroup::Git, gh_effects, git::gh_call)
            .path_args(&["cwd"])
            .consequence(git::gh_consequence)
            .describe(git::gh_describe)
            .taints(git::gh_taints)
            .egress(|_, _| Some(git::gh_host()))
            .enabled(gh_ready),
    ]
}

fn gh_ready(t: &BuiltinTools) -> bool {
    let s = t.state::<GitState>();
    s.is_configured() && s.has_gh()
}

fn git_spec() -> marlowe_model::ToolSpec {
    marlowe_agent::compact_specs::spec("git")
}

fn gh_spec() -> marlowe_model::ToolSpec {
    marlowe_agent::compact_specs::spec("gh")
}

/// Loop middleware for this group, built once per host next to its state (see `specs/mod.rs`).
pub fn middleware(tools: &BuiltinTools) -> Vec<Arc<dyn ToolMiddleware>> {
    vec![Arc::new(git::guard::GitGuard::new(git::Env::of(tools)))]
}

/// This group's `persona/blocks/<group>.md`, appended only while the group is exposed and has a
/// live tool.
pub fn persona_block() -> Option<&'static str> {
    Some(include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../persona/blocks/git.md")))
}

/// Per-host setup from the profile home: marks the group live and records where the CI ledger is.
pub fn configure(tools: BuiltinTools, setup: &super::HostSetup<'_>) -> BuiltinTools {
    tools.with_state(GitState::new(Some(setup.home.to_path_buf())))
}

/// Whether a local model gets this group in `root`: a git repository.
pub fn detect(root: Option<&std::path::Path>) -> bool {
    root.is_some_and(git::is_git_repo)
}
