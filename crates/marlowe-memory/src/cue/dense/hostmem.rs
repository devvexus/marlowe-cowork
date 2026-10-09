//! **Peak host working set, read from the OS, for the one number ONNX Runtime hides.**
//!
//! The subject is the ALiBi relative-distance matrix this export materialises —
//! `Abs(Range(0,N) − Range(0,N)ᵀ)` expanded to `[8, N, N]` at int64, so `8·N²·8` bytes, **per
//! session**. At `MAX_SEQ_LEN = 8192` that is 4,294,967,296 bytes and it succeeded silently
//! whenever the machine had 8.6 GB free; at 1024 it is 64 MB. Nothing inside this process can see
//! it: the allocation happens in ORT's arena, so a Rust allocator hook observes nothing and a
//! `sizeof` observes a declaration rather than a byte.
//!
//! So the reading comes from outside — the OS's own high-water mark for this process. It is
//! **peak**, not current, which is exactly right for a transient per-inference tensor: current
//! resident memory read after the forward pass has already given the arena back.
//!
//! # One definition, two callers, on purpose
//!
//! `examples/embed_memory.rs` reports this number and `tests/session_footprint.rs` asserts a bound
//! on it. Two copies of the query would let the reported figure and the asserted figure measure
//! subtly different things — the shape this project logs as "two producers of one value" — and the
//! failure would look like a threshold being wrong rather than an instrument being wrong.

/// Peak resident set for **this process**, in bytes, or `None` where it cannot be read.
///
/// `None` is a real answer and callers must not collapse it into zero: "the OS did not tell us" and
/// "this process has never held a byte" are different facts, and a bound asserted against a
/// silently-zero reading passes on every machine forever.
pub fn peak_working_set_bytes() -> Option<u64> {
    #[cfg(windows)]
    {
        // `Get-Process` rather than a `windows`/`winapi` dependency: this is a measurement path
        // that runs a handful of times in an example and once in a test, never on the scored path,
        // and adding a crate to the shipped dependency graph to read a diagnostic would be paying
        // in the wrong currency.
        //
        // **Through `vram::bounded_output`, which is the crate's only sanctioned way to run an
        // external command.** This spawned `powershell` through a bare `.output()` until
        // 2026-08-27 -- the exact shape that wedged the workspace suite from `vram.rs` two days
        // earlier, sitting one file away from the ceiling written to prevent it, unseen because
        // the guard greped a single module. `powershell` is not fast and it is not ours: profile
        // loading, execution policy and AV interception all sit between the spawn and the answer,
        // and any of them can take arbitrarily long. `CREATE_NO_WINDOW` and a null stdin come with
        // the helper -- a console child flashes a window otherwise, on a diagnostic path where the
        // user asked for nothing.
        let script = format!("(Get-Process -Id {}).PeakWorkingSet64", std::process::id());
        let out = crate::cue::dense::vram::bounded_output(
            "powershell",
            &["-NoProfile", "-NonInteractive", "-Command", &script],
        )?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8(out.stdout).ok()?.trim().parse::<u64>().ok()
    }
    #[cfg(not(windows))]
    {
        // `VmHWM` is the same quantity: the high-water mark of the resident set.
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let kb: u64 = status
            .lines()
            .find(|l| l.starts_with("VmHWM:"))?
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()?;
        Some(kb * 1024)
    }
}

/// The ALiBi relative-distance matrix's size, in bytes, at a given sequence length.
///
/// `[8, N, N]` int64 — eight heads, an `N × N` distance matrix, eight bytes an element.
///
/// **Stated as a function rather than as a comment because it is the prediction a measurement is
/// checked against**, and because the decomposition is not unique: `2 batch × 8 heads × 8192² ×
/// 4 bytes` gives the identical total and names a different tensor. A byte count that matches is
/// not thereby confirmed — this project has twice matched one to the wrong shape — so the shape
/// this returns is the one the graph's own `Expand` was read off, and the arithmetic is here so a
/// reader can re-derive it rather than trust it.
pub const fn alibi_matrix_bytes(seq_len: usize) -> u64 {
    8 * (seq_len as u64) * (seq_len as u64) * 8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cue::dense::MAX_SEQ_LEN;

    #[test]
    fn the_quadratic_term_is_the_one_that_moved() {
        // The two figures STATE.md records, re-derived here rather than quoted, so a change to
        // this function that broke the arithmetic could not keep agreeing with the write-up.
        assert_eq!(alibi_matrix_bytes(8192), 4_294_967_296);
        assert_eq!(alibi_matrix_bytes(1024), 67_108_864);
        // Quadratic, not linear: doubling N must quadruple the matrix.
        assert_eq!(alibi_matrix_bytes(2048), 4 * alibi_matrix_bytes(1024));
    }

    #[test]
    fn a_reading_is_either_absent_or_plausible() {
        // Cannot assert a value -- it is a live process on a shared machine. What it CAN assert is
        // that a successful read is not nonsense, which is what catches a units error: this
        // returns bytes, and an unconverted KB reading would land under a megabyte for a process
        // that has already loaded a test binary.
        match peak_working_set_bytes() {
            None => eprintln!("SKIP: the OS did not report a peak working set here"),
            Some(n) => assert!(
                n >= 1024 * 1024,
                "{n} bytes peak for a running test process -- the units are wrong"
            ),
        }
    }

    #[test]
    fn the_shipped_cap_predicts_a_footprint_in_megabytes_not_gigabytes() {
        // **Pure arithmetic, and it fires the instant `MAX_SEQ_LEN` is raised.** No model, no
        // session, no machine state -- so it cannot be flaky and it cannot be skipped. The ceiling
        // is a FIXED literal rather than a multiple of the live constant: a bound derived from
        // `MAX_SEQ_LEN` rises with it, which would leave the guard silent at exactly the moment
        // the thing it guards changed.
        //
        // 256 MB sits above 1024's 64 MB and below 2048's 268 MB, so the next step up is refused
        // and has to be argued for rather than absorbed.
        const CEILING_BYTES: u64 = 256 * 1024 * 1024;
        let predicted = alibi_matrix_bytes(MAX_SEQ_LEN);
        assert!(
            predicted <= CEILING_BYTES,
            "MAX_SEQ_LEN = {MAX_SEQ_LEN} predicts an ALiBi matrix of {} MB per session \
             (8 x N x N int64), over the {} MB ceiling. That tensor is transient per inference and \
             per session, so the real cost is this times the worker count. Raising the cap is a \
             decision with a memory bill attached -- if it is the right one, move this ceiling in \
             the same commit and say what was measured.",
            predicted / (1024 * 1024),
            CEILING_BYTES / (1024 * 1024)
        );
    }
}
