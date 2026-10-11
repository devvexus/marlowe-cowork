//! The crew's shell policy. The rules live in `marlowe-permission` (one adjudication point for
//! conductor and crew shells); this module re-exports them and adapts a [`Sandbox`] to them.
//!
//! Lexical, and meant to over-ask: git writes outside the worktree or to what a worktree shares
//! with the checkout, `git push`, tampering with git's environment, deletes, copies, moves and
//! redirects outside the sandbox, new network hosts. It cannot see what a program does once
//! started, so it is a tripwire on top of the sandbox's cwd, not OS confinement.

use std::path::Path;

use marlowe_agent::{Consequence, SandboxView};
use marlowe_proto::SandboxKind;

pub use marlowe_permission::shell::{classify_code, classify_sandbox_shell, Dialect, Verdict};

use crate::sandbox::{Sandbox, ShellVerdict};

impl Sandbox {
    /// The policy's kind for this sandbox.
    fn policy_kind(&self) -> marlowe_permission::shell::SandboxKind {
        use marlowe_permission::shell::SandboxKind as K;
        match self.kind {
            SandboxKind::Worktree => K::Worktree,
            SandboxKind::Scratch => K::Scratch,
            SandboxKind::None => K::None,
        }
    }

    /// The full verdict on a shell command written in `dialect`: free, ask, or a hard deny (force
    /// push, `--no-verify`, shared git config writes) that no approval lifts. Once the run is
    /// `tainted`, every command asks.
    pub fn shell_verdict(&self, dialect: Dialect, command: &str, tainted: bool) -> Verdict {
        classify_sandbox_shell(dialect, &self.root, self.policy_kind(), &|p| self.allows_write(p), command, tainted)
    }

    /// Whether a bash command may run free. A hard deny reads as an ask here; callers that must
    /// refuse it use [`Sandbox::shell_verdict`].
    pub fn classify_shell(&self, command: &str, tainted: bool) -> ShellVerdict {
        self.classify_shell_in(Dialect::Bash, command, tainted)
    }

    /// [`Sandbox::classify_shell`] for a command in `dialect`.
    pub fn classify_shell_in(&self, dialect: Dialect, command: &str, tainted: bool) -> ShellVerdict {
        match self.shell_verdict(dialect, command, tainted) {
            Verdict::Free => ShellVerdict::Free,
            Verdict::Ask(why) => ShellVerdict::Ask(why),
            Verdict::Deny(why) => ShellVerdict::Ask(format!("refused: {why}")),
        }
    }

    /// Whether code (python, or a script a shell runs) may run free in an untainted run. Asks when
    /// it mentions pushing with git or git's environment. Lexical.
    pub fn classify_code(&self, code: &str) -> ShellVerdict {
        match classify_code(code) {
            Verdict::Free => ShellVerdict::Free,
            Verdict::Ask(why) | Verdict::Deny(why) => ShellVerdict::Ask(why),
        }
    }
}

impl SandboxView for Sandbox {
    fn root(&self) -> &Path {
        &self.root
    }

    fn allows_write(&self, path: &Path) -> bool {
        Sandbox::allows_write(self, path)
    }

    /// A hard deny becomes a fixed approval here (the tool table has no deny); the crew's tool
    /// host refuses the call outright before it gets this far.
    fn classify_shell(&self, command: &str, tainted: bool) -> Consequence {
        match self.shell_verdict(Dialect::Bash, command, tainted) {
            Verdict::Free => Consequence::Free,
            Verdict::Ask(summary) => Consequence::Ask { summary },
            Verdict::Deny(why) => Consequence::AskEvery { summary: format!("refused: {why}") },
        }
    }

    fn classify_code(&self, code: &str) -> Consequence {
        match classify_code(code) {
            Verdict::Free => Consequence::Free,
            Verdict::Ask(summary) | Verdict::Deny(summary) => Consequence::Ask { summary },
        }
    }
}
