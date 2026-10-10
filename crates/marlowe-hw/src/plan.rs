//! The VRAM budget and what fits in it.
//!
//! ```text
//! free_for_marlowe = NVML.free + Σ dedicated(runner pids)
//! others           = NVML.used − Σ dedicated(runner pids)
//! budget           = free_for_marlowe − headroom            (1024 MiB: the runner's fit target)
//! fixed(c)         = W_gpu(main) + W_gpu(draft) + W(vision) + compute(c) + driver
//! per_agent(c, t)  = pad256(c)·(KV/tok(t) + draftKV/tok) + state·(1 + n_draft) + SWA cells
//! agents_fit(c, t) = floor((budget − fixed(c)) / per_agent(c, t))
//! ```
//!
//! [`PlanModel`] is that model with every term filled in. It is serialisable so the client can
//! move the sliders without a round trip, and both sides compute with the same functions.

use serde::{Deserialize, Serialize};

use crate::arch::{pad256, ArchProfile};
use crate::ggml::{CacheType, KvType};

pub const MIB: f64 = 1024.0 * 1024.0;
/// What the runner keeps free (`--fit-target`, llama-server's default).
pub const HEADROOM_MIB: f64 = 1024.0;
/// CUDA context, cuBLAS and friends per runner process, until a load measures it.
pub const DRIVER_MIB: f64 = 512.0;
/// System RAM bandwidth assumed until a measured decode speed corrects it.
pub const BW_RAM_GBS: f64 = 60.0;
/// llama-server's prompt cache cap (`--cache-ram`), MiB, and its checkpoints per slot.
pub const PROMPT_CACHE_MIB: f64 = 8192.0;
pub const CHECKPOINTS_PER_SLOT: u32 = 32;
/// The most agents the planner will count.
pub const MAX_AGENTS: u16 = 16;
/// A partial offload slower than this at writing is reported as not fitting.
pub const MAX_SLOWDOWN: f64 = 4.0;
/// Context lengths the table shows.
pub const ROWS: [u32; 6] = [8_192, 16_384, 32_768, 65_536, 131_072, 262_144];

/// The GPU as measured now. All MiB.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpuBudget {
    pub name: String,
    pub total_mib: f64,
    /// NVML's used: every process, the desktop included.
    pub used_mib: f64,
    pub free_mib: f64,
    /// Dedicated memory of the model runners (PDH, per pid).
    pub runners_mib: f64,
    /// Shared (system) memory the runners hold: pinned host buffers, or spill under WDDM.
    pub runners_shared_mib: f64,
    pub headroom_mib: f64,
    /// Peak memory bandwidth: bus width × max memory clock × 2.
    pub bandwidth_gbs: Option<f64>,
}

impl GpuBudget {
    pub fn others_mib(&self) -> f64 {
        (self.used_mib - self.runners_mib).max(0.0)
    }

    pub fn free_for_marlowe_mib(&self) -> f64 {
        self.free_mib + self.runners_mib
    }

    pub fn budget_mib(&self) -> f64 {
        self.free_for_marlowe_mib() - self.headroom_mib
    }
}

/// What a load measured, kept between plans (the `hw_calibration` table).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Calibration {
    /// PDH dedicated − Σ device buffers, MiB.
    pub driver_mib: Option<f64>,
    /// GPU compute buffers (main + draft) measured at this context, MiB.
    pub compute: Option<(u32, f64)>,
    /// Effective RAM bandwidth from a measured partially offloaded decode, GB/s.
    pub bw_ram_gbs: Option<f64>,
}

/// How a configuration lands.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "fit", rename_all = "snake_case")]
pub enum Fit {
    /// Fully on the GPU; `spare_mib` left under the budget.
    Fits { spare_mib: f64 },
    /// The runner would move `cpu_layers` blocks (and their caches) to the CPU.
    Partial { gpu_layers: u32, total_layers: u32, slowdown: f64 },
    /// Under half the layers would stay on the GPU, writing would be over [`MAX_SLOWDOWN`] times
    /// slower, or nothing fits at all.
    NoFit { gpu_layers: u32, total_layers: u32 },
    /// No GPU measurement: nothing to compare against.
    Unknown,
}

impl Fit {
    pub fn fits(&self) -> bool {
        matches!(self, Fit::Fits { .. })
    }
}

/// Everything the planner knows, as a linear model in the context length.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanModel {
    pub gpu: Option<GpuBudget>,
    /// GPU weights of the model (all blocks and the output layer), MiB.
    pub weights_main_mib: f64,
    pub weights_draft_mib: f64,
    pub vision_mib: f64,
    /// GPU compute buffers without the attention-mask term, MiB.
    pub compute_base_mib: f64,
    /// Compute growth per context token (the mask: `n_ubatch × 2 B`, main and draft).
    pub compute_per_token_b: f64,
    pub driver_mib: f64,
    /// Main KV bytes per token per sequence, by [`KvType::index`].
    pub kv_main_token_b: [u64; 3],
    /// The draft's KV per token (its own type; Ollama leaves it f16).
    pub draft_kv_token_b: u64,
    /// Recurrent state per sequence (one copy), bytes.
    pub state_b: u64,
    /// Copies of the state per sequence: 1, or 1 + draft positions under speculative decoding.
    pub state_copies: u32,
    /// Sliding-window cache per sequence at the runner's micro-batch, bytes per KV type.
    pub swa_agent_b: [u64; 3],
    /// Host RAM the load pins or holds regardless of agents (input embeddings, host compute), MiB.
    pub host_fixed_mib: f64,
    /// One context checkpoint (hybrid/SWA models), MiB; 0 when the model takes none.
    pub checkpoint_mib: f64,
    /// The model's trained context.
    pub ctx_limit: u32,
    pub ubatch: u32,
    /// Blocks plus the output layer (llama.cpp's `offloaded N/M`).
    pub layers_total: u32,
    /// Mean GPU weights per block, MiB.
    pub layer_weight_mib: f64,
    pub calibrated: bool,
    pub bw_ram_gbs: f64,
    /// A draft model is part of the load (speculative decoding).
    #[serde(default)]
    pub has_draft: bool,
}

impl PlanModel {
    /// The same model loaded without its draft (Ollama's tag without the DRAFT line): no draft
    /// weights or KV, one copy of the state, the main context's compute only.
    pub fn without_draft(&self) -> PlanModel {
        if !self.has_draft {
            return self.clone();
        }
        PlanModel {
            weights_draft_mib: 0.0,
            draft_kv_token_b: 0,
            state_copies: 1,
            compute_base_mib: self.compute_base_mib / 1.5,
            compute_per_token_b: self.compute_per_token_b / 1.5,
            has_draft: false,
            ..self.clone()
        }
    }

    pub fn budget_mib(&self) -> Option<f64> {
        self.gpu.as_ref().map(GpuBudget::budget_mib)
    }

    /// Fixed costs at context `ctx` (per sequence), MiB.
    pub fn fixed_mib(&self, ctx: u32) -> f64 {
        self.weights_main_mib
            + self.weights_draft_mib
            + self.vision_mib
            + self.compute_base_mib
            + pad256(u64::from(ctx)) as f64 * self.compute_per_token_b / MIB
            + self.driver_mib
    }

    pub fn compute_mib(&self, ctx: u32) -> f64 {
        self.compute_base_mib + pad256(u64::from(ctx)) as f64 * self.compute_per_token_b / MIB
    }

    /// KV bytes per token per agent, draft included.
    pub fn kv_token_b(&self, kv: KvType) -> u64 {
        self.kv_main_token_b[kv.index()] + self.draft_kv_token_b
    }

    /// Bytes per agent that do not grow with the context: state copies and sliding-window cells.
    pub fn agent_const_b(&self, kv: KvType) -> u64 {
        self.state_b * u64::from(self.state_copies) + self.swa_agent_b[kv.index()]
    }

    pub fn per_agent_mib(&self, ctx: u32, kv: KvType) -> f64 {
        (pad256(u64::from(ctx)) * self.kv_token_b(kv) + self.agent_const_b(kv)) as f64 / MIB
    }

    /// GPU memory for `n` agents at `ctx`, MiB.
    pub fn need_mib(&self, ctx: u32, kv: KvType, n: u16) -> f64 {
        self.fixed_mib(ctx) + f64::from(n) * self.per_agent_mib(ctx, kv)
    }

    /// How many agents of `ctx` fit fully on the GPU. `None` without a GPU measurement.
    pub fn agents(&self, ctx: u32, kv: KvType) -> Option<u16> {
        let left = self.budget_mib()? - self.fixed_mib(ctx);
        let per = self.per_agent_mib(ctx, kv);
        if left <= 0.0 || per <= 0.0 {
            return Some(0);
        }
        Some(((left / per).floor() as u64).min(u64::from(MAX_AGENTS)) as u16)
    }

    /// The largest context (a multiple of `step`) at which `n` agents fit, capped at the model's
    /// trained context. `None` without a GPU; `Some(0)` when not even `step` fits.
    pub fn max_ctx(&self, kv: KvType, n: u16, step: u32) -> Option<u32> {
        let budget = self.budget_mib()?;
        let n = f64::from(n.max(1));
        let left = (budget - self.fixed_mib(0)) * MIB - n * self.agent_const_b(kv) as f64;
        let per_token = self.compute_per_token_b + n * self.kv_token_b(kv) as f64;
        if left <= 0.0 || per_token <= 0.0 {
            return Some(0);
        }
        let raw = (left / per_token).floor().min(f64::from(self.ctx_limit)) as u32;
        let mut c = raw / step * step;
        // Padding to 256 can push the last step over; walk back.
        while c > 0 && self.need_mib(c, kv, n as u16) > budget {
            c -= step;
        }
        Some(c)
    }

    /// The automatic choice: the largest multiple of 4096 at which `n` agents fit.
    pub fn auto_ctx(&self, kv: KvType, n: u16) -> Option<u32> {
        self.max_ctx(kv, n, 4096)
    }

    /// How `n` agents at `ctx` land.
    pub fn fit(&self, ctx: u32, kv: KvType, n: u16) -> Fit {
        let Some(budget) = self.budget_mib() else { return Fit::Unknown };
        let need = self.need_mib(ctx, kv, n);
        if need <= budget {
            return Fit::Fits { spare_mib: budget - need };
        }
        // Back to front, as the runner's fit step does: each block moved to the CPU takes its
        // weights and its share of every agent's cache with it.
        let blocks = self.layers_total.saturating_sub(1).max(1);
        let cache = f64::from(n) * self.per_agent_mib(ctx, kv);
        let per_layer = self.layer_weight_mib + cache / f64::from(blocks);
        let over = need - budget;
        let cpu = if per_layer > 0.0 { (over / per_layer).ceil() as u32 } else { blocks };
        let gpu_layers = self.layers_total.saturating_sub(cpu.min(self.layers_total));
        let slowdown = self.slowdown(cpu);
        if cpu >= blocks || gpu_layers * 2 < self.layers_total || slowdown > MAX_SLOWDOWN {
            return Fit::NoFit { gpu_layers, total_layers: self.layers_total };
        }
        Fit::Partial { gpu_layers, total_layers: self.layers_total, slowdown }
    }

    /// Decode time with `cpu_layers` blocks on the CPU over the time fully on the GPU:
    /// `ms/token ≈ W_gpu/BW_gpu + W_cpu/BW_ram`.
    pub fn slowdown(&self, cpu_layers: u32) -> f64 {
        let bw_gpu = self.gpu.as_ref().and_then(|g| g.bandwidth_gbs).unwrap_or(500.0);
        let w_all = self.weights_main_mib;
        let w_cpu = (f64::from(cpu_layers) * self.layer_weight_mib).min(w_all);
        let full = w_all / bw_gpu;
        let partial = (w_all - w_cpu) / bw_gpu + w_cpu / self.bw_ram_gbs;
        if full > 0.0 {
            partial / full
        } else {
            1.0
        }
    }

    /// Host RAM the configuration can hold at most: fixed host buffers, checkpoints per slot and
    /// the prompt-cache cap, MiB.
    pub fn host_mib(&self, n: u16) -> f64 {
        self.host_fixed_mib + f64::from(n) * f64::from(CHECKPOINTS_PER_SLOT) * self.checkpoint_mib + PROMPT_CACHE_MIB
    }

    /// The table rows up to the model's trained context, plus `extra` (the current setting).
    pub fn row_contexts(&self, extra: Option<u32>) -> Vec<u32> {
        let mut v: Vec<u32> = ROWS.iter().copied().filter(|c| *c <= self.ctx_limit.max(8192)).collect();
        if let Some(e) = extra.filter(|e| *e > 0 && !v.contains(e)) {
            v.push(e);
            v.sort_unstable();
        }
        v
    }
}

/// Everything a plan is built from.
#[derive(Debug, Clone)]
pub struct PlanInputs<'a> {
    pub main: &'a ArchProfile,
    /// The draft model, when speculative decoding is on for this model.
    pub draft: Option<&'a ArchProfile>,
    /// Draft positions per step (`--spec-draft-n-max`).
    pub spec_n: u32,
    /// The draft context's KV type (Ollama passes no `-ctkd`, so f16).
    pub draft_kv: CacheType,
    pub ubatch: u32,
    /// A separate projector file (`--mmproj`), bytes.
    pub mmproj_b: u64,
    pub gpu: Option<GpuBudget>,
    pub calib: Calibration,
}

/// Fills in the model.
pub fn build(inp: &PlanInputs<'_>) -> PlanModel {
    let m = inp.main;
    let ub = u64::from(inp.ubatch.max(1));
    let kv_main_token_b = KvType::ALL.map(|t| m.kv_per_token(t.cache(), t.cache()));
    let (draft_kv_token_b, weights_draft, draft_host, draft_swa) = match inp.draft {
        Some(d) => (
            d.kv_per_token(inp.draft_kv, inp.draft_kv),
            d.weights.gpu() as f64 / MIB,
            d.weights.host as f64 / MIB,
            d.swa_cells(1 << 20, ub) * d.swa_per_cell(inp.draft_kv, inp.draft_kv),
        ),
        None => (0, 0.0, 0.0, 0),
    };
    let spec = inp.draft.is_some() && inp.spec_n > 0;
    let state_copies = if spec && m.recurrent.is_some() { 1 + inp.spec_n } else { 1 };
    let swa_agent_b = KvType::ALL.map(|t| m.swa_cells(1 << 20, ub) * m.swa_per_cell(t.cache(), t.cache()) + draft_swa);

    // Compute: ~96 B per ubatch row per embedding element, plus the mask (ctx × ubatch × 2 B);
    // a draft context reserves about half again.
    let draft_share = if inp.draft.is_some() { 1.5 } else { 1.0 };
    let est_base = 96.0 * ub as f64 * m.n_embd as f64 * draft_share / MIB;
    let per_token = ub as f64 * 2.0 * draft_share;
    let compute_base_mib = match inp.calib.compute {
        Some((ctx, mib)) => (mib - pad256(u64::from(ctx)) as f64 * per_token / MIB).max(0.0),
        None => est_base,
    };
    let vision_w = (m.weights.vision + inp.mmproj_b) as f64 / MIB;
    // The vision encoder reserves its own compute at load: estimated until a load is measured.
    let vision_mib = if vision_w > 0.0 { vision_w + 256.0 + vision_w * 0.25 } else { 0.0 };
    let blocks = m.weights.per_block.len().max(1) as f64;
    let checkpoint_mib = if m.recurrent.is_some() || m.n_swa.is_some() {
        (m.state_per_seq() + swa_agent_b[KvType::F16.index()]) as f64 / MIB
    } else {
        0.0
    };
    PlanModel {
        gpu: inp.gpu.clone(),
        weights_main_mib: m.weights.gpu() as f64 / MIB,
        weights_draft_mib: weights_draft,
        vision_mib,
        compute_base_mib,
        compute_per_token_b: per_token,
        driver_mib: inp.calib.driver_mib.unwrap_or(DRIVER_MIB),
        kv_main_token_b,
        draft_kv_token_b,
        state_b: m.state_per_seq(),
        state_copies,
        swa_agent_b,
        host_fixed_mib: m.weights.host as f64 / MIB + draft_host + 64.0,
        checkpoint_mib,
        ctx_limit: m.n_ctx_train.unwrap_or(32_768),
        ubatch: inp.ubatch,
        layers_total: m.n_blocks + 1,
        layer_weight_mib: m.weights.per_block.iter().sum::<u64>() as f64 / MIB / blocks,
        calibrated: inp.calib.driver_mib.is_some() || inp.calib.compute.is_some(),
        bw_ram_gbs: inp.calib.bw_ram_gbs.unwrap_or(BW_RAM_GBS),
        has_draft: inp.draft.is_some(),
    }
}

/// `46k`, `128k`, `200k`, `2048`: a whole number of 1024s reads in those (`131072` is 128k), a
/// hosted model's round thousands in thousands (`200000` is 200k), anything else in whole 1024s.
pub fn ctx_label(ctx: u32) -> String {
    if ctx < 1024 {
        ctx.to_string()
    } else if ctx.is_multiple_of(1024) || !ctx.is_multiple_of(1000) {
        format!("{}k", ctx / 1024)
    } else {
        format!("{}k", ctx / 1000)
    }
}

/// `9.8` (GiB, one decimal) from MiB.
pub fn gib(mib: f64) -> String {
    format!("{:.1}", mib / 1024.0)
}

#[cfg(test)]
mod tests {
    use super::ctx_label;

    #[test]
    fn context_labels() {
        assert_eq!(ctx_label(131_072), "128k");
        assert_eq!(ctx_label(47_104), "46k");
        assert_eq!(ctx_label(200_000), "200k");
        assert_eq!(ctx_label(47_628), "46k");
        assert_eq!(ctx_label(512), "512");
    }
}
