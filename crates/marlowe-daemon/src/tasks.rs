//! The daemon's background task registry (design 3.12): one [`MemoryTaskRegistry`] for the
//! daemon's life, reached with [`registry`], installed as the [`Tasks`] service of every session
//! (`task_ops::services`), and mirrored to clients as `Event::TaskUpdated`.
//!
//! The seam gives the table, the broadcast and the service. Track N fills the rest here: the
//! [`marlowe_agent::TaskSource`]s for B's job table and the crew's runs and background agents,
//! the injection drain at a step boundary, and the waker that starts a turn on an idle session
//! (`SessionWake`). Workflows (W) and monitors and crons (N) register their own tasks.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use marlowe_agent::{MemoryTaskRegistry, TaskRegistry};
use marlowe_proto::Event;

use crate::daemon::Core;

#[derive(Default)]
struct TaskState {
    registry: Arc<MemoryTaskRegistry>,
    broadcasting: AtomicBool,
}

fn state(core: &Core) -> Arc<TaskState> {
    core.ext.get_or_init(TaskState::default)
}

/// The daemon's registry.
pub(crate) fn registry(core: &Core) -> Arc<MemoryTaskRegistry> {
    state(core).registry.clone()
}

/// The registry as the trait object the services carry.
pub(crate) fn handle(core: &Core) -> Arc<dyn TaskRegistry> {
    registry(core)
}

/// Broadcasts every task change to the clients, once. The listener holds the core weakly, so a
/// registry kept alive by a service never keeps a stopped daemon alive.
pub(crate) fn start_broadcast(core: &Arc<Core>) {
    let st = state(core);
    if st.broadcasting.swap(true, Ordering::SeqCst) {
        return;
    }
    let weak = Arc::downgrade(core);
    st.registry.subscribe(Arc::new(move |task| {
        if let Some(core) = weak.upgrade() {
            core.bus.publish(Event::TaskUpdated(task.clone()));
        }
    }));
}
