//! What the user is looking at in their editor, put in front of the model at the start of a turn
//! (U14). The note comes from the connected editor's `get_context` ([`marlowe_mcp::ide`]); with
//! no editor, or one that does not answer within a second and a half, the turn starts without it.
//!
//! The note joins the turn as pending context, the way a hook's added context does, so memory
//! still sees only what the user typed. It is sent when it changed since the last turn.

use std::sync::{Arc, Mutex};

use marlowe_agent::{Conversation, ToolMiddleware, TurnFlow};
use marlowe_mcp::hub::Hub;
use marlowe_mcp::ide;

pub(super) struct IdeContext {
    hub: Arc<Hub>,
    last: Mutex<Option<String>>,
}

impl IdeContext {
    pub(super) fn new(hub: Arc<Hub>) -> Self {
        Self { hub, last: Mutex::new(None) }
    }
}

impl ToolMiddleware for IdeContext {
    fn name(&self) -> &str {
        "ide_context"
    }

    fn before_turn(&self, convo: &mut Conversation, _text: &mut String) -> TurnFlow {
        let note = ide::context_note(&self.hub, ide::CONTEXT_WAIT);
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = &note {
            if last.as_deref() != Some(n.as_str()) {
                convo.pending_context.push(n.clone());
            }
        }
        *last = note;
        TurnFlow::Continue
    }
}
