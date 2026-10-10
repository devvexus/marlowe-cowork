//! Context policy per model: the gate the crew calls ("never above `max_ctx`"), and the planner
//! the client asks what a model costs on this machine.
//!
//! How "never higher" holds:
//! 1. an agent's window is [`ContextGate::allowed_ctx`]; it compacts at 70% of that;
//! 2. every request for the model sends `num_ctx = max_ctx` (the shared allocation), a constant,
//!    so Ollama never reloads per agent, and the provider clamps anything larger;
//! 3. a crew request for more gets the maximum, and the run log can say so.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use marlowe_hw::arch::{ArchProfile, Role};
use marlowe_hw::plan::MIB;
use marlowe_hw::{build, calibrate, log, CacheType, Calibration, ModelFiles, PlanInputs, RunnerObservation, ServerChange};
use marlowe_model::{ModelRef, ProviderKind};
use marlowe_proto::{
    planned_changes, ArchSummary, ContextPlan, ContextPolicy, Engine, EngineSettings, KvType, ModelContext, PlanRow,
    PlannedChange, PolicySource, RunnerSlot, Slot, MAX_AGENTS, MIN_CTX,
};

use crate::brains::Brains;
use crate::error::BrainsError;
use crate::library::now_ms;

/// The window a local model gets with no policy (the provider's own default).
pub const DEFAULT_WINDOW: u32 = 32_768;
/// Under this, the automatic choice is flagged: long tasks will compact often.
pub const SMALL_AUTO_CTX: u32 = 16_384;

/// A model's files, analysed once.
pub(crate) struct Analyzed {
    pub files: ModelFiles,
    pub main: ArchProfile,
    pub draft: Option<ArchProfile>,
}

/// Per-model slot leases: at most `policy.parallel` agents on a model at once.
#[derive(Default)]
pub(crate) struct LeaseTable {
    held: Mutex<HashMap<String, u16>>,
    freed: Condvar,
}

/// State the context gate keeps beside the library.
#[derive(Default)]
pub(crate) struct ContextState {
    pub leases: Arc<LeaseTable>,
    pub analyzed: Mutex<HashMap<String, Arc<Analyzed>>>,
    /// The engine as last observed, per model (for defaults between plans).
    pub seen: Mutex<HashMap<ModelRef, EngineSettings>>,
}

/// A held agent slot on one model; released on drop.
pub struct SlotLease {
    table: Arc<LeaseTable>,
    key: String,
}

impl SlotLease {
    pub fn model_key(&self) -> &str {
        &self.key
    }
}

impl Drop for SlotLease {
    fn drop(&mut self) {
        let mut held = self.table.held.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = held.get_mut(&self.key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                held.remove(&self.key);
            }
        }
        self.table.freed.notify_all();
    }
}

/// The context policy, read and enforced. From [`Brains::context`].
pub struct ContextGate<'a> {
    b: &'a Brains,
}

impl Brains {
    /// The context gate: allowed windows, slots, leases and policies.
    pub fn context(&self) -> ContextGate<'_> {
        ContextGate { b: self }
    }
}

impl ContextGate<'_> {
    /// The window an agent on `model` may use: `min(requested, max_ctx)`, never above `max_ctx`.
    ///
    /// When the loaded runner was launched for `max_ctx` but its slots hold less (Ollama split
    /// the window across `-np` slots), the slot size is the cap: past it the runner's context
    /// shift silently drops tokens.
    pub fn allowed_ctx(&self, model: &ModelRef, requested: Option<u32>) -> Result<u32, BrainsError> {
        let mut max = self.policy(model)?.max_ctx;
        let seen = self.b.ctx.seen.lock().unwrap_or_else(|e| e.into_inner()).get(model).and_then(|e| e.split_slot_ctx(max));
        if let Some(slot) = seen {
            max = max.min(slot);
        }
        Ok(requested.map_or(max, |r| r.min(max)))
    }

    /// Concurrent agents on this model's runner, the conductor included when it shares it.
    pub fn slots(&self, model: &ModelRef) -> Result<u16, BrainsError> {
        Ok(self.policy(model)?.parallel.max(1))
    }

    /// Waits up to `timeout` for a free slot on `model` (crew threads only; the conductor never
    /// waits). The slot is released when the lease drops.
    pub fn lease(&self, model: &ModelRef, timeout: Duration) -> Result<SlotLease, BrainsError> {
        let slots = self.slots(model)?;
        let table = self.b.ctx.leases.clone();
        let key = model.to_string();
        let deadline = Instant::now() + timeout;
        let mut held = table.held.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let n = held.get(&key).copied().unwrap_or(0);
            if n < slots {
                held.insert(key.clone(), n + 1);
                drop(held);
                return Ok(SlotLease { table: table.clone(), key });
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(BrainsError::Invalid(format!(
                    "all {slots} agent slot{} on {model} are busy",
                    if slots == 1 { "" } else { "s" }
                )));
            }
            held = table.freed.wait_timeout(held, left).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    /// The saved policy, or the default: the window requests use now, the engine's KV type and
    /// parallel count as last seen.
    pub fn policy(&self, model: &ModelRef) -> Result<ContextPolicy, BrainsError> {
        if let Some(p) = self.b.with_library(|l| l.policy(model))? {
            return Ok(p);
        }
        let seen = self.b.ctx.seen.lock().unwrap_or_else(|e| e.into_inner()).get(model).cloned().unwrap_or_default();
        Ok(ContextPolicy {
            max_ctx: self.b.default_window(model)?,
            kv_type: seen.next_kv_type().unwrap_or(KvType::F16),
            parallel: seen.next_parallel().unwrap_or(1).max(1),
            source: PolicySource::Auto,
            updated_ms: 0,
        })
    }

    /// Saves `p` for `model` and returns what changed. Refuses a window above the model's own
    /// maximum or under [`MIN_CTX`], and a parallel count of 0 or over [`MAX_AGENTS`].
    pub fn set_policy(&self, model: &ModelRef, p: ContextPolicy) -> Result<Vec<PlannedChange>, BrainsError> {
        let limit = self.b.ctx_limit(model)?;
        if p.max_ctx > limit {
            return Err(BrainsError::Invalid(format!("{model} supports at most {limit} tokens of context, not {}", p.max_ctx)));
        }
        if p.max_ctx < MIN_CTX {
            return Err(BrainsError::Invalid(format!("a context under {MIN_CTX} tokens is too small to hold a turn")));
        }
        if p.parallel == 0 || p.parallel > MAX_AGENTS {
            return Err(BrainsError::Invalid(format!("agents at once must be 1 to {MAX_AGENTS}")));
        }
        let old = self.policy(model)?.max_ctx;
        let in_effect = self.b.engine_now(model);
        let changes = planned_changes(old, &in_effect, &p);
        let p = ContextPolicy { updated_ms: now_ms(), ..p };
        self.b.with_library(|l| l.set_policy(model, &p))?;
        self.b.allocate(model, p.max_ctx);
        Ok(changes)
    }
}

/// `C:\…\blobs\sha256-17c3…` → `sha256-17c3…`.
fn blob_key(blob: &str) -> &str {
    blob.rsplit(['\\', '/']).next().unwrap_or(blob)
}

fn engine_settings(rt: &RunnerObservation, has_draft: bool) -> EngineSettings {
    let launch = rt.load.as_ref().map(|l| &l.launch);
    let own = rt.load.as_ref().filter(|l| !l.contexts.is_empty());
    let on = |s: &Option<String>| s.as_deref().map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "on" | "1" | "true"));
    let slots: Vec<RunnerSlot> = rt
        .slots
        .iter()
        .map(|s| RunnerSlot { id: s.id, n_ctx: s.n_ctx, n_prompt_tokens: s.n_prompt_tokens, is_processing: s.is_processing })
        .collect();
    EngineSettings {
        engine: Engine::Ollama,
        loaded: rt.loaded,
        num_ctx: slots.first().map(|s| s.n_ctx).or(rt.ps_context).or_else(|| own.and_then(|l| l.launch.ctx)),
        launch_ctx: own.and_then(|l| l.launch.ctx),
        parallel: launch.and_then(|l| l.parallel).map(|n| n as u16),
        kv_type: launch.and_then(|l| l.cache_k.as_deref()).and_then(KvType::parse),
        flash_attn: launch.and_then(|l| on(&l.flash_attn)),
        spec_n: launch.and_then(|l| l.spec_n).filter(|_| has_draft),
        gpu_layers: own.and_then(|l| l.main()).and_then(|c| c.offloaded),
        saved_kv_type: rt.saved.kv_cache_type.as_deref().and_then(KvType::parse),
        saved_parallel: rt.saved.num_parallel.as_deref().and_then(|s| s.trim().parse().ok()),
        saved_flash_attn: on(&rt.saved.flash_attention),
        slots,
        decode_tok_s: own.and_then(|l| l.decode_tok_s),
        load_secs: own.and_then(|l| l.started_secs),
    }
}

impl Brains {
    /// The window requests to `model` use with no policy.
    pub(crate) fn default_window(&self, model: &ModelRef) -> Result<u32, BrainsError> {
        let info = self.model_info(model)?;
        Ok(info.context_window.unwrap_or(DEFAULT_WINDOW))
    }

    /// The model's own maximum context: its trained context from the file for a local model,
    /// else what the catalog says.
    pub(crate) fn ctx_limit(&self, model: &ModelRef) -> Result<u32, BrainsError> {
        if let Ok(a) = self.analyzed(model) {
            if let Some(c) = a.main.n_ctx_train {
                return Ok(c);
            }
        }
        Ok(self.model_info(model)?.context_window.unwrap_or(DEFAULT_WINDOW))
    }

    fn is_local_ollama(&self, model: &ModelRef) -> bool {
        model.provider == ProviderKind::Ollama && self.model_info(model).map(|i| i.local).unwrap_or(true)
    }

    /// Sets the provider allocation for a local model.
    pub(crate) fn allocate(&self, model: &ModelRef, max_ctx: u32) {
        if model.provider == ProviderKind::Ollama {
            self.factory().allocations().set(&model.model_id, max_ctx);
        }
    }

    /// Loads every saved policy into the provider allocations (at start).
    pub(crate) fn sync_allocations(&self) {
        if let Ok(list) = self.with_library(|l| l.policies()) {
            for (m, p) in list {
                self.allocate(&m, p.max_ctx);
            }
        }
    }

    /// Forgets cached model files (a model was added, removed or re-pulled).
    pub(crate) fn forget_files(&self) {
        self.ctx.analyzed.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    pub(crate) fn analyzed(&self, model: &ModelRef) -> Result<Arc<Analyzed>, BrainsError> {
        if !self.is_local_ollama(model) {
            return Err(BrainsError::Invalid(format!("{model} is not a local model")));
        }
        if let Some(a) = self.ctx.analyzed.lock().unwrap_or_else(|e| e.into_inner()).get(&model.model_id) {
            return Ok(a.clone());
        }
        let hw = self.factory().hardware().ok_or_else(|| BrainsError::Invalid("no hardware probe in this build".into()))?;
        let files = hw.models.files(&model.model_id).map_err(BrainsError::Invalid)?;
        let main = ArchProfile::analyze(&files.main, Role::Main).map_err(BrainsError::Invalid)?;
        let draft = files.draft.as_ref().map(|d| ArchProfile::analyze(d, Role::Draft)).transpose().map_err(BrainsError::Invalid)?;
        let a = Arc::new(Analyzed { files, main, draft });
        self.ctx.analyzed.lock().unwrap_or_else(|e| e.into_inner()).insert(model.model_id.clone(), a.clone());
        Ok(a)
    }

    /// What the engine runs for `model` now (hosted models: the cloud engine).
    pub(crate) fn engine_now(&self, model: &ModelRef) -> EngineSettings {
        if !self.is_local_ollama(model) {
            return EngineSettings { engine: Engine::Cloud, ..EngineSettings::default() };
        }
        let Some(hw) = self.factory().hardware() else { return EngineSettings::default() };
        let a = self.analyzed(model).ok();
        let blob = a.as_ref().and_then(|a| a.files.blob.clone());
        let rt = hw.models.runtime(&model.model_id, blob.as_deref());
        let e = engine_settings(&rt, a.as_ref().is_some_and(|a| a.draft.is_some()));
        self.ctx.seen.lock().unwrap_or_else(|e| e.into_inner()).insert(model.clone(), e.clone());
        e
    }

    /// The context and hardware plan for `model`.
    pub fn context_plan(&self, model: &ModelRef) -> Result<ContextPlan, BrainsError> {
        let info = self.model_info(model)?;
        if !self.is_local_ollama(model) {
            let policy = self.context().policy(model)?;
            return Ok(ContextPlan {
                model: model.clone(),
                plan: None,
                arch: ArchSummary { arch: "hosted".into(), summary: format!("{} · hosted, nothing runs here", info.display), ..Default::default() },
                rows: Vec::new(),
                in_effect: EngineSettings { engine: Engine::Cloud, ..Default::default() },
                policy,
                auto: None,
                ctx_limit: info.context_window.unwrap_or(DEFAULT_WINDOW),
                ram_total_mib: None,
                ram_free_mib: None,
                warnings: Vec::new(),
            });
        }
        let hw = self.factory().hardware().cloned().ok_or_else(|| BrainsError::Invalid("no hardware probe in this build".into()))?;
        let a = self.analyzed(model)?;
        let blob = a.files.blob.clone();
        let rt = hw.models.runtime(&model.model_id, blob.as_deref());
        let in_effect = engine_settings(&rt, a.draft.is_some());
        self.ctx.seen.lock().unwrap_or_else(|e| e.into_inner()).insert(model.clone(), in_effect.clone());
        let snap = hw.gpu.snapshot();
        let mut warnings: Vec<String> = a.main.warnings.clone();

        // Calibrate from this model's own load when its runner is the only one on the GPU.
        let ub = rt.load.as_ref().and_then(|l| l.launch.ubatch).unwrap_or(512);
        let gpu_name = snap.as_ref().map(|s| s.gpu.name.clone()).unwrap_or_default();
        let own_load = rt.load.as_ref().filter(|l| !l.contexts.is_empty());
        if let (Some(load), Some(s), true) = (own_load, snap.as_ref(), rt.loaded) {
            if s.runners.len() == 1 {
                let c = calibrate(load, Some(s.runners[0].1));
                self.with_library(|l| {
                    if let Some(d) = c.driver_mib {
                        let _ = l.set_calibration(&format!("driver|{gpu_name}"), d);
                    }
                    if let (Some((ctx, mib)), Some(b)) = (c.compute, blob.as_deref()) {
                        let _ = l.set_calibration(&format!("compute|{}|{ub}|{ctx}", blob_key(b)), mib);
                    }
                });
                // A partially offloaded load with no draft measures the RAM side of decoding.
                if let (Some(main), Some(tok_s), None, Some(bw_gpu)) =
                    (load.main(), load.decode_tok_s, a.draft.as_ref(), s.gpu.bandwidth_gbs)
                {
                    if main.offloaded.is_some_and(|(n, m)| n < m) {
                        let w_gpu: f64 = main.model.iter().filter(|(d, _)| log::is_gpu_device(d)).map(|(_, v)| v).sum();
                        let w_cpu = main.on_host() - a.main.weights.host as f64 / MIB;
                        let ms = 1000.0 / tok_s;
                        let t_gpu = w_gpu / 1024.0 / bw_gpu * 1000.0;
                        if ms > t_gpu && w_cpu > 0.0 {
                            let _ = self.with_library(|l| l.set_calibration("bw_ram", w_cpu / 1024.0 / ((ms - t_gpu) / 1000.0)));
                        }
                    }
                }
            }
            // Spill: shared memory well past the pinned host buffers means WDDM moved VRAM out.
            if s.gpu.runners_shared_mib > load.host_mib() + 1024.0 {
                warnings.push(format!(
                    "the driver may be spilling GPU memory into system RAM: the runner holds {:.1} GiB of shared memory against {:.1} GiB of pinned buffers",
                    s.gpu.runners_shared_mib / 1024.0,
                    load.host_mib() / 1024.0
                ));
            }
        }
        let calib = self.with_library(|l| {
            let compute = blob.as_deref().and_then(|b| l.calibration_like(&format!("compute|{}|{ub}|", blob_key(b))).ok().flatten());
            Calibration {
                driver_mib: l.calibration(&format!("driver|{gpu_name}")).ok().flatten(),
                compute: compute.and_then(|(k, v)| k.rsplit('|').next()?.parse().ok().map(|ctx| (ctx, v))),
                bw_ram_gbs: l.calibration("bw_ram").ok().flatten(),
            }
        });
        let spec_n = if a.draft.is_some() { in_effect.spec_n.unwrap_or(3) } else { 0 };
        let plan = build(&PlanInputs {
            main: &a.main,
            draft: a.draft.as_ref(),
            spec_n,
            draft_kv: CacheType::F16,
            ubatch: ub,
            mmproj_b: a.files.mmproj_bytes,
            gpu: snap.as_ref().map(|s| s.gpu.clone()),
            calib,
        });

        let saved = self.with_library(|l| l.policy(model))?;
        let kv = in_effect.next_kv_type().unwrap_or(KvType::F16);
        let parallel = in_effect.next_parallel().unwrap_or(1).max(1);
        let auto = plan.auto_ctx(kv, parallel).filter(|c| *c >= MIN_CTX).map(|c| ContextPolicy {
            max_ctx: c.min(plan.ctx_limit),
            kv_type: kv,
            parallel,
            source: PolicySource::Auto,
            updated_ms: 0,
        });
        if let Some(a) = auto.filter(|a| a.max_ctx < SMALL_AUTO_CTX) {
            warnings.push(format!(
                "the automatic choice is only {}k with {} agent{} at {}: long tasks will compact often",
                a.max_ctx / 1024,
                a.parallel,
                if a.parallel == 1 { "" } else { "s" },
                a.kv_type
            ));
        }
        let policy = match saved {
            Some(p) => p,
            None => ContextPolicy { max_ctx: self.default_window(model)?, kv_type: kv, parallel, source: PolicySource::Auto, updated_ms: 0 },
        };
        if in_effect.loaded {
            if let (Some(n), Some(slot)) = (in_effect.num_ctx, in_effect.slots.iter().find(|s| s.n_ctx != in_effect.slots[0].n_ctx)) {
                warnings.push(format!("runner slots differ in size ({} and {n} tokens)", slot.n_ctx));
            }
            let want = self.factory().allocations().get(&model.model_id).unwrap_or(policy.max_ctx);
            if let Some(slot) = in_effect.split_slot_ctx(want) {
                warnings.push(format!(
                    "Ollama split num_ctx {want} across {} slots: each holds only {slot} tokens, so agents on this model are held to {}",
                    in_effect.slots.len().max(1),
                    marlowe_proto::ctx_label(slot)
                ));
            } else if let Some(s) = in_effect.slots.first().filter(|s| s.n_ctx != want) {
                warnings.push(format!(
                    "the runner's slots hold {} tokens each, not the {want} requests ask for: it reloads on the next request",
                    s.n_ctx
                ));
            }
        }
        if kv != KvType::F16 && !a.main.quantizable() {
            warnings.push("this model's head size is not a multiple of 32: a quantized KV cache falls back to f16".into());
        }
        let rows = plan
            .row_contexts(Some(policy.max_ctx))
            .into_iter()
            .map(|ctx| PlanRow {
                ctx,
                per_agent_mib: KvType::ALL.map(|t| plan.per_agent_mib(ctx, t)),
                agents: KvType::ALL.map(|t| plan.agents(ctx, t)),
                fit: KvType::ALL.map(|t| plan.fit(ctx, t, 1)),
            })
            .collect();
        let arch = ArchSummary {
            arch: a.main.arch.clone(),
            params: a.main.params,
            quant: a.files.quant.clone().or_else(|| a.main.quant.clone()),
            layers: a.main.roles.clone(),
            summary: a.main.summary(),
            state_kind: a.main.recurrent.as_ref().map(|r| r.kind.to_string()),
            sliding_window: a.main.n_swa.map(|w| w as u32),
            mla: a.main.mla,
            draft: a.draft.as_ref().map(|d| {
                format!("{} layer{} with a KV cache", d.kv_layers(), if d.kv_layers() == 1 { "" } else { "s" })
            }),
            vision: a.main.weights.vision > 0 || a.files.mmproj_bytes > 0,
            source: a.files.source.clone(),
        };
        Ok(ContextPlan {
            model: model.clone(),
            ctx_limit: plan.ctx_limit,
            plan: Some(plan),
            arch,
            rows,
            in_effect,
            policy,
            auto,
            ram_total_mib: snap.as_ref().and_then(|s| s.ram_total_mib),
            ram_free_mib: snap.as_ref().and_then(|s| s.ram_free_mib),
            warnings,
        })
    }

    /// Every library model's window and (for the conductor's model) what the engine runs.
    pub fn context_policies(&self) -> Result<Vec<ModelContext>, BrainsError> {
        let models = self.with_library(|l| l.models())?;
        let conductor = self.resolve(&Slot::Conductor).ok().map(|(m, _)| m);
        let mut out = Vec::with_capacity(models.len());
        for lm in models {
            let m = lm.model;
            let policy = self.with_library(|l| l.policy(&m))?;
            let max_ctx = match policy {
                Some(p) => p.max_ctx,
                None => self.default_window(&m)?,
            };
            let in_effect = if conductor.as_ref() == Some(&m) {
                self.engine_now(&m)
            } else if self.is_local_ollama(&m) {
                EngineSettings::default()
            } else {
                EngineSettings { engine: Engine::Cloud, ..Default::default() }
            };
            out.push(ModelContext { model: m, max_ctx, policy, in_effect });
        }
        Ok(out)
    }

    /// Every model the local engine has loaded now (all unload when its server restarts).
    pub fn loaded_local_models(&self) -> Vec<String> {
        self.factory().hardware().map(|hw| hw.models.loaded_models()).unwrap_or_default()
    }

    /// Applies an approved server change. The daemon calls this only after an Allow.
    pub fn apply_server(&self, change: ServerChange) -> Result<String, BrainsError> {
        let hw = self.factory().hardware().ok_or_else(|| BrainsError::Invalid("no server control in this build".into()))?;
        let r = hw.server.apply(&change).map_err(BrainsError::Invalid);
        self.ctx.seen.lock().unwrap_or_else(|e| e.into_inner()).clear();
        r
    }
}
