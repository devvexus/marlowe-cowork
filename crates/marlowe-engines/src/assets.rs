//! Which release assets to download: the archive for this architecture, plus the CUDA runtime zip
//! for the stock backend. Windows only for now.

use crate::backend::Backend;

/// CPU architecture of a Windows install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X64,
    Arm64,
}

impl Arch {
    /// This process's architecture, `None` where there is no prebuilt backend (not Windows).
    pub fn current() -> Option<Arch> {
        if !cfg!(windows) {
            return None;
        }
        if cfg!(target_arch = "aarch64") {
            Some(Arch::Arm64)
        } else if cfg!(target_arch = "x86_64") {
            Some(Arch::X64)
        } else {
            None
        }
    }

    fn name(self) -> &'static str {
        match self {
            Arch::X64 => "x64",
            Arch::Arm64 => "arm64",
        }
    }
}

/// The CUDA build of the stock backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cuda {
    V12_4,
    V13_5,
}

impl Cuda {
    pub fn version(self) -> &'static str {
        match self {
            Cuda::V12_4 => "12.4",
            Cuda::V13_5 => "13.5",
        }
    }
}

/// CUDA 13.5 where the driver supports it, else 12.4. ARM64 only has 13.5. `driver` is the CUDA
/// version the NVIDIA driver supports (`(13, 4)`), `None` when it could not be read.
pub fn cuda_for(arch: Arch, driver: Option<(u32, u32)>) -> Cuda {
    match arch {
        Arch::Arm64 => Cuda::V13_5,
        Arch::X64 if driver.is_some_and(|d| d >= (13, 5)) => Cuda::V13_5,
        Arch::X64 => Cuda::V12_4,
    }
}

/// The files of one install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetPlan {
    /// The zip with `llama-server.exe`.
    pub archive: String,
    /// The CUDA runtime zip that goes into the same folder, when the archive lacks it.
    pub runtime: Option<String>,
}

impl AssetPlan {
    pub fn names(&self) -> Vec<&str> {
        std::iter::once(self.archive.as_str()).chain(self.runtime.as_deref()).collect()
    }
}

/// The assets for `backend`. `build_tag` is the tag the binaries live under (`b12359`; for the
/// fork it is ignored: the asset names carry no tag).
pub fn plan(backend: Backend, build_tag: &str, arch: Arch, cuda: Cuda) -> AssetPlan {
    match backend {
        Backend::LlamaCpp => AssetPlan {
            archive: format!("llama-{build_tag}-bin-win-cuda-{}-{}.zip", cuda.version(), arch.name()),
            runtime: Some(format!("cudart-llama-bin-win-cuda-{}-{}.zip", cuda.version(), arch.name())),
        },
        Backend::MsLlamaCpp => AssetPlan {
            archive: match arch {
                Arch::X64 => "ms-llama-win-x64-cuda12.zip".to_string(),
                Arch::Arm64 => "ms-llama-win-arm64-cuda.zip".to_string(),
            },
            runtime: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_driver_decides_between_cuda_13_5_and_12_4_on_x64() {
        assert_eq!(cuda_for(Arch::X64, Some((13, 4))), Cuda::V12_4);
        assert_eq!(cuda_for(Arch::X64, Some((13, 5))), Cuda::V13_5);
        assert_eq!(cuda_for(Arch::X64, Some((14, 0))), Cuda::V13_5);
        assert_eq!(cuda_for(Arch::X64, Some((12, 9))), Cuda::V12_4);
        assert_eq!(cuda_for(Arch::X64, None), Cuda::V12_4, "an unreadable driver falls back to 12.4");
        assert_eq!(cuda_for(Arch::Arm64, None), Cuda::V13_5);
    }

    #[test]
    fn asset_names_follow_each_repos_scheme() {
        let up = plan(Backend::LlamaCpp, "b12359", Arch::X64, Cuda::V12_4);
        assert_eq!(up.archive, "llama-b12359-bin-win-cuda-12.4-x64.zip");
        assert_eq!(up.runtime.as_deref(), Some("cudart-llama-bin-win-cuda-12.4-x64.zip"));
        let arm = plan(Backend::LlamaCpp, "b12359", Arch::Arm64, Cuda::V13_5);
        assert_eq!(arm.archive, "llama-b12359-bin-win-cuda-13.5-arm64.zip");
        let ms = plan(Backend::MsLlamaCpp, "ms-v1.0", Arch::X64, Cuda::V12_4);
        assert_eq!((ms.archive.as_str(), ms.runtime), ("ms-llama-win-x64-cuda12.zip", None));
        assert_eq!(plan(Backend::MsLlamaCpp, "ms-v1.0", Arch::Arm64, Cuda::V13_5).archive, "ms-llama-win-arm64-cuda.zip");
    }
}
