//! Which models Marlowe's own llama-server serves, and with how many slots.
//!
//! A local model goes to the llama.cpp engine when Ollama itself would hand it to llama-server
//! whole (the hardware source says so: [`ModelSource::engine_eligibility`]) and the user has not
//! turned the engine off for it. Its policy then means: `parallel` slots of `max_ctx` tokens each.
//! Without a saved policy the planner picks them: six slots when each agent still gets
//! [`ENGINE_MIN_CTX`], fewer slots with more context each when it does not.

use std::sync::{Arc, Weak};

use marlowe_hw::plan::PlanModel;
use marlowe_model::{EngineAdvisor, EngineShape, ModelRef, ProviderKind};
use marlowe_proto::{ContextPolicy, Engine, EngineSettings, KvType, PolicySource};

use crate::brains::Brains;
use crate::error::BrainsError;

/// Slots the planner starts from: the conductor and five crew agents.
pub const ENGINE_SLOTS: u16 = 6;
/// GPU memory the engine's automatic choice leaves free beyond the planner's headroom, MiB.
pub const ENGINE_RESERVE_MIB: f64 = 1024.0;
/// The smallest window per agent at which the planner still prefers more agents.
pub const ENGINE_MIN_CTX: u32 = 16_384;

/// The engine's own switch: `MARLOWE_LLAMA_ENGINE=0` keeps every model on Ollama.
fn disabled_by_env() -> bool {
    std::env::var("MARLOWE_LLAMA_ENGINE").is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "off" | "false" | "no"))
}

/// The slots and window the planner starts a model with: the most agents (up to
/// [`ENGINE_SLOTS`]) that each still get [`ENGINE_MIN_CTX`] on the GPU beside the weights, with
/// the largest window that many fit; one agent with whatever fits when even that is small.
pub fn engine_choice(plan: &PlanModel, kv: KvType) -> Option<(u16, u32)> {
    // Measured live (marlowe-red:9b, six slots): the planner's own estimate came out 1.1 GiB under
    // what the server took, and past about 15.5 GiB in all the driver starts spilling to system
    // memory. The engine keeps a GiB more in reserve than the planner's usual headroom.
    let mut plan = plan.clone();
    if let Some(g) = plan.gpu.as_mut() {
        g.headroom_mib += ENGINE_RESERVE_MIB;
    }
    for n in (1..=ENGINE_SLOTS).rev() {
        let Some(fit) = plan.auto_ctx(kv, n) else { continue };
        let ctx = fit.min(plan.ctx_limit);
        if ctx >= ENGINE_MIN_CTX || (n == 1 && ctx >= marlowe_proto::MIN_CTX) {
            return Some((n, ctx));
        }
    }
    None
}

struct Advisor(Weak<Brains>);

impl EngineAdvisor for Advisor {
    fn shape(&self, model_id: &str) -> Option<EngineShape> {
        let b = self.0.upgrade()?;
        b.engine_shape(&ModelRef::new(ProviderKind::Ollama, model_id))
    }
}

impl Brains {
    /// Lets the Ollama provider ask which models the engine serves. Called once the library is
    /// shared; until then everything stays on Ollama.
    pub fn install_engine_advisor(self: &Arc<Self>) {
        self.factory().allocations().set_engine_advisor(Arc::new(Advisor(Arc::downgrade(self))));
    }

    /// Why Ollama must keep serving `model` (or `Ok`): the manifest, read once.
    fn engine_eligible(&self, model: &ModelRef) -> Result<(), String> {
        if let Some(r) = self.ctx.eligibility.lock().unwrap_or_else(|e| e.into_inner()).get(&model.model_id) {
            return r.clone();
        }
        let r = match self.factory().hardware() {
            Some(hw) => hw.models.engine_eligibility(&model.model_id),
            None => Err("this build has no llama.cpp engine".into()),
        };
        self.ctx.eligibility.lock().unwrap_or_else(|e| e.into_inner()).insert(model.model_id.clone(), r.clone());
        r
    }

    fn engine_off(&self, model: &ModelRef) -> bool {
        if let Some(off) = self.ctx.engine_off.lock().unwrap_or_else(|e| e.into_inner()).get(model) {
            return *off;
        }
        let off = self.with_library(|l| l.engine_off(model)).unwrap_or(false);
        self.ctx.engine_off.lock().unwrap_or_else(|e| e.into_inner()).insert(model.clone(), off);
        off
    }

    /// Whether Marlowe's llama-server serves `model`: a local Ollama model the engine may run and
    /// the user has not turned it off for.
    pub fn engine_wanted(&self, model: &ModelRef) -> bool {
        self.is_local_ollama(model) && !disabled_by_env() && !self.engine_off(model) && self.engine_eligible(model).is_ok()
    }

    /// Turns the llama.cpp engine on or off for `model` (saved). A model that is eligible runs on
    /// the engine by default.
    pub fn set_model_engine(&self, model: &ModelRef, on: bool) -> Result<(), BrainsError> {
        self.with_library(|l| l.set_engine_off(model, !on))?;
        self.ctx.engine_off.lock().unwrap_or_else(|e| e.into_inner()).insert(model.clone(), !on);
        self.ctx.engine_auto.lock().unwrap_or_else(|e| e.into_inner()).remove(model);
        self.ctx.seen.lock().unwrap_or_else(|e| e.into_inner()).remove(model);
        Ok(())
    }

    /// Whether `model` can run on the engine at all, and if not why not.
    pub fn engine_eligibility(&self, model: &ModelRef) -> Result<(), String> {
        if !self.is_local_ollama(model) {
            return Err("it is not a local model".into());
        }
        self.engine_eligible(model)
    }

    /// How the engine serves `model`, or `None` when Ollama does.
    pub fn engine_shape(&self, model: &ModelRef) -> Option<EngineShape> {
        if !self.engine_wanted(model) {
            return None;
        }
        let p = self.context().policy(model).ok()?;
        Some(EngineShape { slots: p.parallel.max(1), ctx_per_slot: p.max_ctx, kv: p.kv_type.ollama_name().to_string() })
    }

    /// The planner's choice for an engine model with no saved policy, computed once per run so a
    /// model never changes shape (and restarts) between two requests.
    pub(crate) fn engine_auto_policy(&self, model: &ModelRef) -> Option<ContextPolicy> {
        if !self.engine_wanted(model) {
            return None;
        }
        if let Some(p) = self.ctx.engine_auto.lock().unwrap_or_else(|e| e.into_inner()).get(model) {
            return Some(*p);
        }
        if let Some(p) = self.context_plan(model).ok().and_then(|p| p.auto) {
            self.ctx.engine_auto.lock().unwrap_or_else(|e| e.into_inner()).insert(model.clone(), p);
            return Some(p);
        }
        // No GPU measurement: one agent with the default window, which needs no promise.
        // Kept like any other choice: asking the machine again on every request would be slow, and
        // a shape that changes between two requests restarts the server.
        let kv = KvType::F16;
        let p = ContextPolicy { max_ctx: self.default_window(model).ok()?, kv_type: kv, parallel: 1, source: PolicySource::Auto, updated_ms: 0 };
        self.ctx.engine_auto.lock().unwrap_or_else(|e| e.into_inner()).insert(model.clone(), p);
        Some(p)
    }

    /// Says on `e` which engine serves `model`: the llama.cpp engine takes over the Ollama-only
    /// facts (a qwen35 model served one request at a time, the server environment), and a model
    /// that stays on Ollama carries the reason.
    pub(crate) fn mark_engine(&self, model: &ModelRef, e: &mut EngineSettings) {
        if self.engine_wanted(model) {
            e.engine = Engine::LlamaCpp;
            e.serial_arch = None;
            e.saved_kv_type = None;
            e.saved_parallel = None;
            e.saved_flash_attn = None;
            return;
        }
        if !self.is_local_ollama(model) || self.factory().hardware().is_none() {
            return;
        }
        e.engine_user_off = self.engine_off(model) && self.engine_eligible(model).is_ok() && !disabled_by_env();
        e.engine_note = if disabled_by_env() {
            Some("the llama.cpp engine is switched off (MARLOWE_LLAMA_ENGINE=0)".into())
        } else if self.engine_off(model) {
            Some("the llama.cpp engine is turned off for this model".into())
        } else {
            self.engine_eligible(model).err().map(|why| format!("Ollama keeps this model: {why}"))
        };
    }
}
