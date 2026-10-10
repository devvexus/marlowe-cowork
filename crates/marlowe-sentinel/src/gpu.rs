//! GPU stats via NVML (`nvml.dll` loaded at runtime). Absent -> reported unavailable, never an
//! error. Queries only: no contexts, no work submitted to the device.

use marlowe_proto::{GpuDevice, GpuReport};
use nvml_wrapper::enum_wrappers::device::TemperatureSensor;
use nvml_wrapper::enums::device::UsedGpuMemory;

/// Top process names reported per device.
const TOP_PROCESSES: usize = 3;

/// An NVML session, or the reason there is none.
pub struct Gpu {
    nvml: Result<nvml_wrapper::Nvml, String>,
}

impl Gpu {
    /// Loads NVML. Never fails: a missing library is remembered as the unavailable reason.
    pub fn open() -> Gpu {
        Gpu { nvml: nvml_wrapper::Nvml::init().map_err(|e| e.to_string()) }
    }

    /// A GPU that is always unavailable (tests, `gpu: false`).
    pub fn disabled(reason: &str) -> Gpu {
        Gpu { nvml: Err(reason.to_string()) }
    }

    pub fn is_available(&self) -> bool {
        self.nvml.is_ok()
    }

    /// Per device index, the names of other processes doing compute only: on NVML's compute list
    /// but not its graphics list. Desktop apps (browsers, the shell) show up on both under WDDM;
    /// a training job or an inference server shows up on the compute list alone. This process is
    /// left out. Unreadable devices are skipped; no NVML gives an empty list. Ollama's runners are
    /// labelled by [`compute_label`] so callers can tell them from other compute.
    pub fn compute_only(&self) -> Vec<(u32, Vec<String>)> {
        let Ok(nvml) = &self.nvml else { return Vec::new() };
        let me = std::process::id();
        let mut out = Vec::new();
        for index in 0..nvml.device_count().unwrap_or(0) {
            let Ok(d) = nvml.device_by_index(index) else { continue };
            let graphics: Vec<u32> = d.running_graphics_processes().unwrap_or_default().iter().map(|p| p.pid).collect();
            let mut names: Vec<String> = Vec::new();
            for p in d.running_compute_processes().unwrap_or_default() {
                if p.pid == me || graphics.contains(&p.pid) {
                    continue;
                }
                let name = crate::process::image_path(p.pid)
                    .map(|path| compute_label(&path))
                    .unwrap_or_else(|| format!("pid {}", p.pid));
                if !names.contains(&name) {
                    names.push(name);
                }
            }
            out.push((index, names));
        }
        out
    }

    /// Utilisation, memory, temperature and (when readable) top process names per device.
    /// A device that cannot be read is skipped; no devices at all is reported as unavailable.
    pub fn sample(&self) -> GpuReport {
        let nvml = match &self.nvml {
            Err(reason) => return GpuReport { devices: Vec::new(), unavailable: Some(reason.clone()) },
            Ok(n) => n,
        };
        let count = match nvml.device_count() {
            Ok(c) => c,
            Err(e) => return GpuReport { devices: Vec::new(), unavailable: Some(e.to_string()) },
        };
        let mut devices = Vec::new();
        for index in 0..count {
            let Ok(d) = nvml.device_by_index(index) else { continue };
            let mem = d.memory_info().ok();
            let mut procs: Vec<(u64, u32)> = d
                .running_compute_processes()
                .unwrap_or_default()
                .into_iter()
                .chain(d.running_graphics_processes().unwrap_or_default())
                .map(|p| (if let UsedGpuMemory::Used(b) = p.used_gpu_memory { b } else { 0 }, p.pid))
                .collect();
            procs.sort_by_key(|p| std::cmp::Reverse(p.0));
            let mut processes: Vec<String> = Vec::new();
            for (_, pid) in procs {
                if processes.len() >= TOP_PROCESSES {
                    break;
                }
                if let Some(name) = crate::process::name(pid) {
                    if !processes.contains(&name) {
                        processes.push(name);
                    }
                }
            }
            devices.push(GpuDevice {
                index,
                name: d.name().unwrap_or_else(|_| format!("GPU {index}")),
                util_pct: d.utilization_rates().map(|u| u.gpu).unwrap_or(0),
                mem_used_mb: mem.as_ref().map_or(0, |m| m.used / (1024 * 1024)),
                mem_total_mb: mem.as_ref().map_or(0, |m| m.total / (1024 * 1024)),
                temp_c: d.temperature(TemperatureSensor::Gpu).ok(),
                processes,
            });
        }
        let unavailable = devices.is_empty().then(|| "no readable NVIDIA device".to_string());
        GpuReport { devices, unavailable }
    }
}

/// The name a compute process is reported under: its executable name, prefixed `ollama:` when it
/// is one of Ollama's runners. Current Ollama computes in `lib\ollama\llama-server.exe`, a name
/// that alone cannot be told from a llama.cpp build Matthew runs himself.
pub fn compute_label(path: &str) -> String {
    let name = path.rsplit(['\\', '/']).next().unwrap_or(path).to_string();
    if crate::vram::is_ollama_runner(path) && !name.to_ascii_lowercase().starts_with("ollama") {
        format!("ollama:{name}")
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ollamas_runner_is_labelled_and_other_compute_is_not() {
        let ollama = r"C:\Users\m\AppData\Local\Programs\Ollama\lib\ollama\llama-server.exe";
        assert_eq!(compute_label(ollama), "ollama:llama-server.exe");
        assert_eq!(compute_label(r"C:\Users\m\AppData\Local\Programs\Ollama\ollama.exe"), "ollama.exe");
        assert_eq!(compute_label(r"C:\src\llama.cpp\build\bin\Release\llama-server.exe"), "llama-server.exe");
        assert_eq!(compute_label(r"C:\Python311\python.exe"), "python.exe");
    }

    #[test]
    fn disabled_reports_unavailable() {
        let g = Gpu::disabled("off");
        assert!(!g.is_available());
        assert_eq!(g.sample(), GpuReport { devices: vec![], unavailable: Some("off".into()) });
    }

    /// Reads real NVML counters (no GPU work). Ignored by default: it depends on the machine.
    #[test]
    #[ignore]
    fn samples_real_nvml_when_present() {
        let g = Gpu::open();
        let r = g.sample();
        eprintln!("{r:?}");
        eprintln!("compute-only: {:?}", g.compute_only());
        assert!(r.unavailable.is_some() || !r.devices.is_empty());
    }

    #[test]
    fn disabled_has_no_compute_processes() {
        assert!(Gpu::disabled("off").compute_only().is_empty());
    }
}
