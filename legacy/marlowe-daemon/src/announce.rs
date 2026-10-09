//! **What the daemon says about itself, kept as well as printed.**
//!
//! # The defect
//!
//! The daemon announces real things on its way up, and every one of them goes to stderr:
//!
//! ```text
//! marlowe: engine llama.cpp · http://127.0.0.1:11437 · started in 1935 ms · GPU-resident (96 tok/s measured)
//! marlowe: model provider ollama/llama.cpp · Ollama stores and lists · ...
//! marlowe: 3 interrupted run(s) can be resumed
//! ```
//!
//! Those are exactly the sentences a user wants when something is slow or wrong, and today they
//! are visible only if the daemon was started by hand in a terminal that is still open. §B17 makes
//! that the *unusual* case rather than the normal one: the launcher opens a window, Marlowe draws
//! the frame in it, and the daemon's stderr goes wherever the spawn left it. So the reason a first
//! turn took two seconds is a metre from the user's eye and unreachable.
//!
//! # Why a process-global, when nothing else in this crate is one
//!
//! Because the thing being mirrored *is* one. `eprintln!` writes to a process-wide handle from
//! anywhere, with no receiver threaded through, and the announcement sites are spread across
//! `Daemon::new` (before a `Daemon` exists), `HybridEngine::start` (a free constructor that never
//! sees one) and the middle of a turn. Threading a sink to all three would mean giving
//! `HybridEngine::start` a parameter whose only purpose is to be passed on, and the first site that
//! forgot it would go back to stderr silently — which is the bug, restored, with more code.
//!
//! The alternative considered and rejected was capturing the process's own stderr and parsing it
//! back. That reads the `[dev]` dumps too, it re-derives structure from prose that was never meant
//! to round-trip, and it breaks the moment a line wraps.
//!
//! # Bounded, and the bound is what stops it becoming a leak
//!
//! [`CAPACITY`] entries, oldest evicted. A daemon that runs for a week and switches models a
//! hundred times keeps the last [`CAPACITY`]; the sequence numbers survive eviction, so a client
//! that has fallen behind gets what is left rather than a silent gap that reads as *nothing
//! happened*.
//!
//! # No clock is read here
//!
//! Deliberately. CONTRACTS §4.5 fences real time into `crate::clock`, and an announcement does not
//! need a timestamp to be useful — it needs an **order**, which is what `seq` is. Adding a wall
//! time here would put a second clock read in the crate and buy a column nobody asked for.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::protocol::{AnnounceLevel, Announcement};

/// How many announcements are retained. Roughly a day of ordinary operation, and small enough that
/// the whole log fits in one `Status` frame without anyone thinking about paging.
pub const CAPACITY: usize = 64;

/// The count of announcements ever made. **Read without the lock**, so the streaming sink can ask
/// *"is there anything new"* on every event it forwards and pay a relaxed atomic load rather than a
/// mutex acquisition for the answer. At 70 tok/s the difference does not matter; at the next
/// engine's rate it might, and the cheap version is not harder to write.
static ISSUED: AtomicU64 = AtomicU64::new(0);

fn ring() -> &'static Mutex<VecDeque<(u64, Announcement)>> {
    static RING: OnceLock<Mutex<VecDeque<(u64, Announcement)>>> = OnceLock::new();
    RING.get_or_init(|| Mutex::new(VecDeque::with_capacity(CAPACITY)))
}

/// Print it and keep it. **Both, always** — this function is the only way to do either.
///
/// stderr is not dropped in favour of the wire. A benchmark's stderr is where several of these
/// lines are actually read (see `Daemon::ask_streaming_with`'s openrouter disclosure, which says
/// so), `marlowe --serve` in a terminal is still a supported way to run one, and a daemon that
/// stopped printing would break every script that greps for a line it has always printed.
pub fn say(level: AnnounceLevel, text: impl Into<String>) {
    let text = text.into();
    eprintln!("marlowe: {text}");
    let seq = ISSUED.fetch_add(1, Ordering::Relaxed) + 1;
    let Ok(mut ring) = ring().lock() else {
        // **A poisoned announcement log must not take the daemon with it.** Nothing here is load
        // bearing for a turn: the line has already been printed, which is where it went for the
        // whole of M1 and M2. Losing the retained copy degrades this feature and nothing else,
        // and invariant 4 says degrade rather than break.
        return;
    };
    if ring.len() == CAPACITY {
        ring.pop_front();
    }
    ring.push_back((seq, Announcement { level, text }));
}

/// A fact about the machine.
pub fn info(text: impl Into<String>) {
    say(AnnounceLevel::Info, text);
}

/// Something is not the way it was asked for. Amber, and counted in the pane's summary.
pub fn warn(text: impl Into<String>) {
    say(AnnounceLevel::Warn, text);
}

/// How many have been made. The high-water mark a streaming client compares against.
pub fn issued() -> u64 {
    ISSUED.load(Ordering::Relaxed)
}

/// Everything retained, oldest first. What `Status` carries.
pub fn retained() -> Vec<Announcement> {
    ring().lock().map(|r| r.iter().map(|(_, a)| a.clone()).collect()).unwrap_or_default()
}

/// Everything issued after `seq`, oldest first, with the sequence to ask from next time.
///
/// **The returned sequence is [`issued`], not the last item's.** They differ exactly when eviction
/// dropped something the caller had not seen, and returning the last *retained* sequence would make
/// the caller ask for the missing range forever, receiving it never. Advancing past the gap is the
/// honest version: the client has what survived.
pub fn since(seq: u64) -> (u64, Vec<Announcement>) {
    let now = issued();
    let items = ring()
        .lock()
        .map(|r| r.iter().filter(|(s, _)| *s > seq).map(|(_, a)| a.clone()).collect())
        .unwrap_or_default();
    (now, items)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The ring is process-global, so the tests that mutate it are one test.** Two `#[test]`
    /// functions appending to one ring would race inside cargo's thread pool and fail in whichever
    /// order the scheduler chose — the shared-resource hazard CLAUDE.md logs six forms of, in its
    /// smallest possible form.
    #[test]
    fn the_log_retains_what_it_printed_bounded_and_in_order() {
        let start = issued();
        info("engine llama.cpp · started in 1935 ms");
        warn("memory retrieval WRITE-ONLY — no --reranking directory");

        let (_, new) = since(start);
        assert_eq!(new.len(), 2, "both announcements were retained");
        assert_eq!(new[0].level, AnnounceLevel::Info);
        assert_eq!(new[1].level, AnnounceLevel::Warn);
        assert!(new[0].text.contains("1935 ms"), "{:?}", new[0].text);
        assert!(
            !new[0].text.starts_with("marlowe:"),
            "the prefix belongs to stderr's line format, not to the fact: {:?}",
            new[0].text
        );

        // Nothing before the mark comes back, which is what makes this usable as a flush.
        let (mark, _) = since(start);
        let (_, nothing) = since(mark);
        assert!(nothing.is_empty(), "a second flush returned what it had already given: {nothing:?}");

        // The bound holds, and the oldest is what goes.
        for i in 0..CAPACITY + 10 {
            info(format!("filler {i}"));
        }
        let all = retained();
        assert_eq!(all.len(), CAPACITY, "the ring is bounded");
        assert!(
            !all.iter().any(|a| a.text.contains("1935 ms")),
            "the oldest entries were evicted rather than the newest dropped"
        );
        assert!(all.last().is_some_and(|a| a.text.contains("filler")), "{:?}", all.last());
    }
}
