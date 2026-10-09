//! `/api/tags` and `/api/show` into [`ModelInfo`], and how each model takes the `think` knob.

use marlowe_model::{Effort, ModelInfo, ProviderKind};
use serde_json::Value;

/// What Ollama reports about one model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelMeta {
    /// `None` when the server did not say (older Ollama without `capabilities`).
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

/// One `/api/tags` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagEntry {
    pub name: String,
    pub meta: ModelMeta,
}

/// Parses `/api/tags`. `None` when the body is not Ollama's shape.
pub fn parse_tags(v: &Value) -> Option<Vec<TagEntry>> {
    let models = v.get("models")?.as_array()?;
    Some(
        models
            .iter()
            .filter_map(|m| {
                let name = m.get("name").or_else(|| m.get("model"))?.as_str()?.to_string();
                if name.is_empty() {
                    return None;
                }
                let details = m.get("details");
                let meta = ModelMeta {
                    capabilities: string_array(m.get("capabilities")),
                    context: details
                        .and_then(|d| d.get("context_length"))
                        .and_then(Value::as_u64)
                        .map(clamp_u32),
                    family: details
                        .and_then(|d| d.get("family"))
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                };
                Some(TagEntry { name, meta })
            })
            .collect(),
    )
}

/// Parses `/api/show`.
pub fn parse_show(v: &Value) -> ModelMeta {
    let info = v.get("model_info");
    let arch = info
        .and_then(|i| i.get("general.architecture"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let context = arch
        .as_ref()
        .and_then(|a| info?.get(format!("{a}.context_length"))?.as_u64())
        .or_else(|| {
            // Some architectures name the key differently; take any `*.context_length`.
            info?.as_object()?.iter().find_map(|(k, v)| {
                k.ends_with(".context_length").then(|| v.as_u64()).flatten()
            })
        })
        .map(clamp_u32);
    let family = v
        .get("details")
        .and_then(|d| d.get("family"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or(arch);
    ModelMeta { capabilities: string_array(v.get("capabilities")), context, family }
}

/// Fills fields `base` lacks from `other`.
pub fn merge(base: &ModelMeta, other: &ModelMeta) -> ModelMeta {
    ModelMeta {
        capabilities: base.capabilities.clone().or_else(|| other.capabilities.clone()),
        context: base.context.or(other.context),
        family: base.family.clone().or_else(|| other.family.clone()),
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

fn string_array(v: Option<&Value>) -> Option<Vec<String>> {
    Some(v?.as_array()?.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
}

fn clamp_u32(n: u64) -> u32 {
    n.min(u32::MAX as u64) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tags_with_capabilities_need_no_show_call() {
        let v = json!({"models":[
            {"name":"qwen3.5:9b","details":{"family":"qwen35","context_length":262144},
             "capabilities":["vision","completion","tools","thinking"]},
            {"name":"old:1b","details":{"family":"llama"}}
        ]});
        let tags = parse_tags(&v).unwrap();
        assert!(tags[0].meta.is_complete());
        assert_eq!(tags[0].meta.think_mode(), ThinkMode::Bool);
        assert!(!tags[1].meta.is_complete());
        assert!(parse_tags(&json!({"nope": 1})).is_none());
    }

    #[test]
    fn show_reads_the_architecture_context_key() {
        let v = json!({
            "capabilities": ["completion", "tools", "thinking"],
            "details": {"family": "gptoss"},
            "model_info": {"general.architecture": "gptoss", "gptoss.context_length": 131072}
        });
        let m = parse_show(&v);
        assert_eq!(m.context, Some(131072));
        assert_eq!(m.think_mode(), ThinkMode::Levels);
    }

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
