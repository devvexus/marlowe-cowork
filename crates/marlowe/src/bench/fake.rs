//! A provider that answers from a closure, for the bench's own tests.

use marlowe_model::{
    CancelToken, ChatRequest, Completion, ModelInfo, Part, Provider, ProviderError, ProviderKind, Role, StopReason, StreamEvent, ToolCall,
};
use serde_json::Value;

/// What the closure returns.
pub struct Reply(pub Result<Completion, ProviderError>);

impl Reply {
    pub fn text(t: &str) -> Reply {
        Reply(Ok(Completion { text: t.into(), ..Completion::default() }))
    }

    pub fn call(name: &str, args: Value) -> Reply {
        Reply(Ok(Completion {
            tool_calls: vec![ToolCall { id: "call_1".into(), name: name.into(), args }],
            stop: StopReason::ToolUse,
            ..Completion::default()
        }))
    }
}

pub struct Fake {
    answer: Box<dyn Fn(&ChatRequest) -> Reply + Send + Sync>,
}

impl Fake {
    pub fn new(f: impl Fn(&ChatRequest) -> Reply + Send + Sync + 'static) -> Self {
        Self { answer: Box::new(f) }
    }

    /// A server that is down.
    pub fn failing() -> Self {
        Self::new(|_| Reply(Err(ProviderError::network("connection refused"))))
    }
}

impl Provider for Fake {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Ollama
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }

    fn stream(&self, req: &ChatRequest, _cancel: &CancelToken, _on: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        (self.answer)(req).0
    }
}

/// The text of the first user message of the request (the prompt the bench sent).
pub fn first_user_text(req: &ChatRequest) -> String {
    req.messages
        .iter()
        .find(|m| m.role == Role::User)
        .map(|m| {
            m.content
                .iter()
                .filter_map(|p| if let Part::Text { text } = p { Some(text.as_str()) } else { None })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// The text of the last user message.
pub fn last_user_text(req: &ChatRequest) -> String {
    req.messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .map(|m| {
            m.content
                .iter()
                .filter_map(|p| if let Part::Text { text } = p { Some(text.as_str()) } else { None })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}
