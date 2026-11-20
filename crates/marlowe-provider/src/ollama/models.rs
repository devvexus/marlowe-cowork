//! A local model's capabilities into [`ModelInfo`], and how each model takes the `think` knob.

use marlowe_model::{Effort, ModelInfo, ProviderKind};
use serde_json::Value;

/// What a local model can do, read from its files (`crate::llama::eligible`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelMeta {
    /// `None` when unknown (the file could not be read).
    pub capabilities: Option<Vec<String>>,
    /// Native context length.
    pub context: Option<u32>,
    pub family: Option<String>,
}

/// How a model accepts `think`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkMode {
    /// No thinking capability (or unknown): `think` is omitted.
    None,
    /// `think: true | false`.
    Bool,
    /// `think: "low" | "medium" | "high"` (gpt-oss); cannot be switched off.
    Levels,
}

impl ModelMeta {
    pub fn has(&self, cap: &str) -> bool {
        self.capabilities.as_ref().is_some_and(|c| c.iter().any(|x| x == cap))
    }

    /// False only when capabilities are known and lack `completion` (embedding models).
    pub fn can_chat(&self) -> bool {
        self.capabilities.as_ref().is_none_or(|c| c.iter().any(|x| x == "completion"))
    }

    pub fn think_mode(&self) -> ThinkMode {
        if !self.has("thinking") {
            ThinkMode::None
        } else if self.family.as_deref().is_some_and(|f| f.eq_ignore_ascii_case("gptoss")) {
            ThinkMode::Levels
        } else {
            ThinkMode::Bool
        }
    }

    /// True when every field a request needs is known.
    pub fn is_complete(&self) -> bool {
        self.capabilities.is_some() && self.context.is_some()
    }
}

/// The `think` value for `effort`, or `None` to omit the field.
pub fn think_value(mode: ThinkMode, effort: Effort) -> Option<Value> {
    match mode {
        ThinkMode::None => None,
        ThinkMode::Bool => Some(Value::Bool(effort != Effort::Off)),
        ThinkMode::Levels => Some(Value::String(
            match effort {
                Effort::Off | Effort::Low => "low",
                Effort::Medium => "medium",
                Effort::High | Effort::Max => "high",
            }
            .into(),
        )),
    }
}

/// Ollama tags that proxy to Ollama's hosted service rather than running here.
pub fn is_cloud_tag(name: &str) -> bool {
    let tag = name.rsplit_once(':').map(|(_, t)| t).unwrap_or("");
    tag == "cloud" || tag.ends_with("-cloud")
}

/// Catalog metadata. `context_window` is what requests will actually use (`num_ctx`).
pub fn model_info(name: &str, meta: &ModelMeta, num_ctx: u32) -> ModelInfo {
    let mut info = ModelInfo::bare(ProviderKind::Ollama, name);
    info.context_window = Some(meta.context.map_or(num_ctx, |c| c.min(num_ctx)));
    info.supports_tools = meta.has("tools");
    info.supports_vision = meta.has("vision");
    let mode = meta.think_mode();
    info.supports_reasoning = mode != ThinkMode::None;
    info.effort_levels = match mode {
        ThinkMode::None => Vec::new(),
        ThinkMode::Bool => vec![Effort::Off, Effort::Medium],
        ThinkMode::Levels => vec![Effort::Low, Effort::Medium, Effort::High],
    };
    info.local = !is_cloud_tag(name);
    info
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn effort_maps_to_the_think_field_each_model_accepts() {
        assert_eq!(think_value(ThinkMode::None, Effort::High), None);
        assert_eq!(think_value(ThinkMode::Bool, Effort::Off), Some(json!(false)));
        assert_eq!(think_value(ThinkMode::Bool, Effort::Low), Some(json!(true)));
        assert_eq!(think_value(ThinkMode::Levels, Effort::Off), Some(json!("low")));
        assert_eq!(think_value(ThinkMode::Levels, Effort::Max), Some(json!("high")));
    }

    #[test]
    fn model_info_reports_the_window_requests_will_use() {
        let meta = ModelMeta {
            capabilities: Some(vec!["completion".into(), "tools".into()]),
            context: Some(262_144),
            family: None,
        };
        let i = model_info("qwen2.5:14b", &meta, 32_768);
        assert_eq!(i.context_window, Some(32_768));
        assert!(i.supports_tools && !i.supports_reasoning && i.effort_levels.is_empty());
        assert!(i.local);
        let small = ModelMeta { context: Some(8_192), ..meta };
        assert_eq!(model_info("x", &small, 32_768).context_window, Some(8_192));
    }

    #[test]
    fn cloud_tags_are_not_local_and_embedders_cannot_chat() {
        assert!(is_cloud_tag("glm-5:cloud"));
        assert!(is_cloud_tag("qwen3.5:397b-cloud"));
        assert!(!is_cloud_tag("qwen3.5:9b"));
        assert!(!model_info("glm-5:cloud", &ModelMeta::default(), 32_768).local);
        let embed = ModelMeta { capabilities: Some(vec!["embedding".into()]), ..Default::default() };
        assert!(!embed.can_chat());
        assert!(ModelMeta::default().can_chat());
    }
}
