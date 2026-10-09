//! Providers, model identity and catalog metadata.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::request::Effort;

/// A model provider. Only Ollama and OpenRouter are built in M1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ProviderKind {
    #[serde(rename = "ollama")]
    Ollama,
    #[serde(rename = "openrouter")]
    OpenRouter,
    #[serde(rename = "local_gguf")]
    LocalGguf,
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "openai")]
    OpenAi,
}

impl ProviderKind {
    /// Every provider, in onboarding display order.
    pub const ALL: [ProviderKind; 5] = [
        ProviderKind::Ollama,
        ProviderKind::LocalGguf,
        ProviderKind::OpenRouter,
        ProviderKind::Anthropic,
        ProviderKind::OpenAi,
    ];

    /// True when an adapter exists today.
    pub fn is_available_now(self) -> bool {
        matches!(self, ProviderKind::Ollama | ProviderKind::OpenRouter)
    }

    /// True when models run on this machine.
    pub fn is_local(self) -> bool {
        matches!(self, ProviderKind::Ollama | ProviderKind::LocalGguf)
    }

    /// Stable lower-case id used on the wire and in model refs.
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderKind::Ollama => "ollama",
            ProviderKind::OpenRouter => "openrouter",
            ProviderKind::LocalGguf => "local_gguf",
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::OpenAi => "openai",
        }
    }

    /// Human-facing name.
    pub fn display_name(self) -> &'static str {
        match self {
            ProviderKind::Ollama => "Ollama",
            ProviderKind::OpenRouter => "OpenRouter",
            ProviderKind::LocalGguf => "Local GGUF",
            ProviderKind::Anthropic => "Anthropic",
            ProviderKind::OpenAi => "OpenAI",
        }
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ProviderKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        ProviderKind::ALL
            .into_iter()
            .find(|k| k.as_str().eq_ignore_ascii_case(s) || k.display_name().eq_ignore_ascii_case(s))
            .ok_or_else(|| format!("unknown provider '{s}'"))
    }
}

/// A model in the library: `provider/model_id`, e.g. `ollama/qwen3:8b` or
/// `openrouter/anthropic/claude-sonnet-4.5`. Serialized as that string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ModelRef {
    pub provider: ProviderKind,
    pub model_id: String,
}

/// A string that is not `provider/model_id`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("'{0}' is not a model ref (expected provider/model_id)")]
pub struct ModelRefError(pub String);

impl ModelRef {
    pub fn new(provider: ProviderKind, model_id: impl Into<String>) -> Self {
        Self { provider, model_id: model_id.into() }
    }
}

impl fmt::Display for ModelRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.provider, self.model_id)
    }
}

impl FromStr for ModelRef {
    type Err = ModelRefError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (p, id) = s.split_once('/').ok_or_else(|| ModelRefError(s.to_string()))?;
        let provider = p.parse().map_err(|_| ModelRefError(s.to_string()))?;
        if id.trim().is_empty() {
            return Err(ModelRefError(s.to_string()));
        }
        Ok(ModelRef { provider, model_id: id.to_string() })
    }
}

impl TryFrom<String> for ModelRef {
    type Error = ModelRefError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<ModelRef> for String {
    fn from(r: ModelRef) -> String {
        r.to_string()
    }
}

/// Price in USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Pricing {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
}

impl Pricing {
    /// Cost of a call at this price.
    pub fn cost(&self, input_tokens: u64, output_tokens: u64) -> f64 {
        (input_tokens as f64 * self.input_per_mtok + output_tokens as f64 * self.output_per_mtok)
            / 1_000_000.0
    }
}

/// Catalog metadata for one model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub provider: ProviderKind,
    /// Provider-native id.
    pub id: String,
    pub display: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output: Option<u32>,
    #[serde(default)]
    pub supports_tools: bool,
    #[serde(default)]
    pub supports_reasoning: bool,
    /// Effort levels this model accepts; empty means effort is ignored.
    #[serde(default)]
    pub effort_levels: Vec<Effort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<Pricing>,
    #[serde(default)]
    pub local: bool,
}

impl ModelInfo {
    /// Minimal info for a model id with nothing else known.
    pub fn bare(provider: ProviderKind, id: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            provider,
            display: id.clone(),
            id,
            context_window: None,
            max_output: None,
            supports_tools: false,
            supports_reasoning: false,
            effort_levels: Vec::new(),
            pricing: None,
            local: provider.is_local(),
        }
    }

    /// The library ref for this model.
    pub fn model_ref(&self) -> ModelRef {
        ModelRef::new(self.provider, self.id.clone())
    }
}
