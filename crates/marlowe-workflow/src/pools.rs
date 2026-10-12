//! Token pools per session (WF8): the `+500k` target the user sets is one pool shared by the
//! conductor's main loop and every workflow the session starts. The conductor side (F3's
//! directive parser) sets it with [`set`] and adds the main loop's output tokens with
//! [`Budget::add`]; a workflow launched in the session reads it with [`get`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use marlowe_proto::SessionId;

use crate::engine::Budget;

fn pools() -> &'static Mutex<HashMap<SessionId, Arc<Budget>>> {
    static POOLS: OnceLock<Mutex<HashMap<SessionId, Arc<Budget>>>> = OnceLock::new();
    POOLS.get_or_init(Mutex::default)
}

/// Sets (or replaces) the session's target; the spend starts at zero.
pub fn set(session: &SessionId, total: u64) -> Arc<Budget> {
    let b = Budget::new(Some(total));
    pools().lock().unwrap_or_else(|e| e.into_inner()).insert(session.clone(), b.clone());
    b
}

/// The session's pool, when the user set a target.
pub fn get(session: &SessionId) -> Option<Arc<Budget>> {
    pools().lock().unwrap_or_else(|e| e.into_inner()).get(session).cloned()
}

/// The session ended or the target was cleared.
pub fn clear(session: &SessionId) {
    pools().lock().unwrap_or_else(|e| e.into_inner()).remove(session);
}
