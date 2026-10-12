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

/// The run's taint as it stands now (a message can reach the run's inbox mid-step).
pub type LiveTaint = Arc<dyn Fn() -> bool + Send + Sync>;

/// The host a crew agent sees.
pub struct CrewTools {
    inner: Arc<dyn ToolHost>,
    allow: Vec<String>,
    sandbox: Sandbox,
    live_taint: Option<LiveTaint>,
}

impl CrewTools {
    /// `allow` is the role's allowlist, optionally narrowed by the playbook step. `dispatch` is
    /// dropped from it whatever it says.
    pub fn new(inner: Arc<dyn ToolHost>, allow: Vec<String>, sandbox: Sandbox) -> CrewTools {
        let allow = allow.into_iter().filter(|t| t != crate::DISPATCH_TOOL).collect();
        CrewTools { inner, allow, sandbox, live_taint: None }
    }

    /// Classifies every call with the run's live taint ORed into the step's exposure, so a
    /// shell after untrusted content arrived mid-step gets the tainted policy.
    pub fn with_live_taint(mut self, taint: LiveTaint) -> CrewTools {
        self.live_taint = Some(taint);
        self
    }

    /// The tool names this host exposes.
    pub fn allowed(&self) -> &[String] {
        &self.allow
    }

    pub fn sandbox(&self) -> &Sandbox {
        &self.sandbox
    }
}

/// Argument names file tools give their target in, for tools that declare no mutation.
const PATH_ARGS: [&str; 4] = ["path", "file_path", "notebook_path", "file"];

/// Tools that write the file their path argument names.
const WRITERS: [&str; 6] = ["write", "edit", "multi_edit", "edit_lines", "notebook_edit", "apply_patch"];

impl CrewTools {
    /// The frozen hub file a file-writing call targets, if any: by the paths the tool says it
    /// changes, else by its path argument when it is a writer. Shells are not read here; the
    /// build puts back hub files a shell changed and tells the engineer.
    fn frozen_target(&self, call: &ToolCallReq, ctx: &ToolCtx) -> Option<String> {
        if self.sandbox.frozen.is_empty() || Dialect::of_tool(&call.name).is_some() {
            return None;
        }
        let by_arg = || {
            PATH_ARGS
                .iter()
                .filter_map(|k| call.args.get(*k).and_then(|v| v.as_str()))
                .find_map(|p| self.sandbox.frozen_path(Path::new(p)))
        };
        match self.inner.mutation(call, ctx) {
            Mutation::Paths(paths) => paths.iter().find_map(|p| self.sandbox.frozen_path(p)),
            Mutation::Under(_) => None,
            Mutation::None if WRITERS.contains(&call.name.as_str()) => by_arg(),
            Mutation::None => None,
        }
    }

    /// The refusal for a call this host will not run: off the allowlist, a write to a frozen hub
    /// file, or a shell command the policy denies in every mode (force push, `--no-verify`,
    /// shared git config writes).
    fn refusal(&self, call: &ToolCallReq, ctx: &ToolCtx) -> Option<ToolOutcome> {
        if !self.allow.contains(&call.name) {
            return Some(ToolOutcome::error(format!("`{}` is not on this role's tool list", call.name)));
        }
        if let Some(hub) = self.frozen_target(call, ctx) {
            return Some(ToolOutcome::error(format!(
                "`{hub}` is a hub file, frozen after the build's contract stage. Do not change it; end your reply with \
                 `NEED_HUB_CHANGE: {hub}: <what and why>` and the lead will make the change."
            )));
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
        match self.refusal(call, ctx) {
            Some(out) => out,
            None => self.inner.call(call, ctx),
        }
    }

    /// [`ToolHost::call`] with the loop's session scope, which the inner host's tools need
    /// (jobs, file state, the todo list).
    fn call_scoped(&self, call: &ToolCallReq, cx: &CallCtx) -> ToolOutcome {
        match self.refusal(call, cx) {
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
        let live;
        let exposure = match &self.live_taint {
            Some(taint) if !exposure.tainted && taint() => {
                live = Exposure { tainted: true, ..exposure.clone() };
                &live
            }
            _ => exposure,
        };
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

    fn arg_rules(&self, name: &str) -> Option<marlowe_agent::args::ArgRules> {
        self.inner.arg_rules(name)
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
