//! The `git{op}`, `gh{op}` and `ci_status` tools (design 3.9). Track J.
//!
//! | File | What |
//! |---|---|
//! | `policy.rs` | what is never allowed: force in any spelling, hook skips, protected-branch deletes; secret-shaped paths and diffs |
//! | `run.rs` | [`Env`]: the host's git and `gh` runner (workspace port for git, local scrubbed child for `gh`) |
//! | `git_tool.rs` | `git{op}`: status, diff, log, show, blame, branch, switch, add, commit, stash, push, fetch |
//! | `gh_tool.rs`, `pr.rs`, `threads.rs` | `gh{op}`: the PR plan and its one approval, view/list/diff/checks, review threads, comments and reviews |
//! | `ci.rs`, `watch.rs` | `ci_status` and the ledger of pushed branches the Sentinel's CI probe reads |
//! | `guard.rs` | the middleware: hard denials in every mode, and defaults filled in before the approval modal |
//! | `prompts.rs` | the texts behind `/pr-comments` and `/review` |
//!
//! Rules this track keeps: a push or a PR is one approval that shows remote, branch, title and
//! body; `gh` gets `GH_TOKEN` only through `ChildSpec::env_allow`; pushing and PRs never happen
//! from a crew shell (the crew's no-push environment is inherited, and `gh` is not on a crew
//! allowlist); every child starts through `marlowe_os`.

pub(crate) mod ci;
mod gh_tool;
mod git_tool;
pub(crate) mod guard;
mod pr;
pub mod policy;
pub mod prompts;
mod run;
mod threads;
pub mod watch;

use std::path::{Path, PathBuf};

pub(crate) use gh_tool::{describe as gh_describe, gh_call, gh_consequence, gh_taints, host as gh_host};
pub(crate) use git_tool::{describe as git_describe, git_call, git_consequence, git_crew_consequence, git_mutation};
pub(crate) use run::Env;

/// The hard denials for shell command lines, without the git group: for a host to install next to
/// its `bash` / `powershell` whether or not `git` is offered (same name as the group's own guard,
/// so a host with both runs one).
pub fn shell_guard() -> std::sync::Arc<dyn marlowe_agent::ToolMiddleware> {
    std::sync::Arc::new(guard::ShellGuard)
}

/// The git group's per-host configuration, installed by `specs::git::configure`. A host without
/// it (a bare `BuiltinTools::new`) does not offer the group's tools at all.
#[derive(Debug, Clone, Default)]
pub struct GitState {
    configured: bool,
    /// The profile home: where the CI ledger lives (`ci_watch.json`).
    home: Option<PathBuf>,
    /// The `gh` command: program first, then any leading arguments (tests point it at a fake).
    gh: Option<Vec<String>>,
}

impl GitState {
    /// A configured host: `gh` is whatever is on PATH (or in its default install folder).
    pub fn new(home: Option<PathBuf>) -> Self {
        Self { configured: true, home, gh: find_gh().map(|p| vec![p]) }
    }

    /// Uses this `gh` command (program, then leading arguments) instead of the one on PATH.
    pub fn with_gh(mut self, command: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let cmd: Vec<String> = command.into_iter().map(Into::into).collect();
        self.gh = (!cmd.is_empty()).then_some(cmd);
        self
    }

    /// No `gh` at all (the `gh` and `ci_status` tools are then not offered).
    pub fn without_gh(mut self) -> Self {
        self.gh = None;
        self
    }

    pub fn is_configured(&self) -> bool {
        self.configured
    }

    pub fn has_gh(&self) -> bool {
        self.gh.is_some()
    }

    pub fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }
}

/// `gh` on PATH, else GitHub CLI's default install folder.
fn find_gh() -> Option<String> {
    let names: &[&str] = if cfg!(windows) { &["gh.exe"] } else { &["gh"] };
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            for n in names {
                let p = dir.join(n);
                if p.is_file() {
                    return Some(p.to_string_lossy().into_owned());
                }
            }
        }
    }
    #[cfg(windows)]
    for root in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(root) {
            for rel in ["GitHub CLI\\gh.exe", "Programs\\GitHub CLI\\gh.exe"] {
                let p = Path::new(&base).join(rel);
                if p.is_file() {
                    return Some(p.to_string_lossy().into_owned());
                }
            }
        }
    }
    None
}

/// Whether `dir` is inside a git work tree (a cheap look for `.git` up the path).
pub fn is_git_repo(dir: &Path) -> bool {
    dir.ancestors().any(|d| d.join(".git").exists())
}
