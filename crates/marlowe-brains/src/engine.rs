//! Which models Marlowe's own llama-server serves, and with how many slots.
//!
//! A local model goes to the llama.cpp engine when Ollama itself would hand it to llama-server
//! whole (the hardware source says so: [`ModelSource::engine_eligibility`]), the user has not
//! turned the engine off for it, and the planner says it fits whole on the GPU beside what is
//! there now ([`Brains::engine_fit`]). Its policy then means: `parallel` slots of `max_ctx` tokens
//! each. Without a saved policy the planner picks them: six slots when each agent still gets
//! [`ENGINE_MIN_CTX`], fewer slots with more context each when it does not.
//!
//! A model that does not fit stays on Ollama, which splits layers between the GPU and the CPU
//! itself and says so. No GPU reading and no estimate of what the model needs are not "fits":
//! the driver's sysmem fallback would load it anyway, into system memory, at a crawl.

use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use marlowe_hw::plan::{gib, PlanModel};
use marlowe_hw::{build, PlanInputs};
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
/// How long "Ollama keeps this model" stands before the GPU is read again.
const RECHECK: Duration = Duration::from_secs(30);
/// How much more GPU memory than when a load turned out not to fit must be free before the
/// engine tries that model again.
const REGAIN_MIB: f64 = 1024.0;
/// Micro-batch the engine's server runs with (the planner's compute buffers depend on it).
const ENGINE_UBATCH: u32 = 512;

/// Whether the engine takes a model whole on the GPU, as last worked out.
#[derive(Debug, Clone)]
pub(crate) enum Verdict {
    /// It fits: the policy the server starts with (kept so a model never changes shape, and
    /// restarts, between two requests) and the GPU memory the planner expects it to take.
    Fits { saved: Option<ContextPolicy>, policy: ContextPolicy, need_mib: f64 },
    /// Ollama keeps it. `free_mib` is what was free for Marlowe then; `sticky` is a load that
    /// turned out not to fit, tried again only once memory has been given back.
    Stays { at: Instant, why: String, free_mib: Option<f64>, sticky: bool, announced: bool },
}

/// The engine's own switch: `MARLOWE_LLAMA_ENGINE=0` keeps every model on Ollama.
fn disabled_by_env() -> bool {
    std::env::var("MARLOWE_LLAMA_ENGINE").is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "off" | "false" | "no"))
}

/// The slots and window the planner starts a model with: the most agents (up to
/// [`ENGINE_SLOTS`]) that each still get [`ENGINE_MIN_CTX`] (or the model's whole window, when
/// that is smaller) on the GPU beside the weights, with the largest window that many fit. None
/// when not even one agent gets that much: Ollama splits such a model and gives it a real window.
pub fn engine_choice(plan: &PlanModel, kv: KvType) -> Option<(u16, u32)> {
    // Measured live (a 9b model, six slots): the planner's own estimate came out 1.1 GiB under
    // what the server took, and past about 15.5 GiB in all the driver starts spilling to system
    // memory. The engine keeps a GiB more in reserve than the planner's usual headroom.
    let mut plan = plan.clone();
    if let Some(g) = plan.gpu.as_mut() {
        g.headroom_mib += ENGINE_RESERVE_MIB;
    }
    let floor = ENGINE_MIN_CTX.min(plan.ctx_limit).max(marlowe_proto::MIN_CTX);
    for n in (1..=ENGINE_SLOTS).rev() {
        let Some(fit) = plan.auto_ctx(kv, n) else { continue };
        let ctx = fit.min(plan.ctx_limit);
        if ctx >= floor {
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

    fn need_mib(&self, model_id: &str) -> Option<f64> {
        let b = self.0.upgrade()?;
        let model = ModelRef::new(ProviderKind::Ollama, model_id);
        match b.engine_allowed(&model).then(|| b.engine_fit(&model)) {
            // What the planner required to be free when it said "fits": its estimate and the
            // reserve, so a fit kept from a roomier moment is held to the same room at the start.
            Some(Verdict::Fits { need_mib, .. }) => Some(need_mib + ENGINE_RESERVE_MIB),
            _ => None,
        }
    }

    fn declined(&self, model_id: &str, why: &str) {
        if let Some(b) = self.0.upgrade() {
            b.engine_declined(&ModelRef::new(ProviderKind::Ollama, model_id), why);
        }
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

    /// Whether Marlowe's llama-server serves `model`: a local Ollama model the engine may run, the
    /// user has not turned it off for, and that fits whole on the GPU.
    pub fn engine_wanted(&self, model: &ModelRef) -> bool {
        self.engine_allowed(model) && matches!(self.engine_fit(model), Verdict::Fits { .. })
    }

    /// Everything but the fit: the build, the switch, the user's choice and the manifest.
    fn engine_allowed(&self, model: &ModelRef) -> bool {
        self.is_local_ollama(model) && !disabled_by_env() && !self.engine_off(model) && self.engine_eligible(model).is_ok()
    }

    /// How long a verdict that Ollama keeps a model stands before the GPU is read again.
    pub fn set_engine_recheck(&self, every: Duration) {
        *self.ctx.engine_recheck.lock().unwrap_or_else(|e| e.into_inner()) = Some(every);
    }

    /// The plan for running `model` on the engine: what its weights, caches and compute take
    /// against the GPU as measured now. `None` when the model's files cannot be read.
    fn engine_plan(&self, model: &ModelRef) -> Option<PlanModel> {
        let hw = self.factory().hardware()?;
        let a = self.analyzed(model).ok()?;
        let snap = hw.gpu.snapshot();
        let gpu_name = snap.as_ref().map(|s| s.gpu.name.clone()).unwrap_or_default();
        let calib = self.calibration_for(a.files.blob.as_deref(), ENGINE_UBATCH, &gpu_name);
        Some(build(&PlanInputs {
            main: &a.main,
            draft: None,
            spec_n: 0,
            draft_kv: marlowe_hw::CacheType::F16,
            ubatch: ENGINE_UBATCH,
            mmproj_b: a.files.mmproj_bytes,
            gpu: snap.map(|s| s.gpu),
            calib,
        }))
    }

    /// Whether the engine can take `model` whole on the GPU now. A fit is kept as long as the
    /// settings stand; "Ollama keeps it" is looked at again after a while, because what is free
    /// changes. Never "fits" for want of a GPU reading or of an estimate.
    pub(crate) fn engine_fit(&self, model: &ModelRef) -> Verdict {
        let saved = self.with_library(|l| l.policy(model)).ok().flatten();
        let held = self.ctx.engine_fit.lock().unwrap_or_else(|e| e.into_inner()).get(model).cloned();
        let recheck = self.ctx.engine_recheck.lock().unwrap_or_else(|e| e.into_inner()).unwrap_or(RECHECK);
        match &held {
            Some(Verdict::Fits { saved: s, .. }) if *s == saved => return held.expect("matched"),
            Some(Verdict::Stays { at, .. }) if at.elapsed() < recheck => return held.expect("matched"),
            _ => {}
        }
        let mut fresh = self.engine_verdict(model, saved);
        if let (Some(Verdict::Stays { why, free_mib: Some(then), sticky: true, announced, .. }), Verdict::Fits { .. }) = (&held, &fresh) {
            // It did not fit after all, last time: not again until memory has been given back.
            let now = self.engine_plan(model).and_then(|p| p.gpu).map(|g| g.free_for_marlowe_mib());
            if now.is_none_or(|n| n < *then + REGAIN_MIB) {
                fresh = Verdict::Stays { at: Instant::now(), why: why.clone(), free_mib: Some(*then), sticky: true, announced: *announced };
            }
        }
        // A model already announced is not announced again every time the GPU is read.
        if let (Some(Verdict::Stays { announced: true, .. }), Verdict::Stays { announced, .. }) = (&held, &mut fresh) {
            *announced = true;
        }
        self.ctx.engine_fit.lock().unwrap_or_else(|e| e.into_inner()).insert(model.clone(), fresh.clone());
        fresh
    }

    fn engine_verdict(&self, model: &ModelRef, saved: Option<ContextPolicy>) -> Verdict {
        let stays = |why: String, free_mib: Option<f64>| Verdict::Stays { at: Instant::now(), why, free_mib, sticky: false, announced: false };
        if !self.engine_allowed(model) {
            return stays(String::new(), None);
        }
        let Some(plan) = self.engine_plan(model) else {
            return stays("there is no estimate of what it needs on the GPU".into(), None);
        };
        let Some(g) = plan.gpu.as_ref() else {
            return stays("there is no reading of the GPU to fit it against".into(), None);
        };
        let free = g.free_for_marlowe_mib();
        let kv = match self.analyzed(model) {
            Ok(a) if a.main.quantizable() => KvType::Q8_0,
            _ => KvType::F16,
        };
        let (n, ctx, kv_type) = match saved {
            Some(p) => (p.parallel.max(1), p.max_ctx, p.kv_type),
            None => match engine_choice(&plan, kv) {
                Some((n, c)) => (n, c, kv),
                None => {
                    let least = plan.need_mib(ENGINE_MIN_CTX.min(plan.ctx_limit), kv, 1);
                    return stays(format!("needs {} GiB on the GPU, {} GiB free", gib(least), gib(free)), Some(free));
                }
            },
        };
        let need = plan.need_mib(ctx, kv_type, n);
        // The same room the automatic choice leaves: the planner's headroom and the engine's reserve.
        if need > g.budget_mib() - ENGINE_RESERVE_MIB {
            return stays(format!("needs {} GiB on the GPU, {} GiB free", gib(need), gib(free)), Some(free));
        }
        let policy = match saved {
            Some(p) => p,
            None => ContextPolicy { max_ctx: ctx, kv_type, parallel: n, source: PolicySource::Auto, updated_ms: 0 },
        };
        Verdict::Fits { saved, policy, need_mib: need }
    }

    /// Says once, in one line, that Ollama runs a model the engine could have run: the user would
    /// otherwise see the model run partly on the CPU and not know why.
    fn announce_stay(&self, model: &ModelRef) {
        let line = {
            let mut g = self.ctx.engine_fit.lock().unwrap_or_else(|e| e.into_inner());
            match g.get_mut(model) {
                Some(Verdict::Stays { why, announced, .. }) if !*announced && !why.is_empty() => {
                    *announced = true;
                    Some(format!("{} runs on Ollama: {why}", model.model_id))
                }
                _ => None,
            }
        };
        if let (Some(line), Some(o)) = (line, self.factory().allocations().engine_observer()) {
            o.note(&line);
        }
    }

    /// The engine started `model` and could not keep it: it did not fit after all, or the driver
    /// spilled it into system memory. Ollama keeps it until memory has been given back.
    pub(crate) fn engine_declined(&self, model: &ModelRef, why: &str) {
        let free = self.engine_plan(model).and_then(|p| p.gpu).map(|g| g.free_for_marlowe_mib());
        let v = Verdict::Stays { at: Instant::now(), why: why.to_string(), free_mib: free, sticky: true, announced: true };
        self.ctx.engine_fit.lock().unwrap_or_else(|e| e.into_inner()).insert(model.clone(), v);
        self.ctx.seen.lock().unwrap_or_else(|e| e.into_inner()).remove(model);
    }

    /// Turns the llama.cpp engine on or off for `model` (saved). A model that is eligible runs on
    /// the engine by default.
    pub fn set_model_engine(&self, model: &ModelRef, on: bool) -> Result<(), BrainsError> {
        self.with_library(|l| l.set_engine_off(model, !on))?;
        self.ctx.engine_off.lock().unwrap_or_else(|e| e.into_inner()).insert(model.clone(), !on);
        self.ctx.engine_fit.lock().unwrap_or_else(|e| e.into_inner()).remove(model);
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
            self.announce_stay(model);
            return None;
        }
        let p = self.context().policy(model).ok()?;
        Some(EngineShape { slots: p.parallel.max(1), ctx_per_slot: p.max_ctx, kv: p.kv_type.ollama_name().to_string() })
    }

    /// The planner's choice for an engine model with no saved policy, held so a model never
    /// changes shape (and restarts) between two requests. `None` when Ollama keeps the model,
    /// and when a policy is saved.
    pub(crate) fn engine_auto_policy(&self, model: &ModelRef) -> Option<ContextPolicy> {
        if !self.engine_allowed(model) {
            return None;
        }
        match self.engine_fit(model) {
            Verdict::Fits { saved: None, policy, .. } => Some(policy),
            _ => None,
        }
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
            match self.engine_eligible(model) {
                Err(why) => Some(format!("Ollama keeps this model: {why}")),
                Ok(()) => match self.engine_fit(model) {
                    Verdict::Stays { why, .. } if !why.is_empty() => Some(format!("Ollama keeps this model: {why}")),
                    _ => None,
                },
            }
        };
    }
}
