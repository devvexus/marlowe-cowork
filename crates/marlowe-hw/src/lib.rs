//! The context and hardware planner: what a model costs on this machine, before it loads.
//!
//! * [`gguf`] reads a GGUF header (key-values and tensor infos, never the tensor data);
//!   [`ModelHeader::from_show`] reads the same from Ollama's `/api/show`.
//! * [`arch`] turns a header into an [`ArchProfile`]: which layers keep a KV cache, bytes per
//!   token, the fixed recurrent state, sliding windows, MLA, and where every weight lives.
//! * [`plan`] is the VRAM budget and the linear [`PlanModel`] the daemon and the client share.
//! * [`log`] parses Ollama's `server.log`: the runner's argv and every buffer it allocated, which
//!   is the ground truth the plan is calibrated against.
//!
//! Ollama's `/api/ps` is **not** ground truth for these runners: on 2026-10-09 it reported
//! 1,631 MiB for a runner whose dedicated memory was 13,486 MiB (the draft and the recurrent
//! state only). Nothing here reads its sizes.
//!
//! Pure: no I/O but [`gguf::read_header`], no clocks, no GPU calls.

#![forbid(unsafe_code)]

pub mod arch;
pub mod ggml;
pub mod gguf;
pub mod header;
pub mod log;
pub mod plan;
pub mod source;

pub use arch::{ArchProfile, LayerRole, Recurrent, Role, Weights};
pub use ggml::{CacheType, GgmlType, KvType};
pub use header::{ModelHeader, Tensor};
pub use plan::{build, ctx_label, Calibration, Fit, GpuBudget, PlanInputs, PlanModel};
pub use source::{
    GpuSnapshot, GpuSource, ModelFiles, ModelSource, RunnerObservation, ServerChange, ServerControl, ServerEnv, SlotObservation,
};

/// What the planner expects each buffer of a load to be, MiB, for checking a plan against the
/// runner's own log.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Predicted {
    pub main_gpu_weights: f64,
    pub main_host_weights: f64,
    pub main_kv: f64,
    pub main_state: f64,
    pub draft_gpu_weights: f64,
    pub draft_host_weights: f64,
    pub draft_kv: f64,
}

/// The buffers a load of `main` (and `draft`) at `ctx` × `seqs` with `kv` would allocate.
#[allow(clippy::too_many_arguments)]
pub fn predict(main: &ArchProfile, draft: Option<&ArchProfile>, ctx: u32, seqs: u32, kv: CacheType, draft_kv: CacheType, spec_n: u32, ubatch: u32) -> Predicted {
    let mib = plan::MIB;
    let ctx = arch::pad256(u64::from(ctx));
    let seqs = u64::from(seqs.max(1));
    let copies = if draft.is_some() && spec_n > 0 && main.recurrent.is_some() { 1 + u64::from(spec_n) } else { 1 };
    let ub = u64::from(ubatch);
    Predicted {
        main_gpu_weights: main.weights.gpu() as f64 / mib,
        main_host_weights: main.weights.host as f64 / mib,
        main_kv: (seqs * (ctx * main.kv_per_token(kv, kv) + main.swa_cells(ctx, ub) * main.swa_per_cell(kv, kv))) as f64 / mib,
        main_state: (seqs * main.state_per_seq() * copies) as f64 / mib,
        draft_gpu_weights: draft.map_or(0.0, |d| d.weights.gpu() as f64 / mib),
        draft_host_weights: draft.map_or(0.0, |d| d.weights.host as f64 / mib),
        draft_kv: draft.map_or(0.0, |d| (seqs * ctx * d.kv_per_token(draft_kv, draft_kv)) as f64 / mib),
    }
}

/// What a measured load teaches the planner: the compute buffers at that context, and the
/// driver's share (`PDH dedicated − Σ device buffers`) when the runner's dedicated memory is known.
pub fn calibrate(block: &log::LoadBlock, dedicated_mib: Option<f64>) -> Calibration {
    let compute: f64 = block.contexts.iter().map(|c| c.compute.iter().filter(|(d, _)| log::is_gpu_device(d)).map(|(_, m)| m).sum::<f64>()).sum();
    let ctx = block.main().and_then(|c| c.n_ctx_seq.or(c.n_ctx)).or(block.launch.ctx);
    let driver = dedicated_mib.map(|d| d - block.gpu_mib()).filter(|d| (0.0..4096.0).contains(d));
    Calibration { driver_mib: driver, compute: ctx.filter(|_| compute > 0.0).map(|c| (c, compute)), bw_ram_gbs: None }
}
