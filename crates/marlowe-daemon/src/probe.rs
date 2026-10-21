//! The conductor's tool surface for `marlowe headless --probe-first-call`: what the session's
//! `serve` composes (built-ins, the MCP host, the memory tools, `dispatch`), with the layers that
//! need a live daemon declared only. A probe never runs a tool, so the memory and dispatch tools
//! carry their real specs and nothing behind them.

use std::sync::Arc;

use marlowe_agent::args::{wired_rules, ArgRules};
use marlowe_agent::{
    CallCtx, Consequence, Effects, Exposure, Mutation, PersonaBlock, Profiles, SandboxView, SystemPrompt, ToolCallReq, ToolCtx, ToolGroup, ToolHost,
    ToolManifest, ToolMiddleware, ToolOutcome,
};
use marlowe_mcp::hub::Hub;
use marlowe_model::ToolSpec;

use crate::mcp_ops::McpTools;
use crate::mind::{memory_specs, MindTools};

/// The answer to any call that reaches the declared tools.
pub const NOT_RUN: &str = "probe: not run";

/// `builtin` (the factory's host for the conductor's model) with the MCP host over it when a
/// `hub` is given, then the memory tools and `dispatch`, as `serve` stacks them.
pub fn conductor_host(builtin: Arc<dyn ToolHost>, hub: Option<(Arc<Hub>, Option<u32>)>) -> Arc<dyn ToolHost> {
    let inner: Arc<dyn ToolHost> = match hub {
        Some((hub, window)) => Arc::new(McpTools::new(builtin, hub, window)),
        None => builtin,
    };
    let mut declared = memory_specs();
    declared.push(marlowe_crew::dispatch::dispatch_spec(&marlowe_crew::Roles::builtin().infos()));
    Arc::new(Declared { inner, declared })
}

/// The conductor's persona: the memory and crew blocks, then the host's.
pub fn conductor_system(host: &dyn ToolHost) -> SystemPrompt {
    SystemPrompt::new().block(PersonaBlock::Memory).block(PersonaBlock::Crew).blocks_from(host.persona_blocks())
}

/// The conductor's per-turn guards (the loop guard), shared by the session stack and the probe so
/// a probe measures the loop that ships.
pub fn conductor_middleware() -> Vec<Arc<dyn ToolMiddleware>> {
    vec![marlowe_agent::agent::guard::LoopGuard::new()]
}

struct Declared {
    inner: Arc<dyn ToolHost>,
    declared: Vec<ToolSpec>,
}

impl Declared {
    fn mine(&self, name: &str) -> bool {
        self.declared.iter().any(|s| s.name == name)
    }
}

impl ToolHost for Declared {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> = self.inner.specs().into_iter().filter(|s| !self.mine(&s.name)).collect();
        specs.extend(self.declared.iter().cloned());
        specs
    }

    fn call(&self, call: &ToolCallReq, ctx: &ToolCtx) -> ToolOutcome {
        if self.mine(&call.name) {
            ToolOutcome::error(NOT_RUN)
        } else {
            self.inner.call(call, ctx)
        }
    }

    fn call_scoped(&self, call: &ToolCallReq, cx: &CallCtx) -> ToolOutcome {
        if self.mine(&call.name) {
            ToolOutcome::error(NOT_RUN)
        } else {
            self.inner.call_scoped(call, cx)
        }
    }

    fn middleware(&self) -> Vec<Arc<dyn ToolMiddleware>> {
        self.inner.middleware()
    }

    fn persona_blocks(&self) -> Vec<PersonaBlock> {
        self.inner.persona_blocks()
    }

    fn consequence(&self, call: &ToolCallReq) -> Consequence {
        if self.mine(&call.name) {
            Consequence::Free
        } else {
            self.inner.consequence(call)
        }
    }

    fn consequence_exposed(&self, call: &ToolCallReq, exposure: &Exposure) -> Consequence {
        if self.mine(&call.name) {
            Consequence::Free
        } else {
            self.inner.consequence_exposed(call, exposure)
        }
    }

    fn taints(&self, call: &ToolCallReq) -> bool {
        !self.mine(&call.name) && self.inner.taints(call)
    }

    fn egress_host(&self, call: &ToolCallReq) -> Option<String> {
        if self.mine(&call.name) {
            None
        } else {
            self.inner.egress_host(call)
        }
    }

    fn describe(&self, call: &ToolCallReq) -> String {
        self.inner.describe(call)
    }

    fn tool_spec(&self, name: &str) -> Option<ToolSpec> {
        match self.declared.iter().find(|s| s.name == name) {
            Some(s) => Some(s.clone()),
            None => self.inner.tool_spec(name),
        }
    }

    fn arg_rules(&self, name: &str) -> Option<ArgRules> {
        if self.mine(name) {
            wired_rules(name)
        } else {
            self.inner.arg_rules(name)
        }
    }

    fn prompt_hints(&self) -> Vec<String> {
        self.inner.prompt_hints()
    }

    fn manifest(&self, name: &str) -> Option<ToolManifest> {
        if MindTools::is_memory_tool(name) {
            let effects = if name == "recall" { Effects::READ } else { Effects::NONE };
            Some(ToolManifest { group: ToolGroup::Memory, profiles: Profiles::ALL, effects, path_args: &[] })
        } else {
            self.inner.manifest(name)
        }
    }

    fn mutation(&self, call: &ToolCallReq, ctx: &ToolCtx) -> Mutation {
        if self.mine(&call.name) {
            Mutation::None
        } else {
            self.inner.mutation(call, ctx)
        }
    }

    fn consequence_in_sandbox(&self, call: &ToolCallReq, exposure: &Exposure, sandbox: &dyn SandboxView) -> Option<Consequence> {
        if self.mine(&call.name) {
            None
        } else {
            self.inner.consequence_in_sandbox(call, exposure, sandbox)
        }
    }
}
