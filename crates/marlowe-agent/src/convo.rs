//! A session's conversation state, owned by its runner thread.

use std::collections::HashSet;
use std::path::PathBuf;

use marlowe_model::{Message, Part, Role};
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
    /// Context for the next user message that is not the user's words (hook output): [`push`]
    /// attaches it to that message as parts of its own, after the text, so the model reads it
    /// while the memory engine, given the user's text alone, never ingests it as theirs.
    ///
    /// [`push`]: Conversation::push
    pub pending_context: Vec<String>,
    /// Images attached to the next user message (pasted, dropped or `@img.png`, track H; stored
    /// with `PutBlob`): [`push`] adds them to that message after its text.
    ///
    /// [`push`]: Conversation::push
    pub pending_images: Vec<marlowe_model::ImageRef>,
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
            pending_context: Vec::new(),
            pending_images: Vec::new(),
        }
    }

    /// Rebuilds a conversation from stored messages.
    pub fn from_messages(session: SessionId, workspace: Option<PathBuf>, messages: Vec<Message>, next_turn: u32) -> Self {
        Self { messages, next_turn, ..Self::new(session, workspace) }
    }

    /// Appends `m`; a user message also takes the pending context ([`Conversation::pending_context`]).
    pub fn push(&mut self, mut m: Message) {
        if m.role == Role::User && !self.pending_images.is_empty() {
            m.content.extend(self.pending_images.drain(..).map(Part::Image));
        }
        if m.role == Role::User && !self.pending_context.is_empty() {
            m.content.extend(self.pending_context.drain(..).map(|text| Part::Text { text }));
        }
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
