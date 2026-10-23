//! Marlowe's llama.cpp engine at work, shown as background work: a model loading is one live
//! line with its clock for as long as the load lasts, and an idle unload is said once.

use std::sync::{Arc, Weak};

use marlowe_model::EngineObserver;

use crate::daemon::Core;
use crate::work::Work;

struct EngineWork(Weak<Core>);

impl EngineObserver for EngineWork {
    fn loading(&self, model: &str, detail: &str) -> Box<dyn Send> {
        match self.0.upgrade() {
            Some(core) => Box::new(Work::start(&core, format!("llama:{model}"), format!("loading {model} on llama.cpp ({detail})"))),
            None => Box::new(()),
        }
    }

    fn note(&self, line: &str) {
        if let Some(core) = self.0.upgrade() {
            // Starts and ends at once: the line is on the bus for any client to show.
            drop(Work::start(&core, "llama", line.to_string()));
        }
    }
}

/// Hands the engine the daemon's bus.
pub(crate) fn attach(core: &Arc<Core>) {
    core.brains.factory().allocations().set_engine_observer(Arc::new(EngineWork(Arc::downgrade(core))));
}
