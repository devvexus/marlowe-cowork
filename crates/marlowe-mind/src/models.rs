//! Lazily loaded ONNX models and the degraded-mode flag.
//!
//! Each model loads on first need and unloads after `idle` unused. The embedder is always CPU
//! (belief vectors persist). The cross-encoder picks its provider at every load through
//! [`marlowe_memory::ep`]: with the `cuda` feature and an idle GPU (per the Sentinel's probe) it
//! loads on CUDA, then DirectML, and brings the six-graph cascade with it; otherwise CPU and the
//! shipped single graph. A GPU-resident reranker re-asks the probe every [`GPU_RECHECK`] and
//! moves back to CPU as soon as the GPU stops being idle. A missing model, or one that
//! fails its pinned digest, puts the service in degraded mode for the rest of the process:
//! lexical recall only, no auto-injection. The reason is kept and reported by `stats`. Other
//! load failures (a file locked by a scanner, an ONNX session error) fail that call only and are
//! retried on the next need. No embedding cache: belief vectors persist in `vecfile`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use marlowe_memory::cascade::Cascade;
use marlowe_memory::cue::dense::embedder::{self, EmbedError, Embedder};
use marlowe_memory::ep::{self, Choice, GpuVerdict, Request};
use marlowe_memory::rerank::{self, CrossEncoder, RerankError, SHIPPED_THREADS};

use crate::paths::{EMBEDDER_DIR, RERANKER_DIR};
use crate::types::GpuProbe;

/// How often a GPU-resident reranker re-checks that the GPU is still idle.
pub(crate) const GPU_RECHECK: Duration = Duration::from_secs(30);

/// The loaded cross-encoder, the provider it landed on, and the cascade when on a GPU.
pub(crate) struct Reranker {
    pub encoder: CrossEncoder,
    pub cascade: Option<Cascade>,
    pub choice: Choice,
    checked: Instant,
}

/// With `cuda`, an installed build finds its GPU runtime (scripts/fetch-gpu-runtime.ps1 -Dest
/// <home>\gpu-runtime) at `<home>\gpu-runtime`, unless `$MARLOWE_GPU_RUNTIME` names one. The dev
/// tree's `<repo>\gpu-runtime` remains the last resort (`ep::runtime::root`).
#[cfg(feature = "cuda")]
pub(crate) fn use_home_gpu_runtime(home: &std::path::Path) {
    use marlowe_memory::ep::runtime;
    let dir = home.join("gpu-runtime");
    let env_set = std::env::var_os(runtime::ROOT_ENV).is_some_and(|v| !v.is_empty());
    if !env_set && dir.is_dir() {
        runtime::set_root(dir);
    }
}

#[cfg(not(feature = "cuda"))]
pub(crate) fn use_home_gpu_runtime(_home: &std::path::Path) {}

/// One embedder worker: the queue runs at idle priority and RAM matters more than throughput.
const EMBED_WORKERS: usize = 1;

pub(crate) struct Models {
    dir: PathBuf,
    idle: Duration,
    embedder: Option<(Embedder, Instant)>,
    reranker: Option<(Reranker, Instant)>,
    degraded: Option<String>,
    gpu: Option<GpuProbe>,
}

impl Models {
    /// Checks the files exist; loads nothing.
    pub fn new(dir: PathBuf, idle: Duration, gpu: Option<GpuProbe>) -> Self {
        let required = [
            (EMBEDDER_DIR, embedder::MODEL_FILE),
            (EMBEDDER_DIR, embedder::VOCAB_FILE),
            (RERANKER_DIR, rerank::MODEL_FILE),
            (RERANKER_DIR, rerank::TOKENIZER_FILE),
        ];
        let missing: Vec<String> = required
            .iter()
            .filter(|(sub, file)| !dir.join(sub).join(file).is_file())
            .map(|(sub, file)| format!("{sub}\\{file}"))
            .collect();
        let degraded = (!missing.is_empty()).then(|| {
            format!(
                "models missing from {}: {}. Recall is lexical only and auto-injection is off",
                dir.display(),
                missing.join(", ")
            )
        });
        if let Some(reason) = &degraded {
            eprintln!("marlowe-mind: degraded: {reason}");
        }
        Self { dir, idle, embedder: None, reranker: None, degraded, gpu }
    }

    /// Why the service is degraded, if it is.
    pub fn degraded(&self) -> Option<&str> {
        self.degraded.as_deref()
    }

    fn fail(&mut self, what: &str, err: String) -> String {
        let reason = format!("{what} failed to load: {err}. Recall is lexical only and auto-injection is off");
        eprintln!("marlowe-mind: degraded: {reason}");
        self.degraded = Some(reason.clone());
        reason
    }

    /// The embedder, loading it if needed.
    pub fn embedder(&mut self) -> Result<&mut Embedder, String> {
        if let Some(r) = &self.degraded {
            return Err(r.clone());
        }
        if self.embedder.is_none() {
            let dir = self.dir.join(EMBEDDER_DIR);
            match Embedder::load(&dir, EMBED_WORKERS, None) {
                Ok(e) => self.embedder = Some((e, Instant::now())),
                Err(e) if embed_permanent(&e) => return Err(self.fail("the embedder", e.to_string())),
                Err(e) => return Err(format!("the embedder did not load (will retry): {e}")),
            }
        }
        let slot = self.embedder.as_mut().expect("loaded above");
        slot.1 = Instant::now();
        Ok(&mut slot.0)
    }

    /// Whether the embedder is loaded, so a recall can tell a cold load from a warm one.
    pub fn embedder_resident(&self) -> bool {
        self.embedder.is_some()
    }

    pub fn reranker_resident(&self) -> bool {
        self.reranker.is_some()
    }

    /// Size of the embedder's weights on disk (what a cold load reads), 0 if unreadable.
    pub fn embedder_bytes(&self) -> u64 {
        std::fs::metadata(self.dir.join(EMBEDDER_DIR).join(embedder::MODEL_FILE)).map_or(0, |m| m.len())
    }

    pub fn reranker_bytes(&self) -> u64 {
        std::fs::metadata(self.dir.join(RERANKER_DIR).join(rerank::MODEL_FILE)).map_or(0, |m| m.len())
    }

    /// The GPU verdict now. No probe (tests, no Sentinel) is unknown, which keeps CPU.
    fn verdict(&self) -> GpuVerdict {
        self.gpu.as_ref().map_or_else(|| GpuVerdict::Unknown("no GPU probe".into()), GpuProbe::verdict)
    }

    /// The cross-encoder, loading it if needed (and moving it off a GPU that is no longer idle).
    pub fn reranker(&mut self) -> Result<&mut Reranker, String> {
        if let Some(r) = &self.degraded {
            return Err(r.clone());
        }
        let stale_gpu = self
            .reranker
            .as_ref()
            .is_some_and(|(r, _)| r.choice.provider.is_gpu() && r.checked.elapsed() >= GPU_RECHECK);
        if stale_gpu {
            match self.verdict() {
                GpuVerdict::Idle => {
                    if let Some((r, _)) = self.reranker.as_mut() {
                        r.checked = Instant::now();
                    }
                }
                other => {
                    eprintln!("marlowe-mind: the reranker leaves the GPU ({other:?}); reloading on CPU");
                    self.reranker = None;
                }
            }
        }
        if self.reranker.is_none() {
            let dir = self.dir.join(RERANKER_DIR);
            let verdict = self.verdict();
            let loaded =
                ep::first_that_builds(Request::Auto, &verdict, |p| CrossEncoder::load_on(&dir, SHIPPED_THREADS, p));
            match loaded {
                Ok((encoder, choice)) => {
                    // The cascade is GPU-only by measurement; missing graphs keep the single one.
                    let cascade = if choice.provider.is_gpu() {
                        Cascade::load(&self.dir, SHIPPED_THREADS, choice.provider)
                            .map_err(|e| eprintln!("marlowe-mind: cascade unavailable, single graph on GPU: {e}"))
                            .ok()
                    } else {
                        None
                    };
                    eprintln!("marlowe-mind: cross-encoder on {}", choice.describe());
                    let r = Reranker { encoder, cascade, choice, checked: Instant::now() };
                    self.reranker = Some((r, Instant::now()));
                }
                Err(e) if rerank_permanent(&e) => return Err(self.fail("the cross-encoder", e.to_string())),
                Err(e) => return Err(format!("the cross-encoder did not load (will retry): {e}")),
            }
        }
        let slot = self.reranker.as_mut().expect("loaded above");
        slot.1 = Instant::now();
        Ok(&mut slot.0)
    }

    /// Drops models unused for `idle`.
    pub fn unload_idle(&mut self, now: Instant) {
        if self.embedder.as_ref().is_some_and(|(_, t)| now.duration_since(*t) >= self.idle) {
            self.embedder = None;
        }
        if self.reranker.as_ref().is_some_and(|(_, t)| now.duration_since(*t) >= self.idle) {
            self.reranker = None;
        }
    }

    /// When the next model falls idle.
    pub fn next_unload(&self) -> Option<Instant> {
        let e = self.embedder.as_ref().map(|(_, t)| *t + self.idle);
        let r = self.reranker.as_ref().map(|(_, t)| *t + self.idle);
        match (e, r) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Names of the resident models.
    pub fn loaded(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.embedder.is_some() {
            out.push(EMBEDDER_DIR.to_string());
        }
        if let Some((r, _)) = &self.reranker {
            out.push(match (r.choice.provider.is_gpu(), r.cascade.is_some()) {
                (false, _) => RERANKER_DIR.to_string(),
                (true, true) => format!("{RERANKER_DIR} ({}, 6-graph cascade)", r.choice.provider.short()),
                (true, false) => format!("{RERANKER_DIR} ({})", r.choice.provider.short()),
            });
        }
        out
    }
}

/// Load failures that will not go away by retrying: the files are absent or not the pinned ones.
fn embed_permanent(e: &EmbedError) -> bool {
    matches!(e, EmbedError::Missing { .. } | EmbedError::DigestMismatch { .. } | EmbedError::Vocab { .. })
}

fn rerank_permanent(e: &RerankError) -> bool {
    matches!(
        e,
        RerankError::Missing { .. }
            | RerankError::DigestMismatch { .. }
            | RerankError::SupersededGraph { .. }
            | RerankError::Vocab { .. }
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    use marlowe_memory::ep::Provider;

    use super::*;

    fn models_dir() -> Option<PathBuf> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("models");
        let ok = dir.join(RERANKER_DIR).join(rerank::MODEL_FILE).is_file();
        if !ok {
            eprintln!("SKIP: {} has no reranker", dir.display());
        }
        ok.then_some(dir)
    }

    /// A probe that answers `busy`/idle on demand and counts how often it is asked.
    fn probe(busy: Arc<AtomicBool>, asked: Arc<AtomicUsize>) -> GpuProbe {
        GpuProbe::new(move || {
            asked.fetch_add(1, Ordering::SeqCst);
            if busy.load(Ordering::SeqCst) {
                GpuVerdict::Busy("python.exe is running compute on RTX".into())
            } else {
                GpuVerdict::Idle
            }
        })
    }

    #[test]
    fn a_busy_or_absent_probe_keeps_the_shipped_cpu_reranker() {
        let Some(dir) = models_dir() else { return };
        let (busy, asked) = (Arc::new(AtomicBool::new(true)), Arc::new(AtomicUsize::new(0)));
        let mut m = Models::new(dir.clone(), Duration::from_secs(60), Some(probe(busy, asked.clone())));
        let r = m.reranker().unwrap();
        assert_eq!(r.choice.provider, Provider::Cpu);
        assert!(r.cascade.is_none(), "the cascade is GPU-only");
        let why = if cfg!(feature = "cuda") { "GPU busy" } else { "without the `cuda` feature" };
        assert!(r.choice.reason.contains(why), "{}", r.choice.reason);
        assert_eq!(asked.load(Ordering::SeqCst), 1, "asked once, at load");
        assert_eq!(m.loaded(), vec![RERANKER_DIR.to_string()]);

        let mut none = Models::new(dir, Duration::from_secs(60), None);
        assert_eq!(none.reranker().unwrap().choice.provider, Provider::Cpu);
    }

    #[test]
    fn an_idle_gpu_is_not_taken_by_a_build_without_cuda() {
        if cfg!(feature = "cuda") {
            return; // the `cuda` build's idle path uses the GPU: see the ignored test below
        }
        let Some(dir) = models_dir() else { return };
        let (busy, asked) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicUsize::new(0)));
        let mut m = Models::new(dir, Duration::from_secs(60), Some(probe(busy, asked)));
        let r = m.reranker().unwrap();
        assert_eq!(r.choice.provider, Provider::Cpu);
        assert!(r.choice.reason.contains("without the `cuda` feature"), "{}", r.choice.reason);
    }

    /// `--features cuda`, GPU idle, `gpu-runtime\` fetched, all six graphs present:
    ///   cargo test -p marlowe-mind --features cuda models::tests -- --ignored --nocapture
    #[test]
    #[ignore]
    fn on_an_idle_gpu_the_cascade_loads_and_leaves_when_the_gpu_gets_busy() {
        if !cfg!(feature = "cuda") {
            return;
        }
        let Some(dir) = models_dir() else { return };
        // ORT loads its provider DLLs from beside the executable; cargo leaves them in the
        // profile dir, one level above the test binary.
        let exe = std::env::current_exe().unwrap();
        let (here, profile) = (exe.parent().unwrap(), exe.parent().unwrap().parent().unwrap());
        for dll in ["onnxruntime_providers_shared.dll", "onnxruntime_providers_cuda.dll"] {
            if !here.join(dll).exists() && profile.join(dll).exists() {
                std::fs::copy(profile.join(dll), here.join(dll)).unwrap();
            }
        }
        let (busy, asked) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicUsize::new(0)));
        let mut m = Models::new(dir, Duration::from_secs(60), Some(probe(busy.clone(), asked.clone())));
        let r = m.reranker().unwrap();
        assert_eq!(r.choice.provider, Provider::Cuda, "{}", r.choice.describe());
        assert!(r.cascade.is_some(), "an idle GPU brings the six-graph cascade");
        assert_eq!(m.loaded(), vec![format!("{RERANKER_DIR} (cuda, 6-graph cascade)")]);

        // Still idle at the re-check: stays on the GPU.
        m.reranker.as_mut().unwrap().0.checked -= GPU_RECHECK;
        assert_eq!(m.reranker().unwrap().choice.provider, Provider::Cuda);
        // A training job starts: the next call after the re-check interval reloads on CPU.
        busy.store(true, Ordering::SeqCst);
        m.reranker.as_mut().unwrap().0.checked -= GPU_RECHECK;
        let r = m.reranker().unwrap();
        assert_eq!(r.choice.provider, Provider::Cpu);
        assert!(r.cascade.is_none());
        // Within the interval nothing asks again.
        let before = asked.load(Ordering::SeqCst);
        m.reranker().unwrap();
        assert_eq!(asked.load(Ordering::SeqCst), before);
    }
}
