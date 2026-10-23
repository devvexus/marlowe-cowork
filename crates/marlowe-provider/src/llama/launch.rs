//! The `llama-server` command line for a model served by Marlowe.
//!
//! Mirrors the launch Ollama 0.32.5 makes for a model it hands to llama-server whole (measured
//! off a live Ollama process, 2026-10), with the slot count and the per-slot context Marlowe's
//! planner chose. `-c` is the product of the two and the KV cache is split per slot (no
//! `--kv-unified`), so one agent can never take another's context. `-ngl 99 --fit off` makes a
//! misfit fail at load, loudly, instead of quietly spilling layers to the CPU.

use std::path::{Path, PathBuf};

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
    /// `-b` / `-ub`: Ollama's own default is 512 for the cards it sees as small.
    pub ubatch: u32,
    /// Where the server may write and read slot caches (`--slot-save-path`).
    pub slot_save: Option<PathBuf>,
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
        s("--no-mmap"),
        s("--cache-type-k"),
        spec.kv.clone(),
        s("--cache-type-v"),
        spec.kv.clone(),
        s("--flash-attn"),
        s("on"),
        s("-b"),
        spec.ubatch.to_string(),
        s("-ub"),
        spec.ubatch.to_string(),
        // No `--context-shift`: Ollama's launch has it, but a server that quietly slides a
        // conversation's window rewrites its prefix. The loop compacts; an overflow is a clean error.
        s("--fit"),
        s("off"),
        s("-ngl"),
        s("99"),
    ];
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

/// The CUDA backends Ollama ships beside the binary, newest first: Ollama itself picks the newest
/// its driver runs, and the engine falls back to the next when the log says the first did not load.
pub fn backend_candidates(binary: &Path) -> Vec<PathBuf> {
    let Some(dir) = binary.parent() else { return Vec::new() };
    let mut out = Vec::new();
    for sub in ["cuda_v13", "cuda_v12"] {
        for name in ["ggml-cuda.dll", "libggml-cuda.so"] {
            let p = dir.join(sub).join(name);
            if p.is_file() {
                out.push(p);
            }
        }
    }
    out
}

/// The bundled server binary (`MARLOWE_LLAMA_SERVER` overrides it).
pub fn server_binary() -> Result<PathBuf, ResolveError> {
    crate::ollama_store::server_binary()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Spec {
        Spec { blob: PathBuf::from("C:/m/blob"), port: 18080, slots: 6, ctx_per_slot: 49_152, kv: "q8_0".into(), ubatch: 512, slot_save: None }
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
    fn the_launch_mirrors_ollamas_and_fails_loudly_on_a_misfit() {
        let a = args(&spec());
        assert_eq!(after(&a, "--cache-type-k"), "q8_0");
        assert_eq!(after(&a, "--cache-type-v"), "q8_0");
        assert_eq!(after(&a, "--flash-attn"), "on");
        assert_eq!((after(&a, "-ngl"), after(&a, "--fit")), ("99", "off"));
        assert!(!a.iter().any(|x| x == "--context-shift") && a.iter().any(|x| x == "--no-mmap") && a.iter().any(|x| x == "--jinja"));
        assert_eq!(after(&a, "--host"), "127.0.0.1");
        // None of the flags Ollama adds only for the models it renders itself.
        assert!(!a.iter().any(|x| x == "--no-jinja" || x == "--chat-template" || x.starts_with("--spec")));
    }

    #[test]
    fn the_slot_cache_directory_is_passed_when_there_is_one() {
        let mut s = spec();
        assert!(!args(&s).iter().any(|x| x == "--slot-save-path"));
        s.slot_save = Some(PathBuf::from("C:/m/slots"));
        assert_eq!(after(&args(&s), "--slot-save-path"), "C:/m/slots");
    }

    #[test]
    fn the_backend_dll_and_its_directory_are_part_of_the_plan() {
        let p = plan(Path::new("C:/o/lib/ollama/llama-server.exe"), Some(Path::new("C:/o/lib/ollama/cuda_v13/ggml-cuda.dll")), &spec());
        assert_eq!(p.path_prefix, vec![PathBuf::from("C:/o/lib/ollama"), PathBuf::from("C:/o/lib/ollama/cuda_v13")]);
        assert!(p.backend_dll.is_some());
    }
}
