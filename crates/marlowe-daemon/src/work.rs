//! Background work the daemon does on its own, outside any turn: a memory embed, a voice model
//! loading, a brief polish, an MCP server starting. Each is a [`Work`] guard that says so on the
//! bus as a `Work` phase for as long as it lives, so a client shows it live (one line, one clock);
//! nothing that costs a model load, a helper process or real CPU/GPU time runs unseen. Dropping
//! the guard ends the phase with the measured time.

use std::sync::{Arc, Weak};
use std::time::Instant;

use marlowe_proto::{Event, PhaseEvent, PhaseKind};

use crate::daemon::Core;

/// One running piece of background work. `stage` names it (`embed`, `voice`, `mcp:<server>`,
/// `brief`), `detail` says what, and is what a client draws.
pub(crate) struct Work {
    /// Weak: a guard kept in a callback the core owns (the Mind's sink) must not keep the core alive.
    core: Weak<Core>,
    stage: String,
    began: Instant,
}

impl Work {
    pub fn start(core: &Arc<Core>, stage: impl Into<String>, detail: impl Into<String>) -> Work {
        let stage = stage.into();
        core.bus.publish(Event::Phase(PhaseEvent::start(PhaseKind::Work).stage(stage.clone()).detail(detail)));
        Work { core: Arc::downgrade(core), stage, began: Instant::now() }
    }

    /// `done` of `total` `unit`s so far. Not kept for replay: a client that joins mid-way sees
    /// the next one.
    pub fn progress(&self, done: u64, total: u64, unit: &str) {
        let Some(core) = self.core.upgrade() else { return };
        let p = PhaseEvent::progress(PhaseKind::Work, done, total).stage(self.stage.clone()).unit(unit);
        core.bus.publish_ephemeral(Event::Phase(p));
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        let Some(core) = self.core.upgrade() else { return };
        let ms = self.began.elapsed().as_millis() as u64;
        core.bus.publish(Event::Phase(PhaseEvent::end(PhaseKind::Work, ms).stage(self.stage.clone())));
    }
}
