//! **ATTRIBUTING DEVICE MEMORY TO ONE SESSION, ON A CARD SHARED WITH EVERYTHING ELSE.**
//!
//! # The defect this exists to fix
//!
//! Two tests read free VRAM before a CUDA session loaded and after it ran, and asserted the drop
//! was at least the graph's size. **A free-memory delta is a measurement of the whole machine
//! attributed to one process.** Run alone the tests passed 8/8; in a workspace run they failed
//! intermittently, and the failure message named both hypotheses at once:
//!
//! > *"Either the provider silently fell back to CPU, or another process freed memory during the
//! > read"*
//!
//! Naming both is honest and it is exactly the problem: **the instrument could not tell them
//! apart**, so every red had to be interpreted by hand and the interpretation was wrong as often as
//! not. On 2026-08-27 the llama.cpp hybrid landed and daemon tests began taking and releasing
//! **6.7 GB** on the card mid-run, which turned an occasional false red into a frequent one.
//!
//! # What CANNOT be used, established rather than assumed
//!
//! **Per-process device memory is unavailable on this machine.** `nvidia-smi
//! --query-compute-apps=pid,used_memory` returns the literal `[N/A]` for every row under WDDM on a
//! GeForce card — checked again on 2026-08-27, driver 610.74, RTX 4080 SUPER — and
//! `--query-accounted-apps` returns nothing at all. `examples/embed_provider_bench.rs` already
//! carries that finding in `own_vram_bytes`'s doc.
//!
//! **ORT exposes no per-session allocation either.** `ort` 2.0.0-rc.10 wraps no allocator-stats
//! call, and `ort-sys`'s ONNX Runtime 1.22.0 bindings contain no `AllocatorGetStats` to wrap. The
//! same limitation that stops a session enumerating its own providers stops it reporting its own
//! bytes.
//!
//! So the card-wide reading is the only instrument there is, and the fix has to be in how it is
//! *bracketed*, not in what it reads.
//!
//! # The bracket: TOOK and GAVE, measured over two different windows
//!
//! ```text
//!   before ──▶ [ build + warm ] ──▶ during ──▶ [ drop ] ──▶ after
//!              └── window A ──┘               └─ window B ─┘
//!   took = before − during          gave = after − during
//!   residual = before − after  ==  took − gave
//! ```
//!
//! A session that genuinely holds device memory **takes it in window A and gives it back in window
//! B**, so `took ≈ gave` and the card returns to where it started. A third party moving memory
//! lands in **one** window, not both, and shows up as a residual. That is the discrimination the
//! single-ended delta could not make:
//!
//! | what happened | took | gave | residual | verdict |
//! |---|---|---|---|---|
//! | the session ran on the device | ≥ graph | ≥ graph | ~0 | **attributed** |
//! | **the provider silently fell back to CPU** | **0** | **0** | **~0** | **attributed, and the assertion FAILS** |
//! | a model server allocated 6.7 GB mid-window | huge | ~graph | −6.7 GB | contended, retry |
//! | a model server released 6.7 GB mid-window | 0 | huge | +6.7 GB | contended, retry |
//!
//! **Row 2 is the point.** A quiet card and a session that took nothing is not ambiguous, and it is
//! the failure this whole apparatus exists to catch — a CUDA session that silently ran on CPU, which
//! this project hit on 2026-08-27 at 10 tok/s against 107 with every health signal reading fine.
//! Contention no longer produces that verdict; it produces its own, with its own message.
//!
//! # THE PRIME, and it is a measured fact rather than a precaution
//!
//! The **first** CUDA session in a process creates the driver's primary context, and **that context
//! is not released when the session drops**. Measured here on 2026-08-27, three consecutive
//! load/warm/drop rounds in one process:
//!
//! ```text
//! round 1:  took 510.0 MB   gave 272.0 MB   residual 238.0 MB   <- primary context, kept
//! round 2:  took 273.0 MB   gave 274.0 MB   residual   1.0 MB
//! round 3:  took 182.0 MB   gave 176.0 MB   residual  −2.0 MB
//! ```
//!
//! Round 1's 238 MB residual is not contention and retrying does not clear it — it is a one-time
//! process cost that would be misread as a busy card forever. So one session is built and dropped
//! **before** any reading is taken. Rounds 2 and 3 are what the bracket sees, and their residuals
//! are 1 MB and 2 MB against a 124 MB graph.
//!
//! # What this does NOT do
//!
//! It does not make the card quiet. `exclusive("gpu")` is what serialises against the other GPU
//! tests in this workspace, and the caller must take it — this only *detects* that something moved,
//! it cannot stop it. Nor does it see node placement: M0c Session L measured 13.6% of nodes still
//! on CPU under a successfully registered CUDA session, and no instrument reachable from here can
//! see that.

#![allow(dead_code)]

use marlowe_memory::cue::dense::vram::free_bytes;

/// How many times a contaminated attempt is retried before the reading is reported as unobtainable.
///
/// A count, not a deadline — the same reasoning as `vram::bounded_output` and `exclusive`. Each
/// attempt is one session load plus one warm pass, so three is ~12 s worst case on this machine.
const ATTEMPTS: u32 = 3;

/// A clean reading, with the evidence that made it clean.
#[derive(Debug, Clone, Copy)]
pub struct Attributed {
    /// Bytes of device memory that disappeared while the session was live.
    pub took: u64,
    /// Bytes that came back when it dropped. **The attribution**: noise lands in one window.
    pub gave: u64,
    /// `took − gave`. Near zero is what makes the pair attributable to this session.
    pub residual: i64,
    /// Which attempt produced it. > 1 means the card was moving and the retry earned the reading.
    pub attempt: u32,
}

/// The outcome of one attributed measurement. **Four outcomes, deliberately not three:** the old
/// test collapsed "the card was busy" into "the assertion failed", which is the defect.
pub enum Reading {
    /// A delta this session can be held responsible for.
    Attributed(Attributed),
    /// No readable NVIDIA device. There is no byte to observe and no assertion to make.
    NoDevice,
    /// The session did not construct. The caller decides whether that is a skip or a failure.
    NotBuilt(String),
    /// Something else on the card moved memory during every attempt. **Not a verdict about the
    /// provider** — the log says what was seen so a human can tell which.
    Contended { attempts: u32, log: Vec<String> },
}

fn mb(b: u64) -> f64 {
    b as f64 / 1024.0 / 1024.0
}

fn signed_mb(b: i64) -> f64 {
    b as f64 / 1024.0 / 1024.0
}

/// Measure the device memory one session holds, attributing the delta to it.
///
/// `build_and_warm` must construct the session **and run a forward pass** — an unwarmed session
/// under-reads, because ORT's arena allocates on first run. The instrument drops what the closure
/// returns; that drop is half the measurement.
///
/// `tolerance` is the largest residual that still counts as attributable. Derive it from the thing
/// being measured rather than picking a number: it must sit **above** ordinary desktop jitter
/// (35 MB peak-to-peak over 6 s, measured on this box on 2026-08-27) and **well below** the floor
/// being asserted, or noise the size of the subject would be waved through.
///
/// `implausible_above` catches the one case the residual cannot: a third party that allocates in
/// window A and releases in window B inflates `took` and `gave` equally, leaving a clean residual.
/// Set it above anything this session could legitimately hold and below the model servers that
/// share the card (6.7 GB here). **If the caller's own assertion is a ceiling, this must sit above
/// that ceiling** — a contamination filter set below it would swallow the failure the test exists
/// to report.
pub fn attributed_device_delta<T>(
    tolerance: u64,
    implausible_above: u64,
    mut build_and_warm: impl FnMut() -> Result<T, String>,
) -> Reading {
    if free_bytes().is_none() {
        return Reading::NoDevice;
    }

    // **The prime.** See the module header: the first CUDA session in a process keeps ~238 MB of
    // primary context when it drops, which is a permanent residual and not contention.
    match build_and_warm() {
        Ok(primed) => drop(primed),
        Err(e) => return Reading::NotBuilt(e),
    }

    let mut log: Vec<String> = Vec::new();
    for attempt in 1..=ATTEMPTS {
        let Some(before) = free_bytes() else {
            return Reading::NoDevice;
        };
        let live = match build_and_warm() {
            Ok(t) => t,
            Err(e) => return Reading::NotBuilt(e),
        };
        let Some(during) = free_bytes() else {
            return Reading::NoDevice;
        };
        drop(live);
        let Some(after) = free_bytes() else {
            return Reading::NoDevice;
        };

        let took = before.saturating_sub(during);
        let gave = after.saturating_sub(during);
        let residual = before as i64 - after as i64;

        if residual.unsigned_abs() > tolerance {
            log.push(format!(
                "attempt {attempt}: took {:.1} MB but gave back {:.1} MB -- a residual of {:.1} MB \
                 against a {:.1} MB tolerance. The card did not return to where it started, so \
                 something other than this session moved memory inside the window.",
                mb(took),
                mb(gave),
                signed_mb(residual),
                mb(tolerance)
            ));
            continue;
        }
        if took > implausible_above || gave > implausible_above {
            log.push(format!(
                "attempt {attempt}: took {:.1} MB / gave {:.1} MB, over the {:.1} MB plausibility \
                 bound for one session. The residual is clean ({:.1} MB), which is what a third \
                 party allocating and releasing INSIDE the window looks like.",
                mb(took),
                mb(gave),
                mb(implausible_above),
                signed_mb(residual)
            ));
            continue;
        }

        return Reading::Attributed(Attributed { took, gave, residual, attempt });
    }

    Reading::Contended { attempts: ATTEMPTS, log }
}

/// The sentence a `Contended` reading turns into. **Deliberately says nothing about the provider.**
///
/// The old message offered two hypotheses and let the reader pick. This one reports the only thing
/// that was actually established: no reading could be attributed. If the provider is the suspect,
/// the way to find out is to re-run when the card is quiet — not to read it off a number that
/// cannot carry the claim.
pub fn contention_report(attempts: u32, log: &[String]) -> String {
    format!(
        "no reading could be ATTRIBUTED to this session in {attempts} attempts. Every attempt saw \
         another process move device memory inside the measurement window, so the card-wide delta \
         is not this session's.\n\n{}\n\nThis is NOT a verdict on the execution provider -- it is a \
         refusal to render one from a contaminated number, which is what the single-ended delta \
         this replaces used to do. The likely source is a concurrent GPU consumer: the daemon \
         tests start `llama-server` and Ollama runners that take and release 6.7 GB. Every \
         GPU-touching test in `marlowe-memory` takes `exclusive(\"gpu\")`; those do not, and until \
         they do this is reachable under a workspace run.",
        log.join("\n")
    )
}
