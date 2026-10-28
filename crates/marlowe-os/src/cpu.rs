//! How busy this process keeps the CPU: its own processor time between two readings, as a share
//! of what the whole machine could have given in that time. A meter for the screen, not an
//! accounting: the runner of a local model is another process and is not in it.

use std::time::{Duration, Instant};

/// Processor time this process has used so far (kernel plus user), in 100 ns ticks. `None` where
/// it cannot be read (off Windows).
#[cfg(windows)]
pub fn process_ticks() -> Option<u64> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: the pseudo-handle of the current process is always valid, and the four FILETIMEs are
    // writable locals, as the call requires.
    let ok = unsafe { GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user) };
    if ok == 0 {
        return None;
    }
    let ticks = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    Some(ticks(kernel).saturating_add(ticks(user)))
}

/// Processor time this process has used so far, in 100 ns ticks (unreadable off Windows).
#[cfg(not(windows))]
pub fn process_ticks() -> Option<u64> {
    None
}

/// The percentage of the machine's processors (`cores` of them) that `ticks` of processor time
/// used in `wall`, between 0 and 100.
pub fn percent_of_machine(ticks: u64, wall: Duration, cores: usize) -> f64 {
    let capacity = wall.as_secs_f64() * 1e7 * cores.max(1) as f64;
    if capacity <= 0.0 {
        return 0.0;
    }
    (ticks as f64 / capacity * 100.0).clamp(0.0, 100.0)
}

/// Reads [`process_ticks`] twice and says how busy the process was in between.
pub struct CpuMeter {
    read: fn() -> Option<u64>,
    cores: usize,
    last: Option<(Instant, u64)>,
}

impl CpuMeter {
    /// A meter over this process.
    pub fn new() -> CpuMeter {
        CpuMeter::with(process_ticks, std::thread::available_parallelism().map_or(1, usize::from))
    }

    /// A meter over `read` on a machine of `cores` processors (tests).
    pub fn with(read: fn() -> Option<u64>, cores: usize) -> CpuMeter {
        CpuMeter { read, cores, last: None }
    }

    /// The share of the machine used since the previous call; `None` on the first call (nothing
    /// to compare with) and where the time cannot be read.
    pub fn sample(&mut self) -> Option<f64> {
        let now = (Instant::now(), (self.read)()?);
        let before = self.last.replace(now)?;
        Some(percent_of_machine(now.1.saturating_sub(before.1), now.0.duration_since(before.0), self.cores))
    }
}

impl Default for CpuMeter {
    fn default() -> Self {
        CpuMeter::new()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    #[test]
    fn the_share_is_of_the_whole_machine_and_stays_in_range() {
        let second = Duration::from_secs(1);
        // One core busy for the whole second on an eight-core machine.
        assert!((percent_of_machine(10_000_000, second, 8) - 12.5).abs() < 1e-9);
        assert_eq!(percent_of_machine(10_000_000 * 40, second, 8), 100.0, "clamped");
        assert_eq!(percent_of_machine(5, Duration::ZERO, 8), 0.0);
        assert_eq!(percent_of_machine(0, second, 0), 0.0);
    }

    static TICKS: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn a_meter_needs_two_readings_and_reports_the_time_between_them() {
        fn read() -> Option<u64> {
            Some(TICKS.load(Ordering::SeqCst))
        }
        let mut m = CpuMeter::with(read, 4);
        assert_eq!(m.sample(), None, "the first reading has nothing before it");
        std::thread::sleep(Duration::from_millis(30));
        TICKS.store(5_000_000, Ordering::SeqCst); // half a second of processor time in ~30 ms of wall time
        let busy = m.sample().expect("a second reading");
        assert!(busy > 99.0, "more processor time than the machine has: clamped near the top, got {busy}");
        fn nothing() -> Option<u64> {
            None
        }
        assert_eq!(CpuMeter::with(nothing, 4).sample(), None, "unreadable time is no number, not zero");
    }

    #[test]
    fn this_process_has_used_some_processor_time_on_windows() {
        let spin = std::time::Instant::now();
        while spin.elapsed() < Duration::from_millis(20) {}
        assert_eq!(process_ticks().is_some(), cfg!(windows));
    }
}
