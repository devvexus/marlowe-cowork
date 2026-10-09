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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(r.unavailable.is_some() || !r.devices.is_empty());
    }
}
