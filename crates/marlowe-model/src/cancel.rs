//! Cooperative cancellation with abort hooks.
//!
//! A blocking read cannot poll a flag, so a provider registers a hook (for example shutting
//! down a `TcpStream::try_clone`). On Windows a `shutdown` does not wake a thread already
//! blocked in `read`, so pair the hook with a short read timeout that checks
//! [`CancelToken::is_cancelled`] between reads.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

type Hook = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct Inner {
    cancelled: AtomicBool,
    next_id: AtomicU64,
    hooks: Mutex<Vec<(u64, Hook)>>,
}

/// A shared cancellation flag. Clones share state; cancel is one-way.
#[derive(Clone, Default)]
pub struct CancelToken {
    inner: Arc<Inner>,
}

impl CancelToken {
    /// A fresh, uncancelled token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the flag and runs every registered hook once. Idempotent.
    pub fn cancel(&self) {
        if self.inner.cancelled.swap(true, Ordering::SeqCst) {
            return;
        }
        let hooks = std::mem::take(&mut *lock(&self.inner.hooks));
        for (_, hook) in hooks {
            hook();
        }
    }

    /// True once [`cancel`](Self::cancel) has been called.
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// Registers a hook to run on cancel. Runs it immediately if already cancelled.
    /// Dropping the returned guard deregisters the hook.
    pub fn on_cancel(&self, hook: impl FnOnce() + Send + 'static) -> CancelHook {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        {
            let mut hooks = lock(&self.inner.hooks);
            // Checked under the lock so a concurrent cancel cannot miss this hook.
            if !self.inner.cancelled.load(Ordering::SeqCst) {
                hooks.push((id, Box::new(hook)));
                return CancelHook { inner: Arc::downgrade(&self.inner), id };
            }
        }
        hook();
        CancelHook { inner: Weak::new(), id }
    }

    /// Number of hooks currently registered.
    pub fn hook_count(&self) -> usize {
        lock(&self.inner.hooks).len()
    }
}

impl fmt::Debug for CancelToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancelToken").field("cancelled", &self.is_cancelled()).finish()
    }
}

/// Deregisters its hook on drop.
#[must_use = "dropping the guard deregisters the hook immediately"]
pub struct CancelHook {
    inner: Weak<Inner>,
    id: u64,
}

impl Drop for CancelHook {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            lock(&inner.hooks).retain(|(id, _)| *id != self.id);
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn cancel_sets_the_flag_for_every_clone() {
        let a = CancelToken::new();
        let b = a.clone();
        assert!(!b.is_cancelled());
        a.cancel();
        assert!(b.is_cancelled());
    }

    #[test]
    fn hooks_run_once_on_cancel() {
        let t = CancelToken::new();
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = n.clone();
        let _g = t.on_cancel(move || {
            n2.fetch_add(1, Ordering::SeqCst);
        });
        t.cancel();
        t.cancel();
        assert_eq!(n.load(Ordering::SeqCst), 1);
        assert_eq!(t.hook_count(), 0);
    }

    #[test]
    fn a_hook_registered_after_cancel_runs_immediately() {
        let t = CancelToken::new();
        t.cancel();
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = n.clone();
        let _g = t.on_cancel(move || {
            n2.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn dropping_the_guard_deregisters_the_hook() {
        let t = CancelToken::new();
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = n.clone();
        let g = t.on_cancel(move || {
            n2.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(t.hook_count(), 1);
        drop(g);
        assert_eq!(t.hook_count(), 0);
        t.cancel();
        assert_eq!(n.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_hook_wakes_a_blocked_waiter_on_another_thread() {
        let t = CancelToken::new();
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let _g = t.on_cancel(move || {
            let _ = tx.send(());
        });
        let t2 = t.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            t2.cancel();
        });
        rx.recv_timeout(std::time::Duration::from_secs(5)).expect("the hook ran");
        assert!(t.is_cancelled());
    }
}
