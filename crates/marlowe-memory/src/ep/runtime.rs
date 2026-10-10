//! Where the GPU runtime lives and how it is loaded (`cuda` feature only).
//!
//! `scripts/fetch-gpu-runtime.ps1` puts sha256-pinned files under `<root>\<arch>\`:
//!
//! * x64: `cuda\` holds the CUDA 13 runtime, cuBLAS, cuFFT and cuDNN 9 DLLs that the pyke ORT
//!   1.28 build's `onnxruntime_providers_cuda.dll` imports. Each is preloaded by full path before
//!   the first CUDA session, so the provider's imports resolve to these files and not to whatever
//!   happens to be on `PATH`.
//! * arm64: `onnxruntime\onnxruntime.dll` (official 1.31.0) is loaded with `ort::init_from`
//!   before any session (this build links no ORT), and `cuda-plugin\onnxruntime_providers_cuda.dll`
//!   (Microsoft's CUDA plugin EP v0.3.0) is registered with the environment; CUDA DLLs come from
//!   `cuda\` or `%CUDA_PATH%`.
//!
//! The root is, in order: [`set_root`] (the eval adapter's `--gpu-runtime`; marlowe-mind's
//! `<home>\gpu-runtime` when it exists and the variable below is unset), `$MARLOWE_GPU_RUNTIME`,
//! then a `gpu-runtime` directory beside an ancestor of the executable (the dev tree). The root
//! in use is reported by [`describe`].

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use ort::session::builder::SessionBuilder;

/// Environment override for the runtime root.
pub const ROOT_ENV: &str = "MARLOWE_GPU_RUNTIME";

/// The per-architecture subdirectory of the root.
pub const ARCH_DIR: &str = if cfg!(target_arch = "aarch64") { "arm64" } else { "x64" };

/// CUDA DLLs, dependencies first: loading by full path resolves a DLL's own imports by module
/// name, so cuBLAS must find cuBLASLt already loaded.
const CUDA_DLLS: &[&str] = &["cudart64_13.dll", "cublasLt64_13.dll", "cublas64_13.dll", "cufft64_12.dll"];
/// cuDNN 9: the core library first, then the sub-libraries it loads by name.
const CUDNN_DLLS: &[&str] = &[
    "cudnn64_9.dll",
    "cudnn_graph64_9.dll",
    "cudnn_ops64_9.dll",
    "cudnn_heuristic64_9.dll",
    "cudnn_adv64_9.dll",
    "cudnn_cnn64_9.dll",
    "cudnn_engines_precompiled64_9.dll",
    "cudnn_engines_runtime_compiled64_9.dll",
];

/// TF32 matmuls, off. On by default in the CUDA EP; measured on the shipped graph it moves a
/// logit by up to 1.08e-3 against the reference fixture (over the 1e-3 tolerance). These graphs
/// are tiny, so full f32 costs little and keeps the GPU scorer next to the CPU one.
const TF32: bool = false;

static ROOT: Mutex<Option<PathBuf>> = Mutex::new(None);
static CUDA_LIBS: OnceLock<Result<Vec<PathBuf>, String>> = OnceLock::new();
#[cfg(target_arch = "aarch64")]
static CORE: OnceLock<Result<PathBuf, String>> = OnceLock::new();
#[cfg(target_arch = "aarch64")]
static PLUGIN: OnceLock<Result<PathBuf, String>> = OnceLock::new();

/// Name the runtime root explicitly. Effective until the first GPU load reads it.
pub fn set_root(root: PathBuf) {
    *ROOT.lock().unwrap_or_else(|e| e.into_inner()) = Some(root);
}

/// The runtime root, if one can be found.
pub fn root() -> Option<PathBuf> {
    if let Some(r) = ROOT.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        return Some(r);
    }
    if let Some(r) = std::env::var_os(ROOT_ENV).filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(r));
    }
    let exe = std::env::current_exe().ok()?;
    exe.ancestors().skip(1).take(5).map(|a| a.join("gpu-runtime")).find(|p| p.is_dir())
}

/// One line: the root in use and what has been loaded from it.
pub fn describe() -> String {
    let root = root().map_or("none found".to_string(), |r| r.display().to_string());
    let libs = match CUDA_LIBS.get() {
        None => "CUDA libraries not loaded yet".to_string(),
        Some(Ok(v)) => format!("{} CUDA/cuDNN libraries preloaded", v.len()),
        Some(Err(e)) => format!("CUDA libraries failed: {e}"),
    };
    format!("gpu runtime root: {root} ({ARCH_DIR}); {libs}")
}

/// Directories searched for CUDA and cuDNN DLLs, in order.
fn cuda_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(r) = root() {
        dirs.push(r.join(ARCH_DIR).join("cuda"));
    }
    for var in ["CUDA_PATH", "CUDNN_PATH"] {
        if let Some(p) = std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from) {
            dirs.push(p.join("bin").join(ARCH_DIR));
            dirs.push(p.join("bin"));
        }
    }
    dirs
}

fn find(dirs: &[PathBuf], dll: &str) -> Option<PathBuf> {
    dirs.iter().map(|d| d.join(dll)).find(|p| p.is_file())
}

/// Preload the CUDA libraries once. On x64 every listed DLL is required (the provider imports
/// them); cuDNN sub-libraries are loaded when present.
pub fn preload_cuda() -> Result<&'static [PathBuf], String> {
    let r = CUDA_LIBS.get_or_init(|| {
        let dirs = cuda_dirs();
        if dirs.is_empty() {
            return Err(format!(
                "no GPU runtime found: set {ROOT_ENV} or run scripts/fetch-gpu-runtime.ps1"
            ));
        }
        let mut loaded = Vec::new();
        let required_cudnn = if cfg!(target_arch = "aarch64") { 0 } else { 1 };
        for (i, dll) in CUDA_DLLS.iter().chain(CUDNN_DLLS).enumerate() {
            let required = i < CUDA_DLLS.len() + required_cudnn;
            match find(&dirs, dll) {
                Some(path) => {
                    ort::util::preload_dylib(&path).map_err(|e| format!("loading {}: {e}", path.display()))?;
                    loaded.push(path);
                }
                None if required => {
                    let looked: Vec<String> = dirs.iter().map(|d| d.display().to_string()).collect();
                    return Err(format!(
                        "{dll} not found (looked in {}); run scripts/fetch-gpu-runtime.ps1",
                        looked.join(", ")
                    ));
                }
                None => {}
            }
        }
        Ok(loaded)
    });
    r.as_deref().map_err(Clone::clone)
}

/// Load ONNX Runtime itself where the build links none (ARM64). No-op on x64.
pub fn ensure_core() -> Result<(), String> {
    #[cfg(target_arch = "aarch64")]
    {
        let r = CORE.get_or_init(|| {
            let root = root().ok_or_else(|| {
                format!("no GPU runtime root: this ARM64 build loads ONNX Runtime at run time; set {ROOT_ENV} or run scripts/fetch-gpu-runtime.ps1")
            })?;
            let dll = root.join(ARCH_DIR).join("onnxruntime").join("onnxruntime.dll");
            if !dll.is_file() {
                return Err(format!("{} is missing; run scripts/fetch-gpu-runtime.ps1", dll.display()));
            }
            let builder = ort::init_from(&dll).map_err(|e| e.to_string())?;
            let _ = builder.commit();
            Ok(dll)
        });
        r.as_ref().map(|_| ()).map_err(Clone::clone)
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        Ok(())
    }
}

/// Register CUDA on `builder`: the in-tree CUDA EP on x64, the CUDA plugin EP on ARM64.
pub fn register_cuda(builder: SessionBuilder) -> ort::Result<SessionBuilder> {
    preload_cuda().map_err(ort::Error::new)?;
    #[cfg(not(target_arch = "aarch64"))]
    {
        use ort::ep::CUDA;
        let cuda = CUDA::default().with_tf32(TF32);
        Ok(builder.with_execution_providers([cuda.build().error_on_failure()])?)
    }
    #[cfg(target_arch = "aarch64")]
    {
        use ort::environment::Environment;
        use ort::memory::DeviceType;
        let env = Environment::current()?;
        let plugin = PLUGIN.get_or_init(|| {
            let root = root().ok_or_else(|| format!("no GPU runtime root; set {ROOT_ENV}"))?;
            let dll = root.join(ARCH_DIR).join("cuda-plugin").join("onnxruntime_providers_cuda.dll");
            if !dll.is_file() {
                return Err(format!("{} is missing; run scripts/fetch-gpu-runtime.ps1", dll.display()));
            }
            env.register_ep_library("CUDA", &dll).map_err(|e| e.to_string())?;
            Ok(dll)
        });
        plugin.as_ref().map_err(|e| ort::Error::new(e.clone()))?;
        let devices: Vec<_> = env
            .devices()
            .filter(|d| d.hardware_device().ty() == DeviceType::GPU)
            .filter(|d| d.ep().is_ok_and(|n| n.to_ascii_lowercase().contains("cuda")))
            .collect();
        let Some(name) = devices.first().and_then(|d| d.ep().ok()).map(str::to_string) else {
            return Err(ort::Error::new("the CUDA plugin EP registered but reported no GPU device"));
        };
        let options = [(format!("{name}.use_tf32"), if TF32 { "1" } else { "0" }.to_string())];
        Ok(builder.with_devices(devices, Some(&options))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn the_explicit_root_wins_and_dirs_follow_the_layout() {
        set_root(PathBuf::from(r"C:\nowhere\gpu-runtime"));
        assert_eq!(root(), Some(PathBuf::from(r"C:\nowhere\gpu-runtime")));
        let dirs = cuda_dirs();
        assert_eq!(dirs[0], Path::new(r"C:\nowhere\gpu-runtime").join(ARCH_DIR).join("cuda"));
        assert!(describe().contains(r"C:\nowhere\gpu-runtime"));
    }
}
