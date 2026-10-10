//! Shadow-repo checkpoints and the per-call turn rows, wired into every session (track F2).
//!
//! - The engine is `marlowe_agent::checkpoint`: one bare repository per workspace under
//!   `<home>\checkpoints`, snapshots in `pre_mutation`, `/rewind`, `/diff`. This file owns the
//!   daemon's single [`CheckpointStore`] (in [`Core::ext`](crate::daemon::Core)) and the stack
//!   constructors `stack.rs` calls.
//! - [`recorder`] writes the conversation to the store as the loop runs, so a killed daemon
//!   resumes losing at most the model call in flight.

mod recorder;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use marlowe_agent::checkpoint::{CheckpointMiddleware, CheckpointStore, Warn};
use marlowe_agent::{Conversation, ToolMiddleware, TurnObserver};
use marlowe_proto::{Event, SessionId};

pub(crate) use recorder::{repair_dangling, SessionRecorder};

use crate::daemon::Core;
use crate::lock;

/// The daemon's checkpoint store, created on first use. The first use also starts the 30-day
/// prune on a thread, a minute later, so it never slows a session's start.
pub(crate) fn store(core: &Arc<Core>) -> Arc<CheckpointStore> {
    let created = AtomicBool::new(false);
    let store = core.ext.get_or_init(|| {
        created.store(true, Ordering::SeqCst);
        CheckpointStore::new(core.home.join("checkpoints"))
    });
    if created.load(Ordering::SeqCst) {
        let pruner = store.clone();
        let _ = std::thread::Builder::new().name("marlowe-checkpoint-prune".into()).spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(60));
            pruner.prune_all();
        });
    }
    store
}

/// Recorders by session, for the daemon's life.
#[derive(Default)]
struct Recorders(Mutex<HashMap<SessionId, Arc<SessionRecorder>>>);

/// The recorder for `convo`'s session, created (positioned after the stored history) on first
/// use by the session's runner.
pub(crate) fn recorder(core: &Arc<Core>, convo: &Conversation) -> Arc<SessionRecorder> {
    let all = core.ext.get_or_init(Recorders::default);
    let mut map = lock(&all.0);
    map.entry(convo.session.clone())
        .or_insert_with(|| {
            Arc::new(SessionRecorder::new(convo.session.clone(), core.store.clone(), core.mind.clone(), convo.messages.len()))
        })
        .clone()
}

/// The recorder of a session whose runner is up.
pub(crate) fn recorder_of(core: &Arc<Core>, session: &SessionId) -> Option<Arc<SessionRecorder>> {
    core.ext.get::<Recorders>().and_then(|all| lock(&all.0).get(session).cloned())
}

/// A runner is exiting: its recorder goes with it (a reload builds a new one).
pub(crate) fn release(core: &Arc<Core>, session: &SessionId) {
    if let Some(all) = core.ext.get::<Recorders>() {
        lock(&all.0).remove(session);
    }
}

/// F2's loop middleware for one session: checkpoints before mutations, then the turn rows.
/// The rows come last so their `after_call` sees an outcome other middleware already amended.
pub(crate) fn middleware(core: &Arc<Core>, convo: &Conversation) -> Vec<Arc<dyn ToolMiddleware>> {
    let c = core.clone();
    let warn: Warn = Arc::new(move |session, message| {
        c.bus.publish(Event::Error { session: Some(session.clone()), message });
    });
    vec![Arc::new(CheckpointMiddleware::new(store(core), warn)), recorder(core, convo)]
}

/// F2's observers for one session (turn rows per model call and per tool batch).
pub(crate) fn observers(core: &Arc<Core>, convo: &Conversation) -> Vec<Arc<dyn TurnObserver>> {
    vec![recorder(core, convo)]
}
