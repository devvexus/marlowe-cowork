//! Where an employee gets a place to run.
//!
//! The crew has one pool of model slots (`ContextGate::crew_slots`: the planner's slots less the
//! conductor's one) shared by every run. A project's employees take a slot per call from that
//! pool and give it back after the call; the conductor never takes one from itself and never
//! waits on an employee. The pool lives in `marlowe-crew`, so this crate sees it through
//! [`ModelGate`]; [`SlotPool`] is a self-contained implementation (tests, and a headless run with
//! no crew).

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use marlowe_model::{CancelToken, Usage};
use thiserror::Error;

/// Why a call could not be admitted.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GateError {
    /// The run was cancelled while waiting.
    #[error("cancelled")]
    Cancelled,
    /// The gate refused the call (out of memory, no model); the text is harness-written.
    #[error("{0}")]
    Refused(String),
}

/// Permission to make one model call. Hand it back with [`Permit::finish`] (booking what the call
/// spent) or just drop it.
#[must_use = "dropping the permit frees the slot at once"]
pub struct Permit {
    release: Option<Box<dyn FnOnce(Option<&Usage>) + Send>>,
}

impl Permit {
    /// A permit that runs `release` when it ends.
    pub fn new(release: impl FnOnce(Option<&Usage>) + Send + 'static) -> Permit {
        Permit { release: Some(Box::new(release)) }
    }

    /// A permit that holds nothing (a gate with no limits).
    pub fn free() -> Permit {
        Permit { release: None }
    }

    /// Ends the call: frees the slot and books `usage` (`None` when the call did not finish).
    pub fn finish(mut self, usage: Option<&Usage>) {
        if let Some(r) = self.release.take() {
            r(usage);
        }
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        if let Some(r) = self.release.take() {
            r(None);
        }
    }
}

/// The pool of model slots. `enter` blocks, honouring `cancel`; it never runs on the conductor's
/// thread.
pub trait ModelGate: Send + Sync {
    /// Waits for a slot of `model` and returns the permit to call it.
    fn enter(&self, model: &str, cancel: &CancelToken) -> Result<Permit, GateError>;

    /// How many calls of `model` can run at once from this project (a hint for the scheduler: it
    /// starts no more model tasks than this, so priority holds instead of threads queueing on the
    /// pool in arrival order). At least 1.
    fn width(&self, model: &str) -> usize;
}

/// A gate with no limits.
#[derive(Debug, Clone, Copy, Default)]
pub struct Unlimited;

impl ModelGate for Unlimited {
    fn enter(&self, _model: &str, cancel: &CancelToken) -> Result<Permit, GateError> {
        if cancel.is_cancelled() {
            return Err(GateError::Cancelled);
        }
        Ok(Permit::free())
    }

    fn width(&self, _model: &str) -> usize {
        8
    }
}

/// A counting pool: `limit(model)` calls at once, the rest wait. Cloning shares the pool.
#[derive(Clone)]
pub struct SlotPool {
    inner: Arc<PoolInner>,
}

struct PoolInner {
    limit: Box<dyn Fn(&str) -> usize + Send + Sync>,
    state: Mutex<HashMap<String, Held>>,
    freed: Condvar,
    calls: AtomicUsize,
}

#[derive(Default, Clone, Copy)]
struct Held {
    now: usize,
    peak: usize,
}

impl SlotPool {
    /// A pool with the same limit for every model.
    pub fn new(limit: usize) -> SlotPool {
        SlotPool::per_model(move |_| limit)
    }

    /// A pool whose limit depends on the model.
    pub fn per_model(limit: impl Fn(&str) -> usize + Send + Sync + 'static) -> SlotPool {
        SlotPool { inner: Arc::new(PoolInner { limit: Box::new(limit), state: Mutex::new(HashMap::new()), freed: Condvar::new(), calls: AtomicUsize::new(0) }) }
    }

    /// Slots of `model` in use now.
    pub fn held(&self, model: &str) -> usize {
        self.lock().get(model).map_or(0, |h| h.now)
    }

    /// The most slots of `model` that were ever in use at once.
    pub fn peak(&self, model: &str) -> usize {
        self.lock().get(model).map_or(0, |h| h.peak)
    }

    /// Permits handed out so far.
    pub fn calls(&self) -> usize {
        self.inner.calls.load(Ordering::SeqCst)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Held>> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl ModelGate for SlotPool {
    fn enter(&self, model: &str, cancel: &CancelToken) -> Result<Permit, GateError> {
        let limit = (self.inner.limit)(model).max(1);
        let mut state = self.lock();
        loop {
            if cancel.is_cancelled() {
                return Err(GateError::Cancelled);
            }
            let h = state.entry(model.to_string()).or_default();
            if h.now < limit {
                h.now += 1;
                h.peak = h.peak.max(h.now);
                self.inner.calls.fetch_add(1, Ordering::SeqCst);
                let (inner, name) = (self.inner.clone(), model.to_string());
                return Ok(Permit::new(move |_| {
                    let mut state = inner.state.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(h) = state.get_mut(&name) {
                        h.now = h.now.saturating_sub(1);
                    }
                    drop(state);
                    inner.freed.notify_all();
                }));
            }
            state = self.inner.freed.wait_timeout(state, Duration::from_millis(20)).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    fn width(&self, model: &str) -> usize {
        (self.inner.limit)(model).max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pool_never_lends_more_than_its_limit_and_a_waiter_gets_the_freed_slot() {
        let pool = SlotPool::new(2);
        let cancel = CancelToken::new();
        let a = pool.enter("m", &cancel).unwrap();
        let b = pool.enter("m", &cancel).unwrap();
        assert_eq!(pool.held("m"), 2);
        let (p2, c2) = (pool.clone(), cancel.clone());
        let waiter = std::thread::spawn(move || p2.enter("m", &c2).map(|p| p.finish(None)).is_ok());
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(pool.held("m"), 2, "the third caller is waiting");
        a.finish(None);
        assert!(waiter.join().unwrap());
        b.finish(None);
        assert_eq!((pool.held("m"), pool.peak("m"), pool.calls()), (0, 2, 3));
    }

    #[test]
    fn cancel_wakes_a_waiter_and_a_dropped_permit_frees_its_slot() {
        let pool = SlotPool::new(1);
        let cancel = CancelToken::new();
        let held = pool.enter("m", &cancel).unwrap();
        let (p2, c2) = (pool.clone(), cancel.clone());
        let waiter = std::thread::spawn(move || p2.enter("m", &c2).err());
        std::thread::sleep(Duration::from_millis(40));
        cancel.cancel();
        assert_eq!(waiter.join().unwrap(), Some(GateError::Cancelled));
        drop(held);
        assert_eq!(pool.held("m"), 0);
    }

    #[test]
    fn models_have_their_own_limits() {
        let pool = SlotPool::per_model(|m| if m == "big" { 1 } else { 3 });
        assert_eq!((pool.width("big"), pool.width("small")), (1, 3));
    }
}
