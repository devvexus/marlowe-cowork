//! Conversation messages.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Who authored a message. Tool results travel in a `Tool` message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Model reasoning, kept so it can be sent back on tool turns.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Thinking {
    pub text: String,
    /// Provider signature over the reasoning (e.g. Anthropic `signature`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Opaque provider payload to round-trip verbatim (e.g. OpenRouter `reasoning_details`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl Thinking {
    /// True when there is nothing to show or round-trip.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.signature.is_none() && self.details.is_none()
    }
}

/// One tool call requested by the model. `args` is the parsed JSON object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider id, or harness-assigned when the provider gives none.
    pub id: String,
    pub name: String,
    pub args: Value,
}

/// The result of one tool call, attributed by `call_id`.
///
/// `content` is the text the model reads. `parts` carries anything else the result holds (today:
/// [`Part::Image`] from `read` on a PNG, a notebook's image outputs, a browser screenshot);
/// providers that cannot put images in a tool result move them to a following user message
/// (track G). Old stored turns have no `parts` and load with it empty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub content: String,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<ResultPart>,
}

/// A non-text piece of a tool result. Serialized like the matching [`Part`]
/// (`{"type":"image",..}`), so a provider can lift it into a message unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResultPart {
    Image(ImageRef),
}

impl From<ResultPart> for Part {
    fn from(p: ResultPart) -> Part {
        match p {
            ResultPart::Image(i) => Part::Image(i),
        }
    }
}

impl ToolResult {
    /// A text-only result.
    pub fn text(call_id: impl Into<String>, content: impl Into<String>, is_error: bool) -> Self {
        Self { call_id: call_id.into(), content: content.into(), is_error, parts: Vec::new() }
    }

    /// The images this result carries, in order.
    pub fn images(&self) -> impl Iterator<Item = &ImageRef> {
        self.parts.iter().map(|p| match p {
            ResultPart::Image(i) => i,
        })
    }
}

/// An image in a message or tool result. The bytes live in the blake3-addressed blob store
/// (`<home>\blobs\<hex>`, track G's `marlowe-model/src/image.rs` writes and reads it); a
/// message carries only the reference, so stored turns stay small.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ImageRef {
    /// `image/png`, `image/jpeg`, `image/webp` or `image/gif`.
    pub mime: String,
    /// Lower-case hex blake3 of the encoded bytes.
    pub blob: String,
    /// Pixel width and height of the stored (already downscaled) image.
    pub w: u32,
    pub h: u32,
}

/// One piece of a message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Part {
    Text { text: String },
    Thinking(Thinking),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
    /// An image: user-attached (paste, `@img.png`) or, inside [`ToolResult::parts`], produced by
    /// a tool. Serialized `{"type":"image","mime":..,"blob":..,"w":..,"h":..}`.
    Image(ImageRef),
}

/// A message in a conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Part>,
}

impl Message {
    /// A message holding a single text part.
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self { role, content: vec![Part::Text { text: text.into() }] }
    }

    /// A user message.
    pub fn user(text: impl Into<String>) -> Self {
        Self::text(Role::User, text)
    }

    /// An assistant message.
    pub fn assistant(text: impl Into<String>) -> Self {
        Self::text(Role::Assistant, text)
    }

    /// A message holding text and images (a user turn with an attachment).
    pub fn with_images(role: Role, text: impl Into<String>, images: impl IntoIterator<Item = ImageRef>) -> Self {
        let mut content = vec![Part::Text { text: text.into() }];
        content.extend(images.into_iter().map(Part::Image));
        Self { role, content }
    }

    /// The images in this message (not those inside tool results), in order.
    pub fn images(&self) -> impl Iterator<Item = &ImageRef> {
        self.content.iter().filter_map(|p| match p {
            Part::Image(i) => Some(i),
            _ => None,
        })
    }

    /// A tool message carrying results.
    pub fn tool_results(results: impl IntoIterator<Item = ToolResult>) -> Self {
        Self { role: Role::Tool, content: results.into_iter().map(Part::ToolResult).collect() }
    }

    /// All text parts concatenated.
    pub fn joined_text(&self) -> String {
        self.content
            .iter()
            .filter_map(|p| match p {
                Part::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The tool calls in this message, in order.
    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.content.iter().filter_map(|p| match p {
            Part::ToolCall(c) => Some(c),
            _ => None,
        })
    }

    /// The tool results in this message, in order.
    pub fn tool_results_iter(&self) -> impl Iterator<Item = &ToolResult> {
        self.content.iter().filter_map(|p| match p {
            Part::ToolResult(r) => Some(r),
            _ => None,
        })
    }

    /// The reasoning part, if any.
    pub fn thinking(&self) -> Option<&Thinking> {
        self.content.iter().find_map(|p| match p {
            Part::Thinking(t) => Some(t),
            _ => None,
        })
    }
}
