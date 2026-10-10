//! Shadow-repo checkpoints `<home>\checkpoints\<repo-hash>.git` and `/rewind` (design 3.4). Track F2; empty until it lands (declared by the seam so no track edits `lib.rs`).
//! Its middleware (snapshots in `pre_mutation`) and observers (per-call and per-batch turn rows)
//! are added to every session by `stack.rs`.

use std::sync::Arc;

use marlowe_agent::{Conversation, ToolMiddleware, TurnObserver};

use crate::daemon::Core;

/// F2's loop middleware for one session (checkpoints before mutations).
pub(crate) fn middleware(_core: &Arc<Core>, _convo: &Conversation) -> Vec<Arc<dyn ToolMiddleware>> {
    Vec::new()
}

/// F2's observers for one session (turn rows per model call and per tool batch).
pub(crate) fn observers(_core: &Arc<Core>, _convo: &Conversation) -> Vec<Arc<dyn TurnObserver>> {
    Vec::new()
}
