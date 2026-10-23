//! Which Ollama models Marlowe's own llama-server may serve.
//!
//! A model qualifies when Ollama itself already hands the conversation to llama-server whole:
//! its manifest names no renderer, no parser and no template layer, so Ollama POSTs the messages
//! and tools to `/v1/chat/completions` and llama-server applies the GGUF's own chat template and
//! parses the tool calls. Marlowe then sends the same request and tool calling is unchanged.
//! Every other model (a renderer or parser of Ollama's own writing, a `TEMPLATE`, a draft model,
//! an image projector, an adapter, a baked-in system prompt or messages) is rendered by Ollama's
//! code, which Marlowe does not copy: those stay on Ollama.

use std::path::Path;

use serde_json::Value;

use crate::ollama_store::{manifest_relative_path, MODEL_MEDIA_TYPE, PARAMS_MEDIA_TYPE};

/// Licence text is carried along and never changes the prompt.
const LICENSE_MEDIA_TYPE: &str = "application/vnd.ollama.image.license";

/// What the manifest says about how Ollama would run the model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    pub renderer: Option<String>,
    pub parser: Option<String>,
    /// Layers other than the weights, the parameters and the licence, by short name
    /// (`template`, `draft`, `projector`, `system`, `adapter`, `messages`).
    pub extra_layers: Vec<String>,
    /// Anything but GGUF.
    pub format: Option<String>,
}

impl Facts {
    /// Why Ollama must keep serving this model, or `None` when it qualifies.
    pub fn why_not(&self) -> Option<String> {
        let mut why: Vec<String> = Vec::new();
        if let Some(r) = &self.renderer {
            why.push(format!("Ollama renders its prompt itself (renderer {r})"));
        }
        if let Some(p) = &self.parser {
            why.push(format!("Ollama parses its tool calls itself (parser {p})"));
        }
        if !self.extra_layers.is_empty() {
            why.push(format!("its manifest has a {} layer", self.extra_layers.join(", ")));
        }
        if let Some(f) = &self.format {
            why.push(format!("it is {f}, not GGUF"));
        }
        (!why.is_empty()).then(|| why.join("; "))
    }
}

/// The model's blob when it qualifies, with the manifest facts and `.params` read.
pub fn facts_in(root: &Path, model: &str) -> Result<Facts, String> {
    let manifest = root.join(manifest_relative_path(model).map_err(|e| e.to_string())?);
    let text = std::fs::read_to_string(&manifest).map_err(|e| format!("{}: {e}", manifest.display()))?;
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", manifest.display()))?;
    let layers = v.get("layers").and_then(Value::as_array).ok_or_else(|| format!("{} has no layers", manifest.display()))?;
    let mut facts = Facts::default();
    for l in layers {
        let media = l.get("mediaType").and_then(Value::as_str).unwrap_or_default();
        if [MODEL_MEDIA_TYPE, PARAMS_MEDIA_TYPE, LICENSE_MEDIA_TYPE].contains(&media) {
            continue;
        }
        facts.extra_layers.push(media.rsplit('.').next().unwrap_or(media).to_string());
    }
    // The config blob names the renderer and the parser. A manifest without one (a cloud stub)
    // has no weights either, and is refused below.
    let digest = v.pointer("/config/digest").and_then(Value::as_str);
    let config: Value = digest
        .and_then(|d| std::fs::read_to_string(root.join("blobs").join(d.replace(':', "-"))).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .ok_or_else(|| format!("{} names no readable config blob", manifest.display()))?;
    let name = |k: &str| config.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    facts.renderer = name("renderer");
    facts.parser = name("parser");
    facts.format = name("model_format").filter(|f| f != "gguf");
    Ok(facts)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// Writes a store holding `name` (`family/tag`) with the given config and layer media types.
    pub(crate) fn store(dir: &Path, name: &str, config: Value, layers: &[(&str, &str)]) {
        let blobs = dir.join("blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        let put = |digest: &str, body: &str| std::fs::write(blobs.join(format!("sha256-{digest}")), body).unwrap();
        put("cfg", &config.to_string());
        let mut ls: Vec<Value> = Vec::new();
        for (i, (media, body)) in layers.iter().enumerate() {
            let d = format!("l{i}");
            put(&d, body);
            ls.push(json!({"mediaType": media, "digest": format!("sha256:{d}"), "size": body.len()}));
        }
        let rel = manifest_relative_path(name).unwrap();
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, json!({"config": {"digest": "sha256:cfg"}, "layers": ls}).to_string()).unwrap();
    }

    #[test]
    fn a_plain_gguf_with_params_qualifies() {
        let d = crate::llama::testkit::TempDir::new();
        store(
            d.path(),
            "dusk:22b",
            json!({"model_format": "gguf", "model_family": "qwen35"}),
            &[(MODEL_MEDIA_TYPE, "gguf"), (PARAMS_MEDIA_TYPE, "{}")],
        );
        let f = facts_in(d.path(), "dusk:22b").unwrap();
        assert_eq!(f.why_not(), None);
    }

    #[test]
    fn a_renderer_a_parser_or_a_template_keeps_the_model_on_ollama() {
        let d = crate::llama::testkit::TempDir::new();
        store(
            d.path(),
            "dawn:9b",
            json!({"model_format": "gguf", "renderer": "qwen3.5", "parser": "qwen3.5"}),
            &[(MODEL_MEDIA_TYPE, "gguf"), ("application/vnd.ollama.image.template", "{{ .Prompt }}")],
        );
        let why = facts_in(d.path(), "dawn:9b").unwrap().why_not().unwrap();
        assert!(why.contains("renderer qwen3.5") && why.contains("parser qwen3.5") && why.contains("template"), "{why}");
        store(d.path(), "t:1", json!({"model_format": "gguf"}), &[(MODEL_MEDIA_TYPE, "gguf"), ("application/vnd.ollama.image.template", "x")]);
        assert!(facts_in(d.path(), "t:1").unwrap().why_not().unwrap().contains("template"));
        store(d.path(), "d:1", json!({"model_format": "gguf"}), &[(MODEL_MEDIA_TYPE, "gguf"), ("application/vnd.ollama.image.draft", "x")]);
        assert!(facts_in(d.path(), "d:1").unwrap().why_not().unwrap().contains("draft"));
    }

    #[test]
    fn a_missing_manifest_is_an_error_not_a_yes() {
        let d = crate::llama::testkit::TempDir::new();
        assert!(facts_in(d.path(), "nope:1").is_err());
    }
}
