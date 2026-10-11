//! A crew run's tool host: the injected built-ins rooted in the sandbox, cut to the role's
//! allowlist, with the sandbox's shell policy on top. Never exposes `dispatch`.
//!
//! This file is a thin wrapper. Each tool's crew consequence lives beside the tool in the
//! registration table (`ToolHost::consequence_in_sandbox`); only the shells are decided here,
//! because their dialect comes from the tool's name and a hard deny refuses the call outright.

use std::path::Path;
use std::sync::Arc;

use marlowe_agent::{
    CallCtx, Consequence, Exposure, Mutation, PersonaBlock, ToolCallReq, ToolCtx, ToolHost, ToolManifest, ToolMiddleware,
    ToolOutcome,
};
use marlowe_model::ToolSpec;
use marlowe_permission::shell::Verdict;

use crate::sandbox::Sandbox;
use crate::shell_policy::Dialect;

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

impl CrewTools {
    /// The refusal for a call this host will not run: off the allowlist, or a shell command the
    /// policy denies in every mode (force push, `--no-verify`, shared git config writes).
    fn refusal(&self, call: &ToolCallReq) -> Option<ToolOutcome> {
        if !self.allow.contains(&call.name) {
            return Some(ToolOutcome::error(format!("`{}` is not on this role's tool list", call.name)));
        }
        let dialect = Dialect::of_tool(&call.name)?;
        let command = call.args.get("command").and_then(|v| v.as_str())?;
        // Untainted on purpose: a denial holds whatever the run has read.
        match self.sandbox.shell_verdict(dialect, command, false) {
            Verdict::Deny(why) => Some(ToolOutcome::error(format!("refused: {why}. No approval lifts this."))),
            _ => None,
        }
    }
}

impl ToolHost for CrewTools {
    /// The inner host's specs whose names are on the allowlist.
    fn specs(&self) -> Vec<ToolSpec> {
        self.inner.specs().into_iter().filter(|s| self.allow.contains(&s.name)).collect()
    }

    /// Refuses tools off the allowlist and denied shell commands; otherwise delegates.
    fn call(&self, call: &ToolCallReq, ctx: &ToolCtx) -> ToolOutcome {
        match self.refusal(call) {
            Some(out) => out,
            None => self.inner.call(call, ctx),
        }
    }

    /// [`ToolHost::call`] with the loop's session scope, which the inner host's tools need
    /// (jobs, file state, the todo list).
    fn call_scoped(&self, call: &ToolCallReq, cx: &CallCtx) -> ToolOutcome {
        match self.refusal(call) {
            Some(out) => out,
            None => self.inner.call_scoped(call, cx),
        }
    }

    fn middleware(&self) -> Vec<Arc<dyn ToolMiddleware>> {
        self.inner.middleware()
    }

    fn persona_blocks(&self) -> Vec<PersonaBlock> {
        self.inner.persona_blocks()
    }

    fn consequence(&self, call: &ToolCallReq) -> Consequence {
        self.consequence_exposed(call, &Exposure::default())
    }

    /// Shells go through the sandbox's policy in the tool's dialect with the run's taint; every
    /// other tool answers from its table entry ([`ToolHost::consequence_in_sandbox`]), or from
    /// the inner host's plain consequence when it defines no crew one.
    fn consequence_exposed(&self, call: &ToolCallReq, exposure: &Exposure) -> Consequence {
        if let Some(dialect) = Dialect::of_tool(&call.name) {
            let command = call.args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            return match self.sandbox.shell_verdict(dialect, command, exposure.tainted) {
                Verdict::Free => Consequence::Free,
                Verdict::Ask(why) => Consequence::Ask { summary: format!("{}: {why}", call.name) },
                Verdict::Deny(why) => Consequence::AskEvery { summary: format!("{}: refused: {why}", call.name) },
            };
        }
        self.inner
            .consequence_in_sandbox(call, exposure, &self.sandbox)
            .unwrap_or_else(|| self.inner.consequence_exposed(call, exposure))
    }

    fn consequence_in_sandbox(
        &self,
        call: &ToolCallReq,
        exposure: &Exposure,
        _sandbox: &dyn marlowe_agent::SandboxView,
    ) -> Option<Consequence> {
        // The run's own sandbox decides; a caller's view of another one does not apply.
        Some(self.consequence_exposed(call, exposure))
    }

    fn taints(&self, call: &ToolCallReq) -> bool {
        self.inner.taints(call)
    }

    fn egress_host(&self, call: &ToolCallReq) -> Option<String> {
        self.inner.egress_host(call)
    }

    fn manifest(&self, name: &str) -> Option<ToolManifest> {
        self.allow.iter().any(|t| t == name).then(|| self.inner.manifest(name)).flatten()
    }

    fn mutation(&self, call: &ToolCallReq, ctx: &ToolCtx) -> Mutation {
        self.inner.mutation(call, ctx)
    }

    fn describe(&self, call: &ToolCallReq) -> String {
        self.inner.describe(call)
    }
}
