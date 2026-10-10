//! A session's conversation state, owned by its runner thread.

use std::collections::HashSet;
use std::path::PathBuf;

use marlowe_model::{Message, Part};
use marlowe_proto::SessionId;

use crate::middleware::Extensions;
use crate::tools::{Exposure, ToolHost};

/// The messages of one session plus loop bookkeeping.
#[derive(Debug, Clone)]
pub struct Conversation {
    pub session: SessionId,
    pub workspace: Option<PathBuf>,
    /// Full history in order. Compaction replaces a prefix with `summary`.
    pub messages: Vec<Message>,
    /// The harness summary of compacted history, if any.
    pub summary: Option<String>,
    /// Tools the user allowed for the rest of this session.
    pub session_allows: HashSet<String>,
    /// Turn counter; the next turn gets this index.
    pub next_turn: u32,
    /// Untrusted reads and approved hosts; drives the approval gate.
    pub exposure: Exposure,
    /// Per-session state owned by middleware and later tracks (one value per type).
    pub ext: Extensions,
}

impl Conversation {
    /// An empty conversation.
    pub fn new(session: SessionId, workspace: Option<PathBuf>) -> Self {
        Self {
            session,
            workspace,
            messages: Vec::new(),
            summary: None,
            session_allows: HashSet::new(),
            next_turn: 0,
            exposure: Exposure::default(),
            ext: Extensions::new(),
        }
    }

    /// Rebuilds a conversation from stored messages.
    pub fn from_messages(session: SessionId, workspace: Option<PathBuf>, messages: Vec<Message>, next_turn: u32) -> Self {
        Self { messages, next_turn, ..Self::new(session, workspace) }
    }

    pub fn push(&mut self, m: Message) {
        self.messages.push(m);
    }

    pub fn is_allowed_for_session(&self, tool: &str) -> bool {
        self.session_allows.contains(tool)
    }

    pub fn allow_for_session(&mut self, tool: impl Into<String>) {
        self.session_allows.insert(tool.into());
    }

    /// Marks the conversation tainted if any call in its history brought in untrusted content.
    /// Used after loading a session from the store.
    pub fn recompute_exposure(&mut self, host: &dyn ToolHost) {
        let tainted = self
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .any(|p| matches!(p, Part::ToolCall(c) if host.taints(c)));
        self.exposure.tainted |= tainted;
    }
}
