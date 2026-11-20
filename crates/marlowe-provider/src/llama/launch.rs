//! The `llama-server` command line for a model served by Marlowe.
//!
//! Mirrors the launch Ollama 0.32.5 made for a model it handed to llama-server (measured off a
//! live Ollama process, 2026-10): its draft (`--spec-type`, `--spec-draft-model`,
//! `--spec-draft-n-max 3`) and projector (`--mmproj`) included, with the slot count and the
//! per-slot context Marlowe's planner chose and the engine's settings. The binary is the selected
//! backend's (`marlowe-engines`), never Ollama's. `-c` is the product of the two and the KV cache
//! is split per slot (no `--kv-unified`), so one agent can never take another's context. A model
//! that does not fit whole runs split: `--fit on` and no `-ngl`, so llama.cpp places what fits on
//! the GPU against the memory free, as Ollama splits such a model. `-ngl 99 --fit off` keeps
//! llama.cpp from moving layers to the CPU on its own, but it does NOT make a misfit fail: under
//! the Windows driver's sysmem fallback (NVIDIA "CUDA - Sysmem Fallback Policy", on by default)
//! a `cudaMalloc` past the card's memory succeeds by paging into system RAM, so the server loads
//! and reads at a few tokens a second (a 12.8 GiB model on 11.9 GiB free: 46 tok/s prefill
//! against 2,400). The fit is therefore checked by the engine before it starts a server and the
//! spill after ([`super::engine::VramProbe`]); setting the policy to "Prefer No Sysmem Fallback"
//! for llama-server.exe in the NVIDIA Control Panel makes the allocation fail instead.

use std::path::{Path, PathBuf};

use super::eligible::{Draft, SPEC_DRAFT_N};
use super::settings::ServerSettings;
use crate::ollama_store::{LaunchPlan, ResolveError};

/// What the planner decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub blob: PathBuf,
    pub port: u16,
    pub slots: u16,
    pub ctx_per_slot: u32,
    /// `f16`, `q8_0` or `q4_0`.
    pub kv: String,
    /// Where the server may write and read slot caches (`--slot-save-path`).
    pub slot_save: Option<PathBuf>,
    /// The speculative draft (an MTP head or a small model).
    pub draft: Option<Draft>,
    /// The vision projector.
    pub mmproj: Option<PathBuf>,
    /// llama.cpp places the layers against the free memory (the model does not fit whole).
    pub split: bool,
    /// Flash attention, batch sizes and mmap.
    pub settings: ServerSettings,
}

impl Spec {
    /// The `-c` value: every slot's window together.
    pub fn total_ctx(&self) -> u32 {
        self.ctx_per_slot.saturating_mul(u32::from(self.slots))
    }
}

/// The argument list, in Ollama's order.
pub fn args(spec: &Spec) -> Vec<String> {
    let s = |v: &str| v.to_string();
    let set = &spec.settings;
    // A quantized V cache needs flash attention; without it Ollama ran f16, and so does this.
    let kv = if set.flash_attn { spec.kv.clone() } else { s("f16") };
    let mut a = vec![
        s("--model"),
        spec.blob.display().to_string(),
        s("--port"),
        spec.port.to_string(),
        s("--host"),
        s("127.0.0.1"),
        s("--no-webui"),
        s("--offline"),
        s("-c"),
        spec.total_ctx().to_string(),
        s("-np"),
        spec.slots.to_string(),
        s("--no-log-prefix"),
        s("--no-log-timestamps"),
        // The GGUF's own chat template renders the prompt and llama-server parses the tool
        // calls: what Ollama does for a model with no renderer, parser or template of its own.
        s("--jinja"),
    ];
    if !set.mmap {
        a.push(s("--no-mmap"));
    }
    a.extend([
        s("--cache-type-k"),
        kv.clone(),
        s("--cache-type-v"),
        kv,
        s("--flash-attn"),
        s(if set.flash_attn { "on" } else { "off" }),
        s("-b"),
        set.batch.to_string(),
        s("-ub"),
        set.ubatch.to_string(),
    ]);
    // No `--context-shift`: Ollama's launch has it, but a server that quietly slides a
    // conversation's window rewrites its prefix. The loop compacts; an overflow is a clean error.
    if spec.split {
        a.extend([s("--fit"), s("on")]);
    } else {
        a.extend([s("--fit"), s("off"), s("-ngl"), s("99")]);
    }
    if let Some(p) = &spec.mmproj {
        a.push(s("--mmproj"));
        a.push(p.display().to_string());
    }
    if let Some(d) = &spec.draft {
        a.extend([s("--spec-type"), s(if d.mtp { "draft-mtp" } else { "draft-simple" }), s("--spec-draft-n-max"), SPEC_DRAFT_N.to_string()]);
        a.push(s("--spec-draft-model"));
        a.push(d.path.display().to_string());
    }
    if let Some(dir) = &spec.slot_save {
        // A conversation's cache survives a stop of the server (see `persist`).
        a.push(s("--slot-save-path"));
        a.push(dir.display().to_string());
    }
    a
}

/// The launch as values: binary, argv, the backend DLL and the directories its imports need.
pub fn plan(binary: &Path, backend_dll: Option<&Path>, spec: &Spec) -> LaunchPlan {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for d in [binary.parent(), backend_dll.and_then(Path::parent)].into_iter().flatten() {
        let d = d.to_path_buf();
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    LaunchPlan { binary: binary.to_path_buf(), args: args(spec), backend_dll: backend_dll.map(Path::to_path_buf), path_prefix: dirs }
}

/// The CUDA backends beside the binary, best first. A backend install has `ggml-cuda.dll` in the
/// binary's own folder; the `cuda_v13`/`cuda_v12` subfolders are the layout of a developer's own
/// build. The engine falls back to the next when the log says the first did not load.
pub fn backend_candidates(binary: &Path) -> Vec<PathBuf> {
    let Some(dir) = binary.parent() else { return Vec::new() };
    let mut out = Vec::new();
    for sub in ["", "cuda_v13", "cuda_v12"] {
        for name in ["ggml-cuda.dll", "libggml-cuda.so"] {
            let p = dir.join(sub).join(name);
            if p.is_file() {
                out.push(p);
            }
        }
    }
    out
}

/// The selected backend's server binary (`MARLOWE_LLAMA_SERVER` overrides it).
pub fn server_binary() -> Result<PathBuf, ResolveError> {
    crate::ollama_store::server_binary()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Spec {
        Spec {
            blob: PathBuf::from("C:/m/blob"),
            port: 18080,
            slots: 6,
            ctx_per_slot: 49_152,
            kv: "q8_0".into(),
            slot_save: None,
            draft: None,
            mmproj: None,
            split: false,
            settings: ServerSettings::default(),
        }
    }

    fn after<'a>(a: &'a [String], flag: &str) -> &'a str {
        let i = a.iter().position(|x| x == flag).unwrap_or_else(|| panic!("no {flag}"));
        &a[i + 1]
    }

    #[test]
    fn the_window_is_every_slots_context_together_and_the_cache_is_per_slot() {
        let a = args(&spec());
        assert_eq!(after(&a, "-c"), "294912");
        assert_eq!(after(&a, "-np"), "6");
        assert!(!a.iter().any(|x| x == "--kv-unified"), "unified would let one agent take another's context");
    }

    #[test]
    fn the_launch_mirrors_ollamas_and_keeps_every_layer_on_the_gpu() {
        let a = args(&spec());
        assert_eq!(after(&a, "--cache-type-k"), "q8_0");
        assert_eq!(after(&a, "--cache-type-v"), "q8_0");
        assert_eq!(after(&a, "--flash-attn"), "on");
        assert_eq!((after(&a, "-ngl"), after(&a, "--fit")), ("99", "off"));
        assert!(!a.iter().any(|x| x == "--context-shift") && a.iter().any(|x| x == "--no-mmap") && a.iter().any(|x| x == "--jinja"));
        assert_eq!(after(&a, "--host"), "127.0.0.1");
        // None of the flags Ollama adds only for the models it renders itself, and no draft or
        // projector the model does not have.
        assert!(!a.iter().any(|x| x == "--no-jinja" || x == "--chat-template" || x.starts_with("--spec") || x == "--mmproj"));
    }

    #[test]
    fn an_mtp_model_launches_with_ollamas_speculative_flags_and_a_projector_with_mmproj() {
        let mut s = spec();
        s.draft = Some(Draft { path: PathBuf::from("C:/m/mtp-head.gguf"), mtp: true });
        s.mmproj = Some(PathBuf::from("C:/m/proj"));
        let a = args(&s);
        assert_eq!((after(&a, "--spec-type"), after(&a, "--spec-draft-n-max")), ("draft-mtp", "3"));
        assert_eq!(after(&a, "--spec-draft-model"), "C:/m/mtp-head.gguf");
        assert_eq!(after(&a, "--mmproj"), "C:/m/proj");
        s.draft = Some(Draft { path: PathBuf::from("C:/m/small.gguf"), mtp: false });
        assert_eq!(after(&args(&s), "--spec-type"), "draft-simple");
    }

    #[test]
    fn a_split_model_lets_llama_cpp_place_its_layers_and_the_settings_reach_the_argv() {
        let mut s = spec();
        s.split = true;
        let a = args(&s);
        assert_eq!(after(&a, "--fit"), "on");
        assert!(!a.iter().any(|x| x == "-ngl"), "llama.cpp decides the layers");
        s.settings = ServerSettings { flash_attn: false, batch: 2048, ubatch: 256, mmap: true, ..ServerSettings::default() };
        let a = args(&s);
        assert_eq!((after(&a, "--flash-attn"), after(&a, "--cache-type-k"), after(&a, "--cache-type-v")), ("off", "f16", "f16"));
        assert_eq!((after(&a, "-b"), after(&a, "-ub")), ("2048", "256"));
        assert!(!a.iter().any(|x| x == "--no-mmap"));
    }

    #[test]
    fn the_slot_cache_directory_is_passed_when_there_is_one() {
        let mut s = spec();
        assert!(!args(&s).iter().any(|x| x == "--slot-save-path"));
        s.slot_save = Some(PathBuf::from("C:/m/slots"));
        assert_eq!(after(&args(&s), "--slot-save-path"), "C:/m/slots");
    }

    #[test]
    fn a_backend_installs_cuda_dll_beside_the_binary_comes_first() {
        let dir = std::env::temp_dir().join(format!("marlowe-launch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("cuda_v12")).unwrap();
        std::fs::write(dir.join("ggml-cuda.dll"), b"x").unwrap();
        std::fs::write(dir.join("cuda_v12").join("ggml-cuda.dll"), b"x").unwrap();
        let got = backend_candidates(&dir.join("llama-server.exe"));
        assert_eq!(got, vec![dir.join("ggml-cuda.dll"), dir.join("cuda_v12").join("ggml-cuda.dll")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_backend_dll_and_its_directory_are_part_of_the_plan() {
        let p = plan(Path::new("C:/e/llama.cpp/b1/llama-server.exe"), Some(Path::new("C:/e/llama.cpp/b1/cuda_v13/ggml-cuda.dll")), &spec());
        assert_eq!(p.path_prefix, vec![PathBuf::from("C:/e/llama.cpp/b1"), PathBuf::from("C:/e/llama.cpp/b1/cuda_v13")]);
        assert!(p.backend_dll.is_some());
    }
}
