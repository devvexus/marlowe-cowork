//! The engine's server-wide settings: everything the Ollama era set through Ollama's environment,
//! now the engine's own (`[engine]` in `settings.toml`), with the defaults Marlowe used then.
//!
//! | Setting | Default | Ollama era |
//! |---|---|---|
//! | `flash_attn` | on | `OLLAMA_FLASH_ATTENTION` (a quantized KV cache needs it: off means f16) |
//! | `keep_alive` | `OLLAMA_KEEP_ALIVE` as saved, else 5 m (negative: never unload) | `OLLAMA_KEEP_ALIVE` |
//! | `batch` / `ubatch` | 512 / 512 | Ollama's `-b` / `-ub` |
//! | `mmap` | on (`--load-mode mmap --lazy-mode on`): the input layer is read lazily from the file | llama.cpp's own default (Ollama's launch had it off) |
//! | `gpu_fit` | `auto`: whole on the GPU when the planner says it fits, else split | Ollama's layer split |
//!
//! The KV type, the context and the slots are per model: the context planner's policy (saved per
//! model, automatic until then), which is the server's launch.

use std::time::Duration;

/// Where a model's layers go.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GpuFit {
    /// Whole on the GPU (`-ngl 99 --fit off`) when the planner and the free memory say it fits,
    /// else split, as Ollama does.
    #[default]
    Auto,
    /// Whole on the GPU or not at all: a model that does not fit is an error.
    Whole,
    /// Always let llama.cpp place what fits on the GPU (`--fit on`).
    Split,
}

impl GpuFit {
    pub fn parse(s: &str) -> Option<GpuFit> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(GpuFit::Auto),
            "whole" => Some(GpuFit::Whole),
            "split" => Some(GpuFit::Split),
            _ => None,
        }
    }
}

/// See the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSettings {
    pub flash_attn: bool,
    /// Idle time before the server stops; `None` keeps it until something else needs the GPU.
    pub keep_alive: Option<Duration>,
    pub batch: u32,
    pub ubatch: u32,
    /// Memory-map the model file (llama.cpp's default): what stays on the CPU, the token and
    /// n-gram embedding tables above all, is read from the file as it is used instead of into RAM
    /// at load. Off reads every model's host side into RAM, whatever its planner placement says.
    pub mmap: bool,
    pub gpu_fit: GpuFit,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self { flash_attn: true, keep_alive: Some(Duration::from_secs(300)), batch: 512, ubatch: 512, mmap: true, gpu_fit: GpuFit::Auto }
    }
}

/// Ollama's `OLLAMA_KEEP_ALIVE` spelling: seconds, or a number with `ms`, `s`, `m` or `h`; negative
/// keeps the model loaded for good, and unset is five minutes.
pub fn keep_alive_from(value: Option<&str>) -> Option<Duration> {
    let Some(v) = value.map(str::trim).filter(|v| !v.is_empty()) else { return Some(Duration::from_secs(300)) };
    let (num, unit) = match v.char_indices().find(|(_, c)| c.is_ascii_alphabetic()) {
        Some((i, _)) => v.split_at(i),
        None => (v, "s"),
    };
    let n: f64 = num.parse().ok()?;
    if n < 0.0 {
        return None;
    }
    let secs = match unit {
        "ms" => n / 1000.0,
        "s" => n,
        "m" => n * 60.0,
        "h" => n * 3600.0,
        _ => return Some(Duration::from_secs(300)),
    };
    Some(Duration::from_secs_f64(secs))
}

/// A user environment variable as saved (`HKCU\Environment`), not this process's copy, which is
/// as old as the daemon. Read-only: the default keep-alive starts from Ollama's.
pub fn saved_user_env(name: &str) -> Option<String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let out = std::process::Command::new("reg")
            .args(["query", r"HKCU\Environment", "/v", name])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        parse_reg_value(&String::from_utf8_lossy(&out.stdout), name)
    }
    #[cfg(not(windows))]
    {
        std::env::var(name).ok()
    }
}

/// The value from `reg query` output: `    NAME    REG_SZ    value`.
pub fn parse_reg_value(out: &str, name: &str) -> Option<String> {
    out.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        (it.next()?.eq_ignore_ascii_case(name)).then_some(())?;
        let _ty = it.next()?;
        let v: Vec<&str> = it.collect();
        (!v.is_empty()).then(|| v.join(" "))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keep_alive_reads_ollamas_spellings() {
        assert_eq!(keep_alive_from(None), Some(Duration::from_secs(300)));
        assert_eq!(keep_alive_from(Some("10m")), Some(Duration::from_secs(600)));
        assert_eq!(keep_alive_from(Some("90")), Some(Duration::from_secs(90)));
        assert_eq!(keep_alive_from(Some("1h")), Some(Duration::from_secs(3600)));
        assert_eq!(keep_alive_from(Some("-1")), None);
    }

    #[test]
    fn the_defaults_are_what_marlowe_launched_with_under_ollama_but_mmap() {
        let s = ServerSettings::default();
        assert!(s.flash_attn && s.mmap, "memory-mapped, as llama.cpp loads by default");
        assert_eq!((s.batch, s.ubatch, s.gpu_fit, s.keep_alive), (512, 512, GpuFit::Auto, Some(Duration::from_secs(300))));
        assert_eq!(GpuFit::parse(" Split "), Some(GpuFit::Split));
        assert_eq!(GpuFit::parse("sometimes"), None);
    }

    #[test]
    fn a_saved_variable_is_read_from_reg_output() {
        let out = "\r\nHKEY_CURRENT_USER\\Environment\r\n    OLLAMA_KEEP_ALIVE    REG_SZ    30m\r\n\r\n";
        assert_eq!(parse_reg_value(out, "OLLAMA_KEEP_ALIVE").as_deref(), Some("30m"));
        assert_eq!(parse_reg_value(out, "OLLAMA_MODELS"), None);
    }
}
