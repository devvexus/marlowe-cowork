//! A crew run's tool host: the injected built-ins rooted in the sandbox, cut to the role's
//! allowlist, with the sandbox's shell policy on top. Never exposes `dispatch`.

use std::path::Path;
use std::sync::Arc;

use marlowe_agent::{Consequence, Exposure, ToolCallReq, ToolCtx, ToolHost, ToolOutcome};
use marlowe_model::ToolSpec;

use crate::run::clip;
use crate::sandbox::{Sandbox, ShellVerdict};

/// Builds the inner host for a run: file tools rooted at `sandbox.root` (reads also allowed in
/// `workspace`, read-only), shells with cwd = sandbox, the sandbox's child environment, and every
/// child process in a kill-on-close Job Object. Injected by the binary (marlowe-exec).
pub type CrewToolFactory = Arc<dyn Fn(&Sandbox, Option<&Path>) -> Arc<dyn ToolHost> + Send + Sync>;

/// The host a crew agent sees.
pub struct CrewTools {
    inner: Arc<dyn ToolHost>,
    allow: Vec<String>,
    sandbox: Sandbox,
}

impl CrewTools {
    /// `allow` is the role's allowlist, optionally narrowed by the playbook step. `dispatch` is
    /// dropped from it whatever it says.
    pub fn new(inner: Arc<dyn ToolHost>, allow: Vec<String>, sandbox: Sandbox) -> CrewTools {
        let allow = allow.into_iter().filter(|t| t != crate::DISPATCH_TOOL).collect();
        CrewTools { inner, allow, sandbox }
    }

    /// The tool names this host exposes.
    pub fn allowed(&self) -> &[String] {
        &self.allow
    }

    pub fn sandbox(&self) -> &Sandbox {
        &self.sandbox
    }
}

impl ToolHost for CrewTools {
    /// The inner host's specs whose names are on the allowlist.
    fn specs(&self) -> Vec<ToolSpec> {
        self.inner.specs().into_iter().filter(|s| self.allow.contains(&s.name)).collect()
    }

    /// Refuses tools off the allowlist; otherwise delegates.
    fn call(&self, call: &ToolCallReq, ctx: &ToolCtx) -> ToolOutcome {
        if !self.allow.contains(&call.name) {
            return ToolOutcome::error(format!("`{}` is not on this role's tool list", call.name));
        }
        self.inner.call(call, ctx)
    }

    fn consequence(&self, call: &ToolCallReq) -> Consequence {
        self.consequence_exposed(call, &Exposure::default())
    }

    /// Writes inside the sandbox are free; shells go through [`Sandbox::classify_shell`] with
    /// the run's taint, python through [`Sandbox::classify_code`]; everything else follows the
    /// inner host.
    fn consequence_exposed(&self, call: &ToolCallReq, exposure: &Exposure) -> Consequence {
        let arg = |k: &str| call.args.get(k).and_then(|v| v.as_str());
        match call.name.as_str() {
            "write" | "edit" => match arg("path") {
                Some(p) if self.sandbox.allows_write(Path::new(p)) => Consequence::Free,
                Some(p) => Consequence::Ask { summary: format!("{} {} (outside the sandbox)", call.name, clip(p, 100)) },
                None => Consequence::Free, // refused by the inner host for the missing argument
            },
            "bash" | "powershell" => match self.sandbox.classify_shell(arg("command").unwrap_or(""), exposure.tainted) {
                ShellVerdict::Free => Consequence::Free,
                ShellVerdict::Ask(summary) => Consequence::Ask { summary: format!("{}: {summary}", call.name) },
            },
            // Code runs in the sandbox like a shell; after untrusted content it asks.
            "python" if !exposure.tainted => {
                let code = match (arg("code"), arg("path")) {
                    (Some(code), _) => Some(code.to_string()),
                    (None, Some(p)) => std::fs::read(self.sandbox.root.join(p))
                        .ok()
                        .filter(|b| b.len() <= 512 * 1024)
                        .map(|b| String::from_utf8_lossy(&b).into_owned()),
                    _ => None,
                };
                match code.map(|c| self.sandbox.classify_code(&c)) {
                    Some(ShellVerdict::Ask(why)) => Consequence::Ask { summary: format!("python: {why}") },
                    _ => Consequence::Free,
                }
            }
            _ => self.inner.consequence_exposed(call, exposure),
        }
    }

    fn taints(&self, call: &ToolCallReq) -> bool {
        self.inner.taints(call)
    }

    fn egress_host(&self, call: &ToolCallReq) -> Option<String> {
        self.inner.egress_host(call)
    }

    fn describe(&self, call: &ToolCallReq) -> String {
        self.inner.describe(call)
    }
}
