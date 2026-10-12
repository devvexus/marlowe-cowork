//! `structured_output` (WF3): the tool a workflow agent with a schema answers through. Its
//! parameters are the schema; the call is validated here, at the tool layer, and a wrong answer
//! gets the errors back to fix (at most [`MAX_RETRIES`] retries). A valid answer is kept for the
//! workflow; the agent's turn goes on to its natural end.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use marlowe_agent::{
    CallCtx, Consequence, Effects, Exposure, Mutation, PersonaBlock, Profiles, SandboxView, ToolCallReq, ToolCtx, ToolGroup, ToolHost,
    ToolManifest, ToolMiddleware, ToolOutcome,
};
use marlowe_model::ToolSpec;
use serde_json::Value;

/// The tool's name.
pub const STRUCTURED_OUTPUT: &str = "structured_output";
/// Retries after an invalid call; the invalid call after the last retry is refused (the workflow
/// then sees `None`).
pub const MAX_RETRIES: u32 = 3;

/// Where the valid answer lands.
pub type Slot = Arc<Mutex<Option<Value>>>;

/// The agent's tools plus `structured_output` for one schema.
pub struct WithStructuredOutput {
    inner: Arc<dyn ToolHost>,
    schema: Value,
    slot: Slot,
    invalid: AtomicU32,
}

impl WithStructuredOutput {
    pub fn new(inner: Arc<dyn ToolHost>, schema: Value, slot: Slot) -> Self {
        WithStructuredOutput { inner, schema, slot, invalid: AtomicU32::new(0) }
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: STRUCTURED_OUTPUT.into(),
            description: "Give your final answer as structured data. Its parameters are exactly the answer's schema. Call it once, when you are done; \
                          if it reports errors, fix them and call it again."
                .into(),
            input_schema: self.schema.clone(),
        }
    }

    fn answer(&self, args: &Value) -> ToolOutcome {
        let errors = marlowe_workflow::schema::validate(args, &self.schema);
        if errors.is_empty() {
            *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(args.clone());
            return ToolOutcome::ok("Recorded. That is your answer; end your turn now with a one-line summary.", "answer recorded");
        }
        let n = self.invalid.fetch_add(1, Ordering::SeqCst) + 1;
        if n > MAX_RETRIES {
            return ToolOutcome::error(format!(
                "The answer still does not match the schema ({}). No more attempts: end your turn.",
                errors.join("; ")
            ));
        }
        ToolOutcome::error(format!(
            "The answer does not match the schema; fix these and call structured_output again ({} of {MAX_RETRIES} retries left):\n- {}",
            MAX_RETRIES + 1 - n,
            errors.join("\n- ")
        ))
    }
}

fn is_ours(call: &ToolCallReq) -> bool {
    call.name == STRUCTURED_OUTPUT
}

impl ToolHost for WithStructuredOutput {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> = self.inner.specs().into_iter().filter(|s| s.name != STRUCTURED_OUTPUT).collect();
        specs.push(self.spec());
        specs
    }

    fn call(&self, call: &ToolCallReq, ctx: &ToolCtx) -> ToolOutcome {
        if is_ours(call) {
            self.answer(&call.args)
        } else {
            self.inner.call(call, ctx)
        }
    }

    fn call_scoped(&self, call: &ToolCallReq, cx: &CallCtx) -> ToolOutcome {
        if is_ours(call) {
            self.answer(&call.args)
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
        if is_ours(call) {
            Consequence::Free
        } else {
            self.inner.consequence(call)
        }
    }

    fn consequence_exposed(&self, call: &ToolCallReq, exposure: &Exposure) -> Consequence {
        if is_ours(call) {
            Consequence::Free
        } else {
            self.inner.consequence_exposed(call, exposure)
        }
    }

    fn consequence_in_sandbox(&self, call: &ToolCallReq, exposure: &Exposure, sandbox: &dyn SandboxView) -> Option<Consequence> {
        if is_ours(call) {
            Some(Consequence::Free)
        } else {
            self.inner.consequence_in_sandbox(call, exposure, sandbox)
        }
    }

    fn taints(&self, call: &ToolCallReq) -> bool {
        !is_ours(call) && self.inner.taints(call)
    }

    fn egress_host(&self, call: &ToolCallReq) -> Option<String> {
        if is_ours(call) {
            None
        } else {
            self.inner.egress_host(call)
        }
    }

    fn arg_rules(&self, name: &str) -> Option<marlowe_agent::args::ArgRules> {
        if name == STRUCTURED_OUTPUT {
            // It validates against the script's schema itself, and counts the retries.
            Some(marlowe_agent::args::ArgRules { skip: true, ..marlowe_agent::args::ArgRules::NONE })
        } else {
            self.inner.arg_rules(name)
        }
    }

    fn manifest(&self, name: &str) -> Option<ToolManifest> {
        if name == STRUCTURED_OUTPUT {
            Some(ToolManifest { group: ToolGroup::Workflow, profiles: Profiles::ALL, effects: Effects::NONE, path_args: &[] })
        } else {
            self.inner.manifest(name)
        }
    }

    fn mutation(&self, call: &ToolCallReq, ctx: &ToolCtx) -> Mutation {
        if is_ours(call) {
            Mutation::None
        } else {
            self.inner.mutation(call, ctx)
        }
    }

    fn describe(&self, call: &ToolCallReq) -> String {
        if is_ours(call) {
            "structured_output".into()
        } else {
            self.inner.describe(call)
        }
    }
}
