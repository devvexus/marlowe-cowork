//! **Children in `/runs`.** ADR-057, M3 Session B1.
//!
//! # The gap this closes, and how it stayed invisible
//!
//! `Daemon::ask_streaming_with` inserts one `RunSummary` — the turn the daemon accepted — and
//! that was the **only** writer of the live run table. `Engine::spawn` creates children, journals
//! `RunSpawned`, and knows nothing about a control plane; so a spawned child existed in the log
//! and in no listing. `/runs` showed the parent alone and the run window's roster panel is
//! hardcoded `subagents: Vec::new()`.
//!
//! Nothing looked wrong, because **no model call could produce a spawn** until this session. A
//! roster that is empty because the tree is empty and a roster that is empty because nothing fills
//! it read identically, and the product was in the first state for the whole of M2. Session F's
//! panel and Session A's orphan policy were both correct and both unobservable.
//!
//! # Why a recorder and not a control-plane call from the loop
//!
//! `marlowe-loop` has no dependency on the daemon and must not acquire one — the engine is a
//! state machine over injected ports, which is what makes "one loop, many capability profiles"
//! testable. It already emits everything needed: `RunSpawned` names the child, `Checkpointed`
//! carries its spend and status, `RunCompleted` names its fate. So the daemon **listens** on the
//! port it already owns rather than the loop **telling** a component it should not know exists.
//!
//! This is a decorator over the journal recorder, not a replacement: every event still reaches the
//! journal, in order, through the same writer. If the roster update fails there is nothing to
//! report — a run table is a projection, and the log is the record.

use marlowe_contract::Clock;
use marlowe_journal::EventKind;
use marlowe_loop::record::Recorder;
use marlowe_loop::{RunId, SessionId};

use crate::control_plane::Shared;
use crate::daemon::RunSummary;

/// Wraps a recorder and keeps the control plane's run table in step with the tree the loop builds.
pub struct RosterRecorder<R: Recorder> {
    inner: R,
    plane: Shared,
}

impl<R: Recorder> RosterRecorder<R> {
    pub fn new(inner: R, plane: Shared) -> Self {
        Self { inner, plane }
    }
}

/// Whether a run in this state has stopped for good.
///
/// Read from `RunStatus`'s own serialization rather than from `Checkpoint::is_terminal`, because
/// what this module holds is the encoded payload and decoding a whole checkpoint to answer one
/// question would put a second decoder beside `JournalCheckpoints`. The words are the enum's, and
/// `a_terminal_word_matches_run_status_and_a_live_one_does_not` pins them against it, so a renamed
/// variant fails the build rather than making every child immortal in the listing.
///
/// **`detached` and `adopted` are deliberately absent.** They are orphan *fates*, and both mean the
/// child is still going — under a new parent, or under none. Only `terminated` ends a run, and it
/// arrives as `cancelled` on the child's own amended checkpoint.
pub fn is_terminal_word(w: &str) -> bool {
    matches!(w, "completed" | "failed" | "cancelled")
}

/// A `RunStatus` as it appears in a serialized checkpoint, flattened to the one word the run table
/// holds.
///
/// `RunStatus` is `#[serde(rename_all = "snake_case")]`, so a unit variant is a bare string and a
/// data-carrying one is a single-key object. **Both are handled, and an unrecognised shape returns
/// `None` rather than a plausible default** — a run listed as `running` because its status could
/// not be read is the surface asserting a state nothing holds, which is the mistake
/// `seed_from_journal` documents at length for exactly this table.
fn status_word(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Object(m) => m.keys().next().cloned(),
        _ => None,
    }
}

impl<R: Recorder> Recorder for RosterRecorder<R> {
    fn append(
        &mut self,
        clock: Clock,
        kind: EventKind,
        run: RunId,
        session: SessionId,
        payload: serde_json::Value,
    ) -> Result<u64, String> {
        // **The journal write happens first and is never conditional on the projection.** The log
        // is the record; the run table is a view over it. Updating the view before the write would
        // let a listing describe an event that failed to persist.
        let seq = self.inner.append(clock, kind, run, session, payload.clone())?;

        match kind {
            EventKind::RunSpawned => {
                let Some(child) = payload.get("child").and_then(|c| c.as_str()) else {
                    return Ok(seq);
                };
                let depth = payload.get("depth").and_then(|d| d.as_u64()).unwrap_or(0) as u8;
                let mut plane = self.plane.lock().expect("the control plane lock was poisoned");
                plane.runs.entry(child.to_string()).or_insert_with(|| RunSummary {
                    status: "running".into(),
                    depth,
                    started_ms: clock.now_ms.max(0) as u64,
                    ..RunSummary::accepted(child.to_string())
                });
            }
            // The child's own per-iteration checkpoint. This is what makes a listed child's spend
            // move while it works, rather than sitting at the zero it was inserted with.
            EventKind::Checkpointed => {
                let mut plane = self.plane.lock().expect("the control plane lock was poisoned");
                let Some(s) = plane.runs.get_mut(&run.to_string()) else {
                    return Ok(seq);
                };
                if let Some(t) = payload.pointer("/spent/tokens").and_then(|t| t.as_u64()) {
                    s.tokens = t;
                }
                if let Some(m) = payload.pointer("/spent/micros_usd").and_then(|t| t.as_u64()) {
                    s.spend_micros_usd = m;
                }
                let word = payload.get("status").and_then(status_word);
                if let Some(w) = &word {
                    s.status = w.clone();
                }

                // ── THE ELAPSED, AND THE DEFECT THAT MADE IT NECESSARY ────────────────────
                //
                // **Found in the first live run, not by a test.** `ControlPlane::detail` reads
                // *"final when there is one, live otherwise"*: a row whose `elapsed_ms` is `0` is
                // reported as `now - started_ms`, which is right for a run still going and wrong
                // the moment one stops. A child inserted here and never given a final time
                // therefore reported a **longer elapsed the later anybody looked** — the first
                // real spawn showed a completed child at 32,804 ms inside a parent that took
                // 3,536 ms, which is not a thing that can happen to a blocking spawn.
                //
                // The parent's row is closed by `ask_streaming_with` from `run.spent.wall_ms`
                // when the turn ends. A child has no such moment on the daemon's side, so its
                // final time comes from the same place its spend does: its own last checkpoint,
                // taken when the status is terminal. That keeps one definition of *"how long did
                // this run take"* rather than two that can disagree.
                if word.as_deref().is_some_and(is_terminal_word) {
                    if let Some(w) = payload.pointer("/spent/wall_ms").and_then(|t| t.as_u64()) {
                        s.elapsed_ms = w;
                    }
                }
            }
            // `settle_children` records the fate under the PARENT's run id, naming the child in the
            // payload — so the row to update is the one the payload names, not the one the event
            // was appended against.
            EventKind::RunCompleted => {
                let (Some(child), Some(fate)) = (
                    payload.get("child").and_then(|c| c.as_str()),
                    payload.get("fate").and_then(|f| f.as_str()),
                ) else {
                    return Ok(seq);
                };
                let mut plane = self.plane.lock().expect("the control plane lock was poisoned");
                if let Some(s) = plane.runs.get_mut(child) {
                    s.status = fate.to_string();
                }
            }
            _ => {}
        }
        Ok(seq)
    }
}

// **The child's own transcript is not routed anywhere by this module, and that is deliberate.**
// §10.2's rule is that an orchestrator's context must never accumulate raw worker history, and
// `Engine::spawn` makes it structural by dropping the child's `SessionState`. A roster is a list
// of runs; it is not a second door onto what they read. Stated here so the next change to this
// file has to notice the rule before adding one.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_word_is_read_from_both_shapes_and_never_guessed() {
        assert_eq!(status_word(&serde_json::json!("running")), Some("running".into()));
        assert_eq!(
            status_word(&serde_json::json!({ "paused": { "reason": "budget_exhausted" } })),
            Some("paused".into())
        );
        // The one that matters: an unreadable status leaves the row alone rather than calling it
        // running. A run listed as running because nobody could read its status is the failure
        // `seed_from_journal` names.
        assert_eq!(status_word(&serde_json::json!(7)), None);
        assert_eq!(status_word(&serde_json::json!(null)), None);
    }

    /// **The terminal words are pinned against the enum that produces them**, not against a list
    /// somebody typed. `is_terminal_word` decides whether a child's elapsed is closed at its final
    /// value or left to grow forever, so a renamed `RunStatus` variant that silently stopped
    /// matching would make every completed child report a longer time the later anyone looked —
    /// which is the defect this function was written for, returning by a different route.
    #[test]
    fn a_terminal_word_matches_run_status_and_a_live_one_does_not() {
        use marlowe_loop::run::{PauseReason, RunStatus};

        let word = |s: &RunStatus| {
            status_word(&serde_json::to_value(s).expect("RunStatus encodes"))
                .expect("every variant yields a word")
        };

        for terminal in [
            RunStatus::Completed,
            RunStatus::Failed { error: "x".into() },
            RunStatus::Cancelled,
        ] {
            let w = word(&terminal);
            assert!(is_terminal_word(&w), "{terminal:?} encodes as {w:?}, which is not matched");
        }
        for live in [
            RunStatus::Queued,
            RunStatus::Running,
            RunStatus::Paused { reason: PauseReason::AwaitingAnswer },
            RunStatus::WaitingEvent { until: None },
        ] {
            let w = word(&live);
            assert!(
                !is_terminal_word(&w),
                "{live:?} encodes as {w:?} and would close a run that is still going"
            );
        }
    }
}
