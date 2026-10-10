//! The GPU budget for the context planner: NVML's device totals, the model runners' dedicated
//! memory per process, peak memory bandwidth, and system RAM.
//!
//! Under WDDM NVML reports per-process memory as "not available", so per-pid dedicated and
//! shared memory come from the `GPU Process Memory` performance counters (PDH), the same numbers
//! Task Manager shows. Queries only: no contexts, no work submitted to the device.

use marlowe_hw::plan::HEADROOM_MIB;
use marlowe_hw::{GpuBudget, GpuSnapshot, GpuSource};
use nvml_wrapper::enum_wrappers::device::Clock;

const MIB: f64 = 1024.0 * 1024.0;

/// Executable names of model runners whose memory a new load may take over.
const RUNNERS: &[&str] = &["llama-server", "ollama"];

/// Whether a process name is a model runner.
pub fn is_runner(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    RUNNERS.iter().any(|r| n.starts_with(r)) && !n.contains("app")
}

/// Whether the process at `path` is one of Ollama's runners, whose memory Ollama hands over when
/// it loads another model. A `llama-server` Matthew runs himself (any build outside Ollama's
/// install) is someone else's memory: counting it as free would overstate the budget.
pub fn is_ollama_runner(path: &str) -> bool {
    let name = path.rsplit(['\\', '/']).next().unwrap_or(path);
    if !is_runner(name) {
        return false;
    }
    let lower = path.to_ascii_lowercase();
    name.to_ascii_lowercase().starts_with("ollama") || lower.split(['\\', '/']).any(|c| c == "ollama")
}

/// `pid_8236_luid_0x00000000_0x0000D3E1_phys_0` → 8236.
pub fn pid_of_instance(instance: &str) -> Option<u32> {
    instance.strip_prefix("pid_")?.split('_').next()?.parse().ok()
}

/// Sums counter instances per pid.
pub fn per_pid(items: &[(String, f64)]) -> Vec<(u32, f64)> {
    let mut out: Vec<(u32, f64)> = Vec::new();
    for (inst, v) in items {
        let Some(pid) = pid_of_instance(inst) else { continue };
        match out.iter_mut().find(|(p, _)| *p == pid) {
            Some((_, s)) => *s += v,
            None => out.push((pid, *v)),
        }
    }
    out
}

/// Every instance of one PDH counter path (wildcards allowed), as (instance, value).
#[cfg(windows)]
#[allow(unsafe_code)]
pub fn pdh_counter(path: &str) -> Option<Vec<(String, f64)>> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Performance::{
        PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
        PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA,
    };
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let mut query = PDH_HQUERY::default();
    let mut counter = PDH_HCOUNTER::default();
    // SAFETY: plain PDH calls on handles this function owns; the query is closed on every path,
    // the item buffer is sized by PDH's own first call and outlives every read of it.
    unsafe {
        if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != 0 {
            return None;
        }
        let result = (|| {
            if PdhAddEnglishCounterW(query, PCWSTR(wide.as_ptr()), 0, &mut counter) != 0 {
                return None;
            }
            if PdhCollectQueryData(query) != 0 {
                return None;
            }
            let (mut bytes, mut count) = (0u32, 0u32);
            let first = PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut bytes, &mut count, None);
            if first != PDH_MORE_DATA || bytes == 0 {
                return Some(Vec::new());
            }
            let n = (bytes as usize).div_ceil(std::mem::size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>()) + 1;
            let mut buf: Vec<PDH_FMT_COUNTERVALUE_ITEM_W> = vec![PDH_FMT_COUNTERVALUE_ITEM_W::default(); n];
            if PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut bytes, &mut count, Some(buf.as_mut_ptr())) != 0 {
                return None;
            }
            let mut out = Vec::with_capacity(count as usize);
            for item in buf.iter().take(count as usize) {
                if item.FmtValue.CStatus != 0 {
                    continue;
                }
                let name = item.szName.to_string().unwrap_or_default();
                out.push((name, item.FmtValue.Anonymous.doubleValue));
            }
            Some(out)
        })();
        let _ = PdhCloseQuery(query);
        result
    }
}

#[cfg(not(windows))]
pub fn pdh_counter(_path: &str) -> Option<Vec<(String, f64)>> {
    None
}

/// System RAM (total, available), MiB.
#[cfg(windows)]
#[allow(unsafe_code)]
pub fn system_ram_mib() -> Option<(f64, f64)> {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut m = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    // SAFETY: `m` is a properly sized, initialised out-parameter.
    unsafe { GlobalMemoryStatusEx(&mut m).ok()? };
    Some((m.ullTotalPhys as f64 / MIB, m.ullAvailPhys as f64 / MIB))
}

#[cfg(not(windows))]
pub fn system_ram_mib() -> Option<(f64, f64)> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb = |key: &str| text.lines().find(|l| l.starts_with(key))?.split_whitespace().nth(1)?.parse::<f64>().ok();
    Some((kb("MemTotal:")? / 1024.0, kb("MemAvailable:")? / 1024.0))
}

/// The planner's GPU source over NVML and PDH. Device 0 is the one planned for.
pub struct SentinelGpu {
    nvml: Result<nvml_wrapper::Nvml, String>,
}

impl SentinelGpu {
    pub fn open() -> Self {
        Self { nvml: nvml_wrapper::Nvml::init().map_err(|e| e.to_string()) }
    }

    /// Dedicated and shared memory of every runner process on the GPU, MiB.
    pub fn runner_memory(&self) -> Vec<(u32, f64, f64)> {
        let Ok(nvml) = &self.nvml else { return Vec::new() };
        let Ok(d) = nvml.device_by_index(0) else { return Vec::new() };
        let mut pids: Vec<u32> = d
            .running_compute_processes()
            .unwrap_or_default()
            .into_iter()
            .chain(d.running_graphics_processes().unwrap_or_default())
            .map(|p| p.pid)
            .filter(|pid| crate::process::image_path(*pid).is_some_and(|p| is_ollama_runner(&p)))
            .collect();
        pids.sort_unstable();
        pids.dedup();
        if pids.is_empty() {
            return Vec::new();
        }
        let dedicated = per_pid(&pdh_counter(r"\GPU Process Memory(*)\Dedicated Usage").unwrap_or_default());
        let shared = per_pid(&pdh_counter(r"\GPU Process Memory(*)\Shared Usage").unwrap_or_default());
        let get = |v: &[(u32, f64)], pid: u32| v.iter().find(|(p, _)| *p == pid).map_or(0.0, |(_, b)| b / MIB);
        pids.into_iter().map(|pid| (pid, get(&dedicated, pid), get(&shared, pid))).collect()
    }
}

impl GpuSource for SentinelGpu {
    fn snapshot(&self) -> Option<GpuSnapshot> {
        let nvml = self.nvml.as_ref().ok()?;
        let d = nvml.device_by_index(0).ok()?;
        let mem = d.memory_info().ok()?;
        // Peak bandwidth: bus width (bits) / 8 × max memory clock (MHz) × 2 (double data rate).
        let bandwidth_gbs = match (d.memory_bus_width(), d.max_clock_info(Clock::Memory)) {
            (Ok(bits), Ok(mhz)) if bits > 0 && mhz > 0 => Some(f64::from(bits) / 8.0 * f64::from(mhz) * 2.0 / 1000.0),
            _ => None,
        };
        let runners = self.runner_memory();
        let ram = system_ram_mib();
        Some(GpuSnapshot {
            gpu: GpuBudget {
                name: d.name().unwrap_or_else(|_| "GPU 0".into()),
                total_mib: mem.total as f64 / MIB,
                used_mib: mem.used as f64 / MIB,
                free_mib: mem.free as f64 / MIB,
                runners_mib: runners.iter().map(|r| r.1).sum(),
                runners_shared_mib: runners.iter().map(|r| r.2).sum(),
                headroom_mib: HEADROOM_MIB,
                bandwidth_gbs,
            },
            runners: runners.iter().map(|r| (r.0, r.1)).collect(),
            ram_total_mib: ram.map(|r| r.0),
            ram_free_mib: ram.map(|r| r.1),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_instances_sum_per_pid_and_runners_are_named() {
        let items = vec![
            ("pid_8236_luid_0x00000000_0x0000D3E1_phys_0".to_string(), 1.0 * MIB),
            ("pid_8236_luid_0x00000000_0x0000F001_phys_0".to_string(), 2.0 * MIB),
            ("pid_99_luid_0x0_0x1_phys_0".to_string(), 5.0),
            ("_Total".to_string(), 9.0),
        ];
        assert_eq!(per_pid(&items), vec![(8236, 3.0 * MIB), (99, 5.0)]);
        assert!(is_runner("llama-server.exe") && is_runner("ollama.exe"));
        assert!(!is_runner("ollama app.exe") && !is_runner("python.exe"));
        // Only Ollama's own runners count as memory a load can take over.
        assert!(is_ollama_runner(r"C:\Users\m\AppData\Local\Programs\Ollama\lib\ollama\llama-server.exe"));
        assert!(is_ollama_runner(r"C:\Users\m\AppData\Local\Programs\Ollama\ollama.exe"));
        assert!(!is_ollama_runner(r"C:\src\llama.cpp\build\bin\Release\llama-server.exe"));
        assert!(!is_ollama_runner(r"C:\Users\m\AppData\Local\Programs\Ollama\ollama app.exe"));
        assert!(!is_ollama_runner(r"C:\tools\python.exe"));
    }

    /// Reads NVML and PDH on this machine. Ignored by default (machine-specific).
    #[test]
    #[ignore]
    fn live_snapshot() {
        let s = SentinelGpu::open().snapshot();
        eprintln!("{s:#?}");
        eprintln!("ram {:?}", system_ram_mib());
    }
}
