//! How Marlowe's own llama-server serves each local model, and with how many slots.
//!
//! Every local model runs on the llama.cpp engine; Ollama is only where its files come from. One
//! llama.cpp cannot run (the hardware source says why: [`ModelSource::engine_eligibility`]) is an
//! error at its first request. A model the planner says fits whole on the GPU beside what is there
//! now ([`Brains::engine_fit`]) runs there, its policy meaning `parallel` slots of `max_ctx`
//! tokens each. Without a saved policy the planner picks them: six slots when each agent still gets
//! [`ENGINE_MIN_CTX`], fewer slots with more context each when it does not.
//!
//! Only the GPU part counts against the GPU: the input layer (token and n-gram embedding tables)
//! stays on the CPU, memory-mapped by default, and an MoE model whose experts do not fit keeps the
//! experts of its first blocks on the CPU (`--n-cpu-moe`) with every layer on the GPU. The
//! automatic choice takes the model whole first and moves experts only when not one agent fits
//! that way.
//!
//! A model that does not fit even so runs split: llama.cpp places what fits on the GPU and keeps
//! the rest on the CPU, as Ollama does, with one slot of the model's default window unless a policy
//! is saved. No GPU reading and no estimate of what the model needs are not "fits": the driver's
//! sysmem fallback would load it whole anyway, into system memory, at a crawl.

use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use marlowe_hw::plan::{gib, PlanModel};
use marlowe_hw::{build, Experts, HostTables, Placement, PlanInputs};
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
/// How long "this model runs split" stands before the GPU is read again.
const RECHECK: Duration = Duration::from_secs(30);
/// How much more GPU memory than when a load turned out not to fit must be free before the
/// engine tries that model again.
const REGAIN_MIB: f64 = 1024.0;
/// Micro-batch the engine's server runs with (the planner's compute buffers depend on it).
const ENGINE_UBATCH: u32 = 512;
/// Draft positions the engine's speculative decoding runs with, as Ollama's launch.
const ENGINE_SPEC_N: u32 = 3;

/// Whether the engine takes a model whole on the GPU, as last worked out.
#[derive(Debug, Clone)]
pub(crate) enum Verdict {
    /// It fits: the policy the server starts with (kept so a model never changes shape, and
    /// restarts, between two requests), the GPU memory the planner expects it to take, and the
    /// blocks whose experts stay on the CPU for it to fit (`--n-cpu-moe`).
    Fits { saved: Option<ContextPolicy>, policy: ContextPolicy, need_mib: f64, cpu_moe: u32 },
    /// It runs split between the GPU and the CPU. `free_mib` is what was free for Marlowe then;
    /// `sticky` is a load that turned out not to fit, tried whole again only once memory has been
    /// given back.
    Split { at: Instant, why: String, free_mib: Option<f64>, sticky: bool, announced: bool },
}

/// The slots and window the planner starts a model with: the most agents (up to
/// [`ENGINE_SLOTS`]) that each still get [`ENGINE_MIN_CTX`] (or the model's whole window, when
/// that is smaller) on the GPU beside the weights, with the largest window that many fit. An MoE
/// model is tried whole first; its experts move to the CPU only when not one agent fits that way
/// (the placement's [`Experts::Auto`]). None when not even one agent gets that much: such a model
/// runs split, with a real window.
pub fn engine_choice(plan: &PlanModel, kv: KvType) -> Option<(u16, u32)> {
    let plan = with_reserve(plan);
    let floor = ENGINE_MIN_CTX.min(plan.ctx_limit).max(marlowe_proto::MIN_CTX);
    let mut tries = Vec::new();
    if plan.has_experts() && plan.placement.experts == Experts::Auto {
        tries.push(plan.with_placement(Placement { experts: Experts::Gpu, ..plan.placement }));
    }
    tries.push(plan);
    for p in &tries {
        for n in (1..=ENGINE_SLOTS).rev() {
            let Some(fit) = p.auto_ctx(kv, n) else { continue };
            let ctx = fit.min(p.ctx_limit);
            if ctx >= floor {
                return Some((n, ctx));
            }
        }
    }
    None
}

/// `plan` with the engine's reserve on top of the planner's headroom. Measured live (a 9b model,
/// six slots): the planner's own estimate came out 1.1 GiB under what the server took, and past
/// about 15.5 GiB in all the driver starts spilling to system memory.
fn with_reserve(plan: &PlanModel) -> PlanModel {
    let mut plan = plan.clone();
    if let Some(g) = plan.gpu.as_mut() {
        g.headroom_mib += ENGINE_RESERVE_MIB;
    }
    plan
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
    /// Lets the local provider ask how the engine serves each model. Called once the library is
    /// shared; until then the engine's defaults apply.
    pub fn install_engine_advisor(self: &Arc<Self>) {
        self.factory().allocations().set_engine_advisor(Arc::new(Advisor(Arc::downgrade(self))));
    }

    /// Why llama.cpp can't run `model` (or `Ok`): its files, read once.
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

    /// Whether `model` runs whole on the GPU: a local model llama.cpp can run that fits there.
    pub fn engine_wanted(&self, model: &ModelRef) -> bool {
        self.engine_allowed(model) && matches!(self.engine_fit(model), Verdict::Fits { .. })
    }

    /// Everything but the fit: a local model whose files llama.cpp can run.
    fn engine_allowed(&self, model: &ModelRef) -> bool {
        self.is_local_ollama(model) && self.engine_eligible(model).is_ok()
    }

    /// How long a verdict that a model runs split stands before the GPU is read again.
    pub fn set_engine_recheck(&self, every: Duration) {
        *self.ctx.engine_recheck.lock().unwrap_or_else(|e| e.into_inner()) = Some(every);
    }

    /// The plan for running `model` on the engine: what its weights, caches and compute take
    /// against the GPU as measured now, placed as its saved policy says. `None` when the model's
    /// files cannot be read.
    fn engine_plan(&self, model: &ModelRef) -> Option<PlanModel> {
        let hw = self.factory().hardware()?;
        let a = self.analyzed(model).ok()?;
        let placement = self.with_library(|l| l.policy(model)).ok().flatten().map(|p| p.placement).unwrap_or_default();
        let snap = hw.gpu.snapshot();
        let gpu_name = snap.as_ref().map(|s| s.gpu.name.clone()).unwrap_or_default();
        let calib = self.calibration_for(a.files.blob.as_deref(), ENGINE_UBATCH, &gpu_name);
        Some(build(&PlanInputs {
            main: &a.main,
            // The draft the engine runs beside it (Ollama's `--spec-draft-n-max 3`).
            draft: a.draft.as_ref(),
            spec_n: if a.draft.is_some() { ENGINE_SPEC_N } else { 0 },
            draft_kv: marlowe_hw::CacheType::F16,
            ubatch: ENGINE_UBATCH,
            mmproj_b: a.files.mmproj_bytes,
            gpu: snap.map(|s| s.gpu),
            calib,
            placement,
        }))
    }

    /// Whether the engine can take `model` whole on the GPU now. A fit is kept as long as the
    /// settings stand; "split" is looked at again after a while, because what is free changes.
    /// Never "fits" for want of a GPU reading or of an estimate.
    pub(crate) fn engine_fit(&self, model: &ModelRef) -> Verdict {
        let saved = self.with_library(|l| l.policy(model)).ok().flatten();
        let held = self.ctx.engine_fit.lock().unwrap_or_else(|e| e.into_inner()).get(model).cloned();
        let recheck = self.ctx.engine_recheck.lock().unwrap_or_else(|e| e.into_inner()).unwrap_or(RECHECK);
        match &held {
            Some(Verdict::Fits { saved: s, .. }) if *s == saved => return held.expect("matched"),
            Some(Verdict::Split { at, .. }) if at.elapsed() < recheck => return held.expect("matched"),
            _ => {}
        }
        let mut fresh = self.engine_verdict(model, saved);
        if let (Some(Verdict::Split { why, free_mib: Some(then), sticky: true, announced, .. }), Verdict::Fits { .. }) = (&held, &fresh) {
            // It did not fit after all, last time: not whole again until memory has been given back.
            let now = self.engine_plan(model).and_then(|p| p.gpu).map(|g| g.free_for_marlowe_mib());
            if now.is_none_or(|n| n < *then + REGAIN_MIB) {
                fresh = Verdict::Split { at: Instant::now(), why: why.clone(), free_mib: Some(*then), sticky: true, announced: *announced };
            }
        }
        // A model already announced is not announced again every time the GPU is read.
        if let (Some(Verdict::Split { announced: true, .. }), Verdict::Split { announced, .. }) = (&held, &mut fresh) {
            *announced = true;
        }
        self.ctx.engine_fit.lock().unwrap_or_else(|e| e.into_inner()).insert(model.clone(), fresh.clone());
        fresh
    }

    fn engine_verdict(&self, model: &ModelRef, saved: Option<ContextPolicy>) -> Verdict {
        let stays = |why: String, free_mib: Option<f64>| Verdict::Split { at: Instant::now(), why, free_mib, sticky: false, announced: false };
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
        // The same room the automatic choice leaves: the planner's headroom and the engine's
        // reserve. Experts the placement lets go move to the CPU against that room.
        let room = with_reserve(&plan);
        let need = room.need_mib(ctx, kv_type, n);
        if room.budget_mib().is_none_or(|b| need > b) {
            return stays(format!("needs {} GiB on the GPU, {} GiB free", gib(need), gib(free)), Some(free));
        }
        let cpu_moe = room.cpu_moe(ctx, kv_type, n);
        let policy = match saved {
            Some(p) => p,
            None => ContextPolicy { max_ctx: ctx, kv_type, parallel: n, source: PolicySource::Auto, updated_ms: 0, placement: Default::default() },
        };
        Verdict::Fits { saved, policy, need_mib: need, cpu_moe }
    }

    /// Says once, in one line, that a model runs split: the user would otherwise see it run partly
    /// on the CPU and not know why.
    fn announce_split(&self, model: &ModelRef) {
        let line = {
            let mut g = self.ctx.engine_fit.lock().unwrap_or_else(|e| e.into_inner());
            match g.get_mut(model) {
                Some(Verdict::Split { why, announced, .. }) if !*announced && !why.is_empty() => {
                    *announced = true;
                    Some(format!("{} runs split between the GPU and the CPU: {why}", model.model_id))
                }
                _ => None,
            }
        };
        if let (Some(line), Some(o)) = (line, self.factory().allocations().engine_observer()) {
            o.note(&line);
        }
    }

    /// The engine could not start `model` whole: it did not fit after all, or the driver spilled it
    /// into system memory. It runs split until memory has been given back.
    pub(crate) fn engine_declined(&self, model: &ModelRef, why: &str) {
        let free = self.engine_plan(model).and_then(|p| p.gpu).map(|g| g.free_for_marlowe_mib());
        let v = Verdict::Split { at: Instant::now(), why: why.to_string(), free_mib: free, sticky: true, announced: true };
        self.ctx.engine_fit.lock().unwrap_or_else(|e| e.into_inner()).insert(model.clone(), v);
        self.ctx.seen.lock().unwrap_or_else(|e| e.into_inner()).remove(model);
    }

    /// The old per-model switch back to Ollama. Every local model runs on the engine now, so only
    /// "on" is accepted (it clears a switch saved before).
    pub fn set_model_engine(&self, model: &ModelRef, on: bool) -> Result<(), BrainsError> {
        if !on {
            return Err(BrainsError::Invalid("every local model runs on the llama.cpp engine; Ollama only provides the model files".into()));
        }
        self.with_library(|l| l.set_engine_off(model, false))?;
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

    /// How the engine serves `model`: whole on the GPU with its policy, or split. `None` for a
    /// hosted model and one llama.cpp can't run (its request then says why).
    pub fn engine_shape(&self, model: &ModelRef) -> Option<EngineShape> {
        if !self.engine_allowed(model) {
            return None;
        }
        let (split, cpu_moe) = match self.engine_fit(model) {
            Verdict::Fits { cpu_moe, .. } => (false, cpu_moe),
            Verdict::Split { .. } => (true, 0),
        };
        if split {
            self.announce_split(model);
        }
        let p = self.context().policy(model).ok()?;
        Some(EngineShape {
            slots: p.parallel.max(1),
            ctx_per_slot: p.max_ctx,
            kv: p.kv_type.ollama_name().to_string(),
            split,
            cpu_moe,
            host_ram: p.placement.host == HostTables::Ram,
        })
    }

    /// The planner's choice for an engine model with no saved policy, held so a model never
    /// changes shape (and restarts) between two requests: the slots that fit whole, or for a split
    /// model one slot of its default window. `None` when a policy is saved or llama.cpp can't run
    /// the model.
    pub(crate) fn engine_auto_policy(&self, model: &ModelRef) -> Option<ContextPolicy> {
        if !self.engine_allowed(model) {
            return None;
        }
        match self.engine_fit(model) {
            Verdict::Fits { saved: None, policy, .. } => Some(policy),
            Verdict::Split { .. } if self.with_library(|l| l.policy(model)).ok().flatten().is_none() => {
                let kv = match self.analyzed(model) {
                    Ok(a) if a.main.quantizable() => KvType::Q8_0,
                    _ => KvType::F16,
                };
                let max_ctx = self.default_window(model).ok()?.min(self.ctx_limit(model).ok()?);
                Some(ContextPolicy { max_ctx, kv_type: kv, parallel: 1, source: PolicySource::Auto, updated_ms: 0, placement: Default::default() })
            }
            _ => None,
        }
    }

    /// Says on `e` how the engine serves `model`: every local model runs on llama.cpp, so the
    /// Ollama-only facts (a qwen35 model served one request at a time, Ollama's environment) do
    /// not apply; a split model, and one llama.cpp can't run, carry the reason.
    pub(crate) fn mark_engine(&self, model: &ModelRef, e: &mut EngineSettings) {
        if !self.is_local_ollama(model) {
            return;
        }
        e.engine = Engine::LlamaCpp;
        e.serial_arch = None;
        e.saved_kv_type = None;
        e.saved_parallel = None;
        e.saved_flash_attn = None;
        e.engine_user_off = false;
        if self.factory().hardware().is_none() {
            return;
        }
        e.engine_note = match self.engine_eligible(model) {
            Err(why) => Some(format!("llama.cpp can't run this model: {why}")),
            Ok(()) => match self.engine_fit(model) {
                Verdict::Split { why, .. } if !why.is_empty() => Some(format!("llama.cpp runs it split between the GPU and the CPU: {why}")),
                _ => None,
            },
        };
    }
}
