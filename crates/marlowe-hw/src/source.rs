//! Where the planner's inputs come from. The library calls these traits; the binary implements
//! them over the model store and the engine's own log (`marlowe-provider`) and NVML/PDH
//! (`marlowe-sentinel`), so the planner itself never touches the machine and tests use fakes.

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
    /// Architectures the engine said it serves one request at a time (its log's warnings).
    pub serial_archs: Vec<String>,
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
    /// The backend or its installed version changed: Marlowe's own server is replaced once its
    /// requests are done (never cut off), the next request starting the new one. The default
    /// has no server of its own.
    fn restart_engine(&self) {}
    /// The folders (and single files) the user added under `[models] dirs`: model files there make
    /// the local provider available whatever Ollama and Hugging Face hold. The default has none.
    fn model_folders(&self) -> Vec<std::path::PathBuf> {
        Vec::new()
    }
    /// The MS models this machine has with the generation of each (`MS Gen 1` is 1), by name:
    /// what a new ms-llama.cpp could stop running. The default knows none.
    fn ms_models(&self) -> Vec<(String, u8)> {
        Vec::new()
    }
    /// Adds a folder (or one `.gguf` file) the model files are searched in, effective at once.
    /// `Ok(false)` when it was there already. The default cannot.
    fn add_model_dir(&self, _path: &std::path::Path) -> Result<bool, String> {
        Err("this engine has no model folders".into())
    }
    /// Whether `model_id` runs on Marlowe's own llama-server (which saves each conversation's cache
    /// when it stops) rather than on the engine's runner (which loses it when unloaded).
    fn runs_on_own_server(&self, _model_id: &str) -> bool {
        false
    }
    /// Whether Marlowe's own llama-server may serve `model_id` (Ollama itself hands the model to
    /// llama-server whole): `Ok`, or why Ollama must keep it. The default has no such engine.
    fn engine_eligibility(&self, _model_id: &str) -> Result<(), String> {
        Err("this build has no llama.cpp engine".into())
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

/// A change to the local engine's settings: the KV type and slots of the model's policy, and
/// flash attention, which is server-wide. Applying it restarts the engine.
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

    /// The settings this sets, as (name, value): `kv_type`, `parallel`, `flash_attn`. A quantized
    /// V cache needs flash attention, so a quantized KV type turns it on unless it is set.
    pub fn settings(&self) -> Vec<(&'static str, String)> {
        let mut v = Vec::new();
        if let Some(k) = self.kv_type {
            v.push(("kv_type", k.ollama_name().to_string()));
            if k != KvType::F16 && self.flash_attn.is_none() {
                v.push(("flash_attn", "on".into()));
            }
        }
        if let Some(n) = self.parallel {
            v.push(("parallel", n.to_string()));
        }
        if let Some(f) = self.flash_attn {
            v.push(("flash_attn", if f { "on" } else { "off" }.into()));
        }
        v
    }

    /// What a person would do by hand, step by step.
    pub fn manual_steps(&self) -> Vec<String> {
        let mut v = Vec::new();
        if self.kv_type.is_some() || self.parallel.is_some() {
            v.push("save the KV type and the agents in the context planner".to_string());
        }
        if let Some(f) = self.flash_attn {
            v.push(format!("set `flash_attn = {f}` under [engine] in settings.toml"));
        }
        v.push("unload the model (the next request starts the engine with the new settings)".into());
        v
    }
}

/// Applies a server change to the local engine and restarts it. Only ever called after the user
/// approved it; tests use a fake.
pub trait ServerControl: Send + Sync {
    fn apply(&self, change: &ServerChange) -> Result<String, String>;

    /// Makes sure the engine is answering, starting it if it is not (never restarting a running
    /// one: that is `apply`, behind an approval). `progress` hears one line when a start begins.
    fn ensure_running(&self, _progress: &mut dyn FnMut(&str)) -> EngineStart {
        EngineStart::NotApplicable
    }

    /// The engine's own output from now on (the log of a server Marlowe started, or the app's
    /// `server.log`), or `None` when none can be seen: a server started by hand writes only to
    /// its console. Read-only.
    fn log_feed(&self) -> Option<Box<dyn LogFeed>> {
        None
    }
}

/// New whole lines of the engine's output since the last call.
pub trait LogFeed: Send {
    fn poll(&mut self) -> Vec<String>;
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
