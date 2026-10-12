//! Token pools per session (WF8): the `+500k` target the user sets is one pool shared by the
//! conductor's main loop and every workflow the session starts. The daemon [`link`]s each
//! session's conductor pool (F3's `TokenPool`, armed by the directive parser) when it builds the
//! session; a workflow launched in the session reads it with [`get`] and charges it through
//! [`Budget::add`]. [`set`] is a standalone pool (tests, a session without a conductor).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use marlowe_proto::SessionId;

use crate::engine::{Budget, PoolLink};

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

/// Links the session to its conductor's pool: every reading and charge goes there.
pub fn link(session: &SessionId, pool: Arc<dyn PoolLink>) -> Arc<Budget> {
    let b = Budget::linked(pool);
    pools().lock().unwrap_or_else(|e| e.into_inner()).insert(session.clone(), b.clone());
    b
}

/// The session's pool, when the user set a target or the session is linked.
pub fn get(session: &SessionId) -> Option<Arc<Budget>> {
    pools().lock().unwrap_or_else(|e| e.into_inner()).get(session).cloned()
}

/// The session ended or the target was cleared.
pub fn clear(session: &SessionId) {
    pools().lock().unwrap_or_else(|e| e.into_inner()).remove(session);
}
