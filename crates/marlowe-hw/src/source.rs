//! Where the planner's inputs come from. The library calls these traits; the binary implements
//! them over Ollama's store and logs (`marlowe-provider`) and NVML/PDH (`marlowe-sentinel`), so
//! the planner itself never touches the machine and tests use fakes.

use crate::ggml::KvType;
use crate::header::ModelHeader;
use crate::log::LoadBlock;
use crate::plan::GpuBudget;

/// A model's files, read without loading it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelFiles {
    pub main: ModelHeader,
    /// The DRAFT model Ollama runs alongside for speculative decoding.
    pub draft: Option<ModelHeader>,
    /// A separate vision projector (`--mmproj`), bytes.
    pub mmproj_bytes: u64,
    /// The main blob's path, for matching the runner log.
    pub blob: Option<String>,
    /// `GGUF header` or `/api/show`.
    pub source: String,
    /// The quantization Ollama reports (`IQ3_XXS`), when the header's file type is unnamed.
    pub quant: Option<String>,
}

/// Ollama's server-wide settings: what the running server uses and what is saved for its next
/// start. Strings as Ollama takes them (`q8_0`, `1`, `1`/`true`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerEnv {
    pub kv_cache_type: Option<String>,
    pub num_parallel: Option<String>,
    pub flash_attention: Option<String>,
}

/// One runner slot (`GET /slots` on the runner's port).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlotObservation {
    pub id: u32,
    pub n_ctx: u32,
    pub n_prompt_tokens: u32,
    pub is_processing: bool,
}

/// What the engine is doing with a model now.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunnerObservation {
    /// The last load of this model's blob in the runner log.
    pub load: Option<LoadBlock>,
    /// `/api/ps` lists the model.
    pub loaded: bool,
    /// `/api/ps` `context_length`.
    pub ps_context: Option<u32>,
    pub slots: Vec<SlotObservation>,
    /// The saved user environment (what the next server start reads).
    pub saved: ServerEnv,
}

/// Reads model files and the engine's state.
pub trait ModelSource: Send + Sync {
    fn files(&self, model_id: &str) -> Result<ModelFiles, String>;
    fn runtime(&self, model_id: &str, blob: Option<&str>) -> RunnerObservation;
    /// Every model the engine has loaded now: a server restart unloads all of them.
    fn loaded_models(&self) -> Vec<String> {
        Vec::new()
    }
    /// [`ModelSource::loaded_models`], or `None` when the engine did not answer: what is loaded
    /// is then unknown, which is not the same as nothing.
    fn resident(&self) -> Option<Vec<String>> {
        Some(self.loaded_models())
    }
    /// Unloads `model_id` from the engine now (Ollama: `keep_alive: 0`), so its memory is free
    /// before this returns. Used when the window closes. The default cannot.
    fn unload(&self, _model_id: &str) -> Result<(), String> {
        Err("this engine cannot unload a model".into())
    }
}

/// The GPU and system memory now.
#[derive(Debug, Clone, PartialEq)]
pub struct GpuSnapshot {
    pub gpu: GpuBudget,
    /// Dedicated memory per runner pid, MiB.
    pub runners: Vec<(u32, f64)>,
    pub ram_total_mib: Option<f64>,
    pub ram_free_mib: Option<f64>,
}

pub trait GpuSource: Send + Sync {
    /// `None` when there is no NVIDIA GPU (or NVML is absent).
    fn snapshot(&self) -> Option<GpuSnapshot>;
}

/// A change to Ollama's server-wide settings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServerChange {
    pub kv_type: Option<KvType>,
    pub parallel: Option<u16>,
    pub flash_attn: Option<bool>,
}

impl ServerChange {
    pub fn is_empty(&self) -> bool {
        self.kv_type.is_none() && self.parallel.is_none() && self.flash_attn.is_none()
    }

    /// The user environment variables this sets, as (name, value).
    pub fn variables(&self) -> Vec<(&'static str, String)> {
        let mut v = Vec::new();
        if let Some(k) = self.kv_type {
            v.push(("OLLAMA_KV_CACHE_TYPE", k.ollama_name().to_string()));
            // Quantized V needs flash attention; Ollama ignores the KV type without it.
            if k != KvType::F16 && self.flash_attn.is_none() {
                v.push(("OLLAMA_FLASH_ATTENTION", "1".into()));
            }
        }
        if let Some(n) = self.parallel {
            v.push(("OLLAMA_NUM_PARALLEL", n.to_string()));
        }
        if let Some(f) = self.flash_attn {
            v.push(("OLLAMA_FLASH_ATTENTION", if f { "1" } else { "0" }.into()));
        }
        v
    }

    /// What a person would do by hand, step by step.
    pub fn manual_steps(&self) -> Vec<String> {
        let mut v: Vec<String> =
            self.variables().into_iter().map(|(k, val)| format!("setx {k} {val}")).collect();
        v.push("quit Ollama from its tray icon, then start Ollama again".into());
        v
    }
}

/// Applies a server change: sets the user environment variables and restarts the Ollama server.
/// Only ever called after the user approved it; tests use a fake.
pub trait ServerControl: Send + Sync {
    fn apply(&self, change: &ServerChange) -> Result<String, String>;

    /// Makes sure the engine is answering, starting it if it is not (never restarting a running
    /// one: that is `apply`, behind an approval). `progress` hears one line when a start begins.
    fn ensure_running(&self, _progress: &mut dyn FnMut(&str)) -> EngineStart {
        EngineStart::NotApplicable
    }
}

/// What [`ServerControl::ensure_running`] found or did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineStart {
    /// It was already answering; nothing was started.
    Running,
    /// It was not answering and now is; the line says how long it took.
    Started(String),
    /// Not a local engine this build manages (a remote server, a test).
    NotApplicable,
    /// It could not be started: why, plainly.
    Failed(String),
}
