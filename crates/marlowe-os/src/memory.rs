//! Commit memory: how much Windows can still hand out, and an allocator that waits for it.
//!
//! An allocation fails on Windows when the system's commit charge reaches its limit (RAM plus the
//! page file), however much physical memory is free. A local model's runner can push it there in
//! seconds (llama-server's host-side prompt cache grows with every distinct prompt), and Rust's
//! default reaction is to abort the whole daemon: "memory allocation of N bytes failed". The runner
//! usually dies of the same shortage a moment later and its gigabytes come back, so [`Patient`]
//! waits for them instead of aborting at once. [`commit`] lets the crew hold new model calls
//! before the machine gets there.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// The system's commit memory now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Commit {
    /// Bytes that can still be committed.
    pub available: u64,
    /// The commit limit (RAM plus page file), bytes.
    pub limit: u64,
}

/// The system's commit memory, or `None` where it cannot be read (off Windows).
#[cfg(windows)]
pub fn commit() -> Option<Commit> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut s = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    // SAFETY: `s` is a writable MEMORYSTATUSEX with `dwLength` set, as the call requires.
    if unsafe { GlobalMemoryStatusEx(&mut s) } == 0 {
        return None;
    }
    Some(Commit { available: s.ullAvailPageFile, limit: s.ullTotalPageFile })
}

#[cfg(not(windows))]
pub fn commit() -> Option<Commit> {
    None
}

/// How long [`Patient`] waits for memory before it gives up (and the process aborts as before).
pub const PATIENCE: Duration = Duration::from_secs(30);
const RETRY: Duration = Duration::from_millis(100);

/// Allocations that failed at least once and were retried, process-wide.
static STALLS: AtomicU64 = AtomicU64::new(0);

/// Allocations [`Patient`] had to wait for since the process started.
pub fn stalls() -> u64 {
    STALLS.load(Ordering::Relaxed)
}

/// A global allocator that retries a failed allocation for up to [`PATIENCE`] before failing it.
/// Waiting allocates nothing, so it is safe inside the allocator.
pub struct Patient<A = System> {
    inner: A,
    tries: u32,
    pause: Duration,
}

impl Patient<System> {
    /// The system allocator, patient for [`PATIENCE`].
    pub const fn system() -> Self {
        Patient { inner: System, tries: (PATIENCE.as_millis() / RETRY.as_millis()) as u32, pause: RETRY }
    }
}

impl<A> Patient<A> {
    /// `inner`, retried `tries` times `pause` apart (tests).
    pub const fn with(inner: A, tries: u32, pause: Duration) -> Self {
        Patient { inner, tries, pause }
    }

    fn retry(&self, mut attempt: impl FnMut() -> *mut u8) -> *mut u8 {
        let mut p = attempt();
        if !p.is_null() {
            return p;
        }
        STALLS.fetch_add(1, Ordering::Relaxed);
        for _ in 0..self.tries {
            std::thread::sleep(self.pause);
            p = attempt();
            if !p.is_null() {
                break;
            }
        }
        p
    }
}

// SAFETY: every call is forwarded to `inner` with the caller's own arguments; a retry repeats the
// same call after the previous attempt returned null (nothing was allocated), so the contract of
// `GlobalAlloc` holds exactly as `inner` keeps it.
unsafe impl<A: GlobalAlloc> GlobalAlloc for Patient<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged (see the impl).
        self.retry(|| unsafe { self.inner.alloc(layout) })
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged (see the impl).
        self.retry(|| unsafe { self.inner.alloc_zeroed(layout) })
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged (see the impl).
        unsafe { self.inner.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: a failed realloc leaves `ptr` valid and unchanged, so repeating it is the same call.
        self.retry(|| unsafe { self.inner.realloc(ptr, layout, new_size) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    /// The system allocator, failing its first `fail` allocations as a full commit limit does.
    struct Short {
        fail: AtomicU32,
    }

    unsafe impl GlobalAlloc for Short {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            if self.fail.load(Ordering::SeqCst) > 0 {
                self.fail.fetch_sub(1, Ordering::SeqCst);
                return std::ptr::null_mut();
            }
            unsafe { System.alloc(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    #[test]
    fn a_shortage_that_passes_is_waited_out_and_one_that_lasts_still_fails() {
        // The daemon died of "memory allocation of 5505024 bytes failed" while llama-server held
        // the last of the commit limit; the runner died of it too and gave 15 GB back.
        let layout = Layout::from_size_align(5_505_024, 8).unwrap();
        let passing = Patient::with(Short { fail: AtomicU32::new(3) }, 5, Duration::from_millis(1));
        let before = stalls();
        let p = unsafe { passing.alloc(layout) };
        assert!(!p.is_null(), "three failures, then memory came back");
        unsafe { passing.dealloc(p, layout) };
        assert!(stalls() > before, "the wait is counted");

        let lasting = Patient::with(Short { fail: AtomicU32::new(100) }, 5, Duration::from_millis(1));
        assert!(unsafe { lasting.alloc(layout) }.is_null(), "it gives up after its tries");
    }

    #[test]
    fn the_commit_limit_is_read() {
        if let Some(c) = commit() {
            assert!(c.limit > 0 && c.available <= c.limit, "{c:?}");
        }
    }
}
