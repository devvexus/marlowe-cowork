//! `GET /api/v1/models` into [`ModelInfo`].
//!
//! Prices are per token as decimal strings; they become USD per million tokens. A negative price
//! (`"-1"`, routers with variable pricing) is reported as unknown. Models that cannot produce
//! text are skipped.

use marlowe_model::{Effort, ModelInfo, Pricing, ProviderKind};
use serde_json::Value;

/// Reasoning metadata for one model, kept for mapping [`Effort`] onto the request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReasoningMeta {
    pub supported: bool,
    /// Reasoning cannot be switched off.
    pub mandatory: bool,
    /// OpenRouter effort names the model accepts (`none`, `minimal`, `low`, ..., `max`); empty
    /// when the catalog does not list them.
    pub efforts: Vec<String>,
}

/// One parsed catalog entry.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogEntry {
    pub info: ModelInfo,
    pub reasoning: ReasoningMeta,
    /// Whether the model takes image input, when the catalog says (`architecture.input_modalities`).
    pub vision: Option<bool>,
}

/// Parses the catalog. Errors when the body is not the expected shape.
pub fn parse_catalog(json: &str) -> Result<Vec<CatalogEntry>, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("the catalog is not JSON: {e}"))?;
    let data = v
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| "the catalog has no `data` array".to_string())?;
    let mut out: Vec<CatalogEntry> = data.iter().filter_map(parse_entry).collect();
    out.sort_by(|a, b| a.info.id.cmp(&b.info.id));
    out.dedup_by(|a, b| a.info.id == b.info.id);
    Ok(out)
}

fn parse_entry(m: &Value) -> Option<CatalogEntry> {
    let id = m.get("id")?.as_str()?.trim();
    if id.is_empty() {
        return None;
    }
    let outputs = m
        .get("architecture")
        .and_then(|a| a.get("output_modalities"))
        .and_then(Value::as_array);
    if let Some(outs) = outputs {
        if !outs.iter().any(|o| o.as_str() == Some("text")) {
            return None;
        }
    }

    let vision = m
        .get("architecture")
        .and_then(|a| a.get("input_modalities"))
        .and_then(Value::as_array)
        .map(|ins| ins.iter().any(|i| i.as_str() == Some("image")));

    let params: Vec<&str> = m
        .get("supported_parameters")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let reasoning_obj = m.get("reasoning").filter(|r| r.is_object());
    let supported = params.contains(&"reasoning") || params.contains(&"include_reasoning") || reasoning_obj.is_some();
    let reasoning = ReasoningMeta {
        supported,
        mandatory: reasoning_obj
            .and_then(|r| r.get("mandatory"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        efforts: reasoning_obj
            .and_then(|r| r.get("supported_efforts"))
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|e| e.as_str().map(str::to_ascii_lowercase)).collect())
            .unwrap_or_default(),
    };

    let mut info = ModelInfo::bare(ProviderKind::OpenRouter, id);
    if let Some(name) = m.get("name").and_then(Value::as_str).filter(|n| !n.trim().is_empty()) {
        info.display = name.trim().to_string();
    }
    let top = m.get("top_provider");
    info.context_window = m
        .get("context_length")
        .and_then(Value::as_u64)
        .or_else(|| top?.get("context_length")?.as_u64())
        .map(clamp_u32);
    info.max_output = top
        .and_then(|t| t.get("max_completion_tokens"))
        .and_then(Value::as_u64)
        .map(clamp_u32);
    info.supports_tools = params.contains(&"tools");
    info.supports_reasoning = supported;
    info.supports_vision = vision == Some(true);
    info.effort_levels = effort_levels(&reasoning);
    info.pricing = pricing(m.get("pricing"));
    info.local = false;
    Some(CatalogEntry { info, reasoning, vision })
}

/// The [`Effort`] levels a model offers.
pub fn effort_levels(r: &ReasoningMeta) -> Vec<Effort> {
    if !r.supported {
        return Vec::new();
    }
    let mut levels: Vec<Effort> = if r.efforts.is_empty() {
        vec![Effort::Low, Effort::Medium, Effort::High]
    } else {
        r.efforts.iter().filter_map(|e| from_openrouter(e)).collect()
    };
    if !r.mandatory && !levels.contains(&Effort::Off) {
        levels.push(Effort::Off);
    }
    if r.mandatory {
        levels.retain(|e| *e != Effort::Off);
    }
    levels.sort();
    levels.dedup();
    levels
}

fn from_openrouter(e: &str) -> Option<Effort> {
    Some(match e {
        "none" => Effort::Off,
        "minimal" | "low" => Effort::Low,
        "medium" => Effort::Medium,
        "high" | "xhigh" => Effort::High,
        "max" => Effort::Max,
        _ => return None,
    })
}

/// The request's `reasoning` object for `effort`, or `None` to omit it.
///
/// `meta` is `None` when the model is not in the cached catalog; the mapping then uses the
/// common effort names and lets OpenRouter normalise.
pub fn reasoning_field(effort: Effort, meta: Option<&ReasoningMeta>) -> Option<Value> {
    if let Some(m) = meta {
        if !m.supported {
            return None;
        }
        if effort == Effort::Off {
            if m.mandatory {
                return None;
            }
            if m.efforts.iter().any(|e| e == "none") {
                return Some(serde_json::json!({ "effort": "none" }));
            }
            return Some(serde_json::json!({ "enabled": false }));
        }
    } else if effort == Effort::Off {
        return Some(serde_json::json!({ "enabled": false }));
    }
    let prefs: &[&str] = match effort {
        Effort::Off => return None,
        Effort::Low => &["low", "minimal", "medium"],
        Effort::Medium => &["medium", "high", "low"],
        Effort::High => &["high", "xhigh", "medium"],
        Effort::Max => &["max", "xhigh", "high"],
    };
    let supported = meta.map(|m| m.efforts.as_slice()).unwrap_or(&[]);
    let pick = prefs
        .iter()
        .find(|p| supported.iter().any(|s| s == *p))
        .copied()
        .unwrap_or(match effort {
            Effort::Max => "xhigh",
            _ => prefs[0],
        });
    Some(serde_json::json!({ "effort": pick }))
}

fn pricing(p: Option<&Value>) -> Option<Pricing> {
    let p = p?;
    let per_token = |k: &str| -> Option<f64> {
        let v = p.get(k)?;
        let n = match v {
            Value::String(s) => s.trim().parse::<f64>().ok()?,
            Value::Number(n) => n.as_f64()?,
            _ => return None,
        };
        (n >= 0.0 && n.is_finite()).then_some(n)
    };
    Some(Pricing {
        input_per_mtok: per_mtok(per_token("prompt")?),
        output_per_mtok: per_mtok(per_token("completion")?),
    })
}

/// Per-token USD to per-million, rounded to a millionth of a dollar (binary float noise).
fn per_mtok(per_token: f64) -> f64 {
    (per_token * 1e12).round() / 1e6
}

fn clamp_u32(n: u64) -> u32 {
    n.min(u32::MAX as u64) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> String {
        json!({"data": [
            {
                "id": "anthropic/claude-haiku-5.5",
                "name": "Anthropic: Claude Haiku 5.5",
                "context_length": 1000000,
                "architecture": {"input_modalities": ["text", "image"], "output_modalities": ["text"]},
                "pricing": {"prompt": "0.0000001", "completion": "0.0000005"},
                "top_provider": {"context_length": 1000000, "max_completion_tokens": 128000},
                "supported_parameters": ["max_tokens", "reasoning", "tools"],
                "reasoning": {"mandatory": false,
                              "supported_efforts": ["max", "xhigh", "high", "medium", "low"]}
            },
            {
                "id": "openrouter/auto",
                "name": "Auto Router",
                "context_length": 2000000,
                "pricing": {"prompt": "-1", "completion": "-1"},
                "supported_parameters": ["tools"]
            },
            {
                "id": "some/image-model",
                "architecture": {"output_modalities": ["image"]},
                "pricing": {"prompt": "0", "completion": "0"}
            },
            {
                "id": "deepseek/r1",
                "context_length": 163840,
                "pricing": {"prompt": "0.0000004", "completion": "0.000002"},
                "supported_parameters": ["reasoning", "include_reasoning"],
                "reasoning": {"mandatory": true}
            },
            {"name": "no id"}
        ]})
        .to_string()
    }

    #[test]
    fn catalog_entries_become_model_info() {
        let cat = parse_catalog(&sample()).unwrap();
        let ids: Vec<&str> = cat.iter().map(|c| c.info.id.as_str()).collect();
        assert_eq!(ids, ["anthropic/claude-haiku-5.5", "deepseek/r1", "openrouter/auto"]);

        let haiku = &cat[0].info;
        assert_eq!(haiku.display, "Anthropic: Claude Haiku 5.5");
        assert_eq!(haiku.context_window, Some(1_000_000));
        assert_eq!(haiku.max_output, Some(128_000));
        assert!(haiku.supports_tools && haiku.supports_reasoning && !haiku.local);
        assert!(haiku.supports_vision);
        assert_eq!(cat[0].vision, Some(true));
        assert_eq!(cat[1].vision, None, "a catalog without modalities does not say");
        assert!(!cat[1].info.supports_vision);
        let p = haiku.pricing.unwrap();
        assert!((p.input_per_mtok - 0.1).abs() < 1e-9 && (p.output_per_mtok - 0.5).abs() < 1e-9);
        assert_eq!(
            haiku.effort_levels,
            vec![Effort::Off, Effort::Low, Effort::Medium, Effort::High, Effort::Max]
        );

        let r1 = &cat[1].info;
        assert!(!r1.supports_tools);
        assert_eq!(r1.effort_levels, vec![Effort::Low, Effort::Medium, Effort::High], "mandatory: no Off");

        assert!(cat[2].info.pricing.is_none(), "variable pricing is unknown, not negative");
        assert!(cat[2].info.effort_levels.is_empty());
    }

    #[test]
    fn a_body_without_data_is_an_error() {
        assert!(parse_catalog("not json").is_err());
        assert!(parse_catalog(r#"{"models":[]}"#).is_err());
        assert!(parse_catalog(r#"{"data":[]}"#).unwrap().is_empty());
    }

    #[test]
    fn effort_maps_to_names_the_model_accepts() {
        let full = ReasoningMeta {
            supported: true,
            mandatory: false,
            efforts: vec!["xhigh".into(), "high".into(), "medium".into(), "low".into(), "none".into()],
        };
        assert_eq!(reasoning_field(Effort::Max, Some(&full)), Some(json!({"effort": "xhigh"})));
        assert_eq!(reasoning_field(Effort::Off, Some(&full)), Some(json!({"effort": "none"})));
        assert_eq!(reasoning_field(Effort::Low, Some(&full)), Some(json!({"effort": "low"})));

        let mandatory = ReasoningMeta { supported: true, mandatory: true, efforts: vec![] };
        assert_eq!(reasoning_field(Effort::Off, Some(&mandatory)), None);
        assert_eq!(reasoning_field(Effort::High, Some(&mandatory)), Some(json!({"effort": "high"})));

        let plain = ReasoningMeta { supported: true, mandatory: false, efforts: vec![] };
        assert_eq!(reasoning_field(Effort::Off, Some(&plain)), Some(json!({"enabled": false})));

        let none = ReasoningMeta::default();
        assert_eq!(reasoning_field(Effort::High, Some(&none)), None, "no reasoning, no field");
        assert_eq!(reasoning_field(Effort::Medium, None), Some(json!({"effort": "medium"})));
    }
}
