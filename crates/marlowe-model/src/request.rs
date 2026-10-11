//! What the loop sends to a provider.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::message::Message;

/// Reasoning effort. Providers map it to their own knob; `Off` disables reasoning.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    Off,
    Low,
    #[default]
    Medium,
    High,
    Max,
}

impl Effort {
    /// Every level, lowest first.
    pub const ALL: [Effort; 5] = [Effort::Off, Effort::Low, Effort::Medium, Effort::High, Effort::Max];

    /// Lower-case name, as on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Off => "off",
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Max => "max",
        }
    }

    /// The next level within `allowed`, wrapping; `self` if `allowed` is empty.
    pub fn cycle(self, allowed: &[Effort]) -> Effort {
        if allowed.is_empty() {
            return self;
        }
        match allowed.iter().position(|e| *e == self) {
            Some(i) => allowed[(i + 1) % allowed.len()],
            None => allowed[0],
        }
    }
}

impl fmt::Display for Effort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Effort {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Effort::ALL
            .into_iter()
            .find(|e| e.as_str().eq_ignore_ascii_case(s.trim()))
            .ok_or_else(|| format!("unknown effort '{s}' (off, low, medium, high, max)"))
    }
}

/// A tool the model may call. `input_schema` is a JSON Schema object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// One block of the system prompt. `cache` marks a stable prefix boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemBlock {
    pub text: String,
    #[serde(default)]
    pub cache: bool,
}

/// A complete request to a provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    /// Provider-native model id (e.g. `qwen3:8b`, `anthropic/claude-sonnet-4.5`).
    pub model: String,
    #[serde(default)]
    pub effort: Effort,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub system: Vec<SystemBlock>,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Constrains the reply to this JSON Schema (Ollama `format`, OpenRouter `response_format`).
    /// A workflow agent's finalisation call uses it (WF3); providers without it ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<Value>,
}

impl ChatRequest {
    /// A request with defaults for everything but the model and messages.
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            effort: Effort::default(),
            max_output_tokens: None,
            system: Vec::new(),
            messages,
            tools: Vec::new(),
            temperature: None,
            format: None,
        }
    }

    /// The system blocks joined with blank lines, for wires with a single system string.
    pub fn system_text(&self) -> String {
        self.system.iter().map(|b| b.text.as_str()).collect::<Vec<_>>().join("\n\n")
    }
}
