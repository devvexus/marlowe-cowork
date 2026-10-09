//! The memory port. Implemented by the daemon over `marlowe-mind`.

use marlowe_proto::SessionId;

/// What memory adds to one turn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recollection {
    /// Blocks to inject into the context. Only retrieved memories, never invented.
    pub blocks: Vec<String>,
    /// At least one block carries untrusted lineage; the loop then marks the run tainted.
    pub untrusted: bool,
}

/// Retrieval before a turn and capture after it.
pub trait MemoryHook: Send + Sync {
    /// Memories to inject into the context for this turn.
    fn before_turn(&self, session: &SessionId, query: &str) -> Recollection;

    /// Called once a turn completes with its user text and final assistant text.
    fn after_turn(&self, session: &SessionId, user: &str, assistant: &str);
}

/// No memory.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoMemory;

impl MemoryHook for NoMemory {
    fn before_turn(&self, _session: &SessionId, _query: &str) -> Recollection {
        Recollection::default()
    }

    fn after_turn(&self, _session: &SessionId, _user: &str, _assistant: &str) {}
}
