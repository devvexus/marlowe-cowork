//! The memory port. Wired to `marlowe-memory` in M2.

use marlowe_proto::SessionId;

/// Retrieval before a turn and capture after it.
pub trait MemoryHook: Send + Sync {
    /// Blocks to inject into the context for this turn. Only retrieved memories, never invented.
    fn before_turn(&self, session: &SessionId, query: &str) -> Vec<String>;

    /// Called once a turn completes with its user text and final assistant text.
    fn after_turn(&self, session: &SessionId, user: &str, assistant: &str);
}

/// No memory (M1).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoMemory;

impl MemoryHook for NoMemory {
    fn before_turn(&self, _session: &SessionId, _query: &str) -> Vec<String> {
        Vec::new()
    }

    fn after_turn(&self, _session: &SessionId, _user: &str, _assistant: &str) {}
}
