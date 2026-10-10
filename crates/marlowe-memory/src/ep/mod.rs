//! Execution providers: which device runs an ONNX graph, decided each time a model loads.
//!
//! CPU is the shipped, measured scorer and the only provider in a default build. With the `cuda`
//! feature a loader may take CUDA, then DirectML, then CPU, and only while the GPU is idle
//! ([`judge_gpu`]): no other compute-only process on it, utilisation under [`IDLE_UTIL_PCT`], no
//! watched run live. Callers re-ask on every model (re)load, so a training job that starts while
//! a model is unloaded keeps the next load on CPU.
//!
//! Every registration uses `error_on_failure()`. A provider that does not construct is an error
//! that moves the loader to the next candidate and is reported in [`Choice::fallbacks`]; it is
//! never a CPU session labelled GPU (the old harness's Session G failure).
//!
//! The embedder is NOT routed through here: belief vectors persist, and a vector computed on a
//! different provider is a different scorer. Only the cross-encoder (and its cascade) moves.

#[cfg(feature = "cuda")]
pub mod runtime;

use ort::session::builder::SessionBuilder;
use ort::session::Session;

/// A device an ONNX graph can run on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    Cpu,
    Cuda,
    DirectMl,
}

impl Provider {
    /// The ONNX Runtime provider name. It reaches cache namespaces and run reports, so it is an
    /// identity, not a label.
    pub fn name(self) -> &'static str {
        match self {
            Provider::Cpu => "CPUExecutionProvider",
            Provider::Cuda => "CUDAExecutionProvider",
            Provider::DirectMl => "DmlExecutionProvider",
        }
    }

    /// The short form flags and reports use.
    pub fn short(self) -> &'static str {
        match self {
            Provider::Cpu => "cpu",
            Provider::Cuda => "cuda",
            Provider::DirectMl => "directml",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "cpu" => Some(Provider::Cpu),
            "cuda" => Some(Provider::Cuda),
            "directml" | "dml" => Some(Provider::DirectMl),
            _ => None,
        }
    }

    pub fn is_gpu(self) -> bool {
        !matches!(self, Provider::Cpu)
    }

    /// Whether this build can register the provider at all.
    pub fn compiled(self) -> bool {
        match self {
            Provider::Cpu => true,
            Provider::Cuda | Provider::DirectMl => cfg!(feature = "cuda"),
        }
    }

    /// Whether the cross-encoder scores a slate in one forward pass on this provider.
    ///
    /// Measured in the old harness (M0c Session L, warm-249 held-out, ranking identical in every
    /// cell): at one CPU thread batching LOSES (rerank p50 185.8 ms sequential vs 195.6 batched);
    /// on CUDA it WINS (15.2 -> 3.4 ms). DirectML was never measured and follows the GPU reading.
    pub fn default_batching(self) -> bool {
        self.is_gpu()
    }
}

/// Utilisation at or above which a GPU is not idle.
pub const IDLE_UTIL_PCT: u32 = 10;

/// One GPU as the Sentinel last sampled it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuLoad {
    pub name: String,
    pub util_pct: u32,
    /// Processes doing compute only (NVML compute list minus graphics list), other than this one.
    /// Desktop apps show up as compute+graphics and are not counted; a training job or an
    /// inference server is compute-only.
    pub compute_only: Vec<String>,
}

/// Whether a GPU may be borrowed now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpuVerdict {
    Idle,
    Busy(String),
    /// No Sentinel, no NVML, no device: unknown is treated as not idle.
    Unknown(String),
}

impl GpuVerdict {
    pub fn is_idle(&self) -> bool {
        matches!(self, GpuVerdict::Idle)
    }
}

/// Judge the Sentinel's sample: idle only if every device is under [`IDLE_UTIL_PCT`], no device
/// carries another compute-only process, and no watched run is live.
pub fn judge_gpu(devices: &[GpuLoad], live_runs: usize) -> GpuVerdict {
    if devices.is_empty() {
        return GpuVerdict::Unknown("no GPU reported".into());
    }
    if live_runs > 0 {
        return GpuVerdict::Busy(format!("{live_runs} watched run(s) live"));
    }
    for d in devices {
        if let Some(p) = d.compute_only.first() {
            return GpuVerdict::Busy(format!("{} is running compute on {}", p, d.name));
        }
        if d.util_pct >= IDLE_UTIL_PCT {
            return GpuVerdict::Busy(format!("{} at {}% utilisation (idle is < {IDLE_UTIL_PCT}%)", d.name, d.util_pct));
        }
    }
    GpuVerdict::Idle
}

/// What a loader asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// The shipped scorer.
    Cpu,
    /// CUDA, then DirectML, then CPU, while the GPU is idle; CPU otherwise.
    Auto,
    /// Exactly this provider or an error. Measurement only: a run labelled CUDA must be CUDA.
    Require(Provider),
}

/// The providers to try for `request`, in order, and why the list is what it is.
pub fn plan(request: Request, gpu: &GpuVerdict) -> (Vec<Provider>, String) {
    match request {
        Request::Cpu => (vec![Provider::Cpu], "CPU requested".into()),
        Request::Require(p) => (vec![p], format!("{} required", p.short())),
        Request::Auto if !cfg!(feature = "cuda") => {
            (vec![Provider::Cpu], "built without the `cuda` feature".into())
        }
        Request::Auto => match gpu {
            GpuVerdict::Idle => {
                (vec![Provider::Cuda, Provider::DirectMl, Provider::Cpu], "GPU idle".into())
            }
            GpuVerdict::Busy(why) => (vec![Provider::Cpu], format!("GPU busy: {why}")),
            GpuVerdict::Unknown(why) => (vec![Provider::Cpu], format!("GPU state unknown: {why}")),
        },
    }
}

/// The provider a loader ended up on, and every candidate that failed before it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub provider: Provider,
    pub reason: String,
    pub fallbacks: Vec<(Provider, String)>,
}

impl Choice {
    /// One line for logs and `stats`.
    pub fn describe(&self) -> String {
        let mut s = format!("{} ({})", self.provider.short(), self.reason);
        for (p, e) in &self.fallbacks {
            s.push_str(&format!("; {} failed: {e}", p.short()));
        }
        s
    }
}

/// Try each planned provider in turn with `make` until one constructs.
///
/// Errors from candidates before the last are recorded in the [`Choice`]; the last candidate's
/// error is returned as-is, so a `Require` or a CPU failure surfaces unchanged.
pub fn first_that_builds<T, E: std::fmt::Display>(
    request: Request,
    gpu: &GpuVerdict,
    mut make: impl FnMut(Provider) -> Result<T, E>,
) -> Result<(T, Choice), E> {
    let (order, reason) = plan(request, gpu);
    let mut fallbacks = Vec::new();
    let last = order.len() - 1;
    for (i, provider) in order.into_iter().enumerate() {
        match make(provider) {
            Ok(t) => return Ok((t, Choice { provider, reason, fallbacks })),
            Err(e) if i < last => fallbacks.push((provider, e.to_string())),
            Err(e) => return Err(e),
        }
    }
    unreachable!("plan() never returns an empty order")
}

/// A session builder, after making sure the runtime is loaded (ARM64 `cuda` builds load ONNX
/// Runtime from a DLL at run time; every other build links it).
pub fn session_builder() -> ort::Result<SessionBuilder> {
    #[cfg(feature = "cuda")]
    runtime::ensure_core().map_err(ort::Error::new)?;
    Session::builder()
}

/// Register `provider` on `builder`, failing rather than falling back.
pub fn register(builder: SessionBuilder, provider: Provider) -> ort::Result<SessionBuilder> {
    use ort::ep::CPU;
    match provider {
        Provider::Cpu => {
            Ok(builder.with_execution_providers([CPU::default().build().error_on_failure()])?)
        }
        #[cfg(feature = "cuda")]
        Provider::Cuda => runtime::register_cuda(builder),
        #[cfg(feature = "cuda")]
        Provider::DirectMl => {
            use ort::ep::DirectML;
            // DirectML requires sequential execution (the default) and no memory pattern.
            let b = builder.with_memory_pattern(false)?;
            Ok(b.with_execution_providers([DirectML::default().build().error_on_failure()])?)
        }
        #[cfg(not(feature = "cuda"))]
        p => Err(ort::Error::new(format!(
            "{} is not available: this build has no `cuda` feature (CPU only)",
            p.name()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu(util: u32, compute: &[&str]) -> GpuLoad {
        GpuLoad { name: "RTX".into(), util_pct: util, compute_only: compute.iter().map(|s| s.to_string()).collect() }
    }

    #[test]
    fn idle_needs_low_util_no_compute_process_and_no_live_run() {
        assert_eq!(judge_gpu(&[gpu(3, &[])], 0), GpuVerdict::Idle);
        assert!(matches!(judge_gpu(&[gpu(37, &[])], 0), GpuVerdict::Busy(_)));
        assert!(matches!(judge_gpu(&[gpu(10, &[])], 0), GpuVerdict::Busy(_)), "10% is not under 10%");
        assert!(matches!(judge_gpu(&[gpu(0, &["python.exe"])], 0), GpuVerdict::Busy(_)));
        assert!(matches!(judge_gpu(&[gpu(0, &[])], 1), GpuVerdict::Busy(_)));
        assert!(matches!(judge_gpu(&[], 0), GpuVerdict::Unknown(_)));
        assert!(matches!(judge_gpu(&[gpu(0, &[]), gpu(50, &[])], 0), GpuVerdict::Busy(_)), "every device");
    }

    #[test]
    fn auto_takes_the_gpu_only_when_idle_and_compiled() {
        let (busy, _) = plan(Request::Auto, &GpuVerdict::Busy("x".into()));
        assert_eq!(busy, vec![Provider::Cpu]);
        let (unknown, _) = plan(Request::Auto, &GpuVerdict::Unknown("x".into()));
        assert_eq!(unknown, vec![Provider::Cpu]);
        let (idle, _) = plan(Request::Auto, &GpuVerdict::Idle);
        if cfg!(feature = "cuda") {
            assert_eq!(idle, vec![Provider::Cuda, Provider::DirectMl, Provider::Cpu]);
        } else {
            assert_eq!(idle, vec![Provider::Cpu]);
        }
        assert_eq!(plan(Request::Cpu, &GpuVerdict::Idle).0, vec![Provider::Cpu]);
        assert_eq!(plan(Request::Require(Provider::Cuda), &GpuVerdict::Busy("x".into())).0, vec![Provider::Cuda]);
    }

    #[test]
    fn fallbacks_are_recorded_and_the_last_error_is_returned() {
        let (v, choice) = first_that_builds(Request::Auto, &GpuVerdict::Idle, |p| match p {
            Provider::Cpu => Ok(1),
            other => Err(format!("{} missing", other.short())),
        })
        .unwrap();
        assert_eq!(v, 1);
        assert_eq!(choice.provider, Provider::Cpu);
        if cfg!(feature = "cuda") {
            assert_eq!(choice.fallbacks.len(), 2);
            assert!(choice.describe().contains("cuda failed: cuda missing"));
        }
        let err = first_that_builds(Request::Require(Provider::Cuda), &GpuVerdict::Idle, |_| {
            Err::<(), _>("no runtime".to_string())
        })
        .unwrap_err();
        assert_eq!(err, "no runtime");
    }

    #[test]
    fn batching_follows_the_measurement() {
        assert!(!Provider::Cpu.default_batching());
        assert!(Provider::Cuda.default_batching());
        assert_eq!(Provider::parse("CUDA"), Some(Provider::Cuda));
        assert_eq!(Provider::parse("dml"), Some(Provider::DirectMl));
        assert_eq!(Provider::parse("tpu"), None);
    }

    #[test]
    fn a_cpu_only_build_refuses_gpu_providers_by_name() {
        if cfg!(feature = "cuda") {
            return;
        }
        let err = Session::builder().and_then(|b| register(b, Provider::Cuda)).err().expect("refused");
        assert!(err.to_string().contains("no `cuda` feature"), "{err}");
    }
}
