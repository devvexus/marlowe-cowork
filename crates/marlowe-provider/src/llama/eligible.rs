//! Which Ollama models Marlowe's own llama-server may serve.
//!
//! A model qualifies when the GGUF carries a chat template of its own, so llama-server can apply
//! it (`--jinja`) and parse the tool calls itself, which is what Ollama does for a model with no
//! renderer or parser. A renderer or parser named in the manifest (Ollama's own port of a family's
//! template) is no obstacle: the engine does not use it, it uses the model file's template, and
//! the per-model switch puts the model back on Ollama. A `TEMPLATE` that is more than the
//! pass-through `{{ .Prompt }}`, a draft model, an image projector, an adapter, a baked-in system
//! prompt or messages are Ollama's own machinery, which Marlowe does not copy: those stay on Ollama.

use std::path::Path;

use serde_json::Value;

use crate::ollama_store::{manifest_relative_path, MODEL_MEDIA_TYPE, PARAMS_MEDIA_TYPE};

/// The `TEMPLATE` layer Ollama writes for a model that brings nothing of its own.
const TEMPLATE_MEDIA_TYPE: &str = "application/vnd.ollama.image.template";

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
    /// Whether the GGUF header carries `tokenizer.chat_template`; `None` when it could not be read.
    pub chat_template: Option<bool>,
}

impl Facts {
    /// Why Ollama must keep serving this model, or `None` when it qualifies.
    pub fn why_not(&self) -> Option<String> {
        let mut why: Vec<String> = Vec::new();
        // Ollama's renderer and parser are not used on the engine; the model file's template is.
        if self.chat_template != Some(true) {
            if let Some(r) = &self.renderer {
                why.push(format!("Ollama renders its prompt itself (renderer {r}) and the model file has no chat template of its own"));
            }
            if let Some(p) = &self.parser {
                why.push(format!("Ollama parses its tool calls itself (parser {p})"));
            }
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
    let blob = |l: &Value| {
        let d = l.get("digest").and_then(Value::as_str)?;
        Some(root.join("blobs").join(d.replace(':', "-")))
    };
    for l in layers {
        let media = l.get("mediaType").and_then(Value::as_str).unwrap_or_default();
        if media == MODEL_MEDIA_TYPE {
            facts.chat_template = blob(l)
                .and_then(|p| marlowe_hw::gguf::read_header(&p).ok())
                .map(|h| h.kv.get("tokenizer.chat_template").and_then(Value::as_str).is_some_and(|t| !t.trim().is_empty()));
        }
        if [MODEL_MEDIA_TYPE, PARAMS_MEDIA_TYPE, LICENSE_MEDIA_TYPE].contains(&media) {
            continue;
        }
        // `{{ .Prompt }}` hands the prompt through untouched: it changes nothing.
        if media == TEMPLATE_MEDIA_TYPE && blob(l).and_then(|p| std::fs::read_to_string(p).ok()).is_some_and(|t| is_passthrough(&t)) {
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

/// Whether an Ollama `TEMPLATE` only passes the prompt through.
fn is_passthrough(t: &str) -> bool {
    t.split_whitespace().collect::<String>().trim_matches('\u{feff}') == "{{.Prompt}}" || t.trim().is_empty()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// A GGUF header with (or without) a chat template, as the text a test store keeps in a blob.
    pub(crate) fn gguf(template: Option<&str>) -> Vec<u8> {
        let mut kv = vec![("general.architecture", json!("qwen35"))];
        if let Some(t) = template {
            kv.push(("tokenizer.chat_template", json!(t)));
        }
        marlowe_hw::gguf::write_header(&kv, &[])
    }

    /// Writes a store holding `name` (`family/tag`) with the given config and layer media types.
    pub(crate) fn store<B: AsRef<[u8]>>(dir: &Path, name: &str, config: Value, layers: &[(&str, B)]) {
        let blobs = dir.join("blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        let put = |digest: &str, body: &[u8]| std::fs::write(blobs.join(format!("sha256-{digest}")), body).unwrap();
        put("cfg", config.to_string().as_bytes());
        let mut ls: Vec<Value> = Vec::new();
        for (i, (media, body)) in layers.iter().enumerate() {
            let d = format!("l{i}");
            put(&d, body.as_ref());
            ls.push(json!({"mediaType": media, "digest": format!("sha256:{d}"), "size": body.as_ref().len()}));
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
    fn a_renderer_and_parser_are_no_obstacle_when_the_model_file_has_its_own_template() {
        let d = crate::llama::testkit::TempDir::new();
        let with = gguf(Some("{% for m in messages %}{{ m.content }}{% endfor %}"));
        store(
            d.path(),
            "dawn:9b",
            json!({"model_format": "gguf", "renderer": "qwen3.5", "parser": "qwen3.5"}),
            &[(MODEL_MEDIA_TYPE, &with[..]), (TEMPLATE_MEDIA_TYPE, &b"{{ .Prompt }}"[..]), (PARAMS_MEDIA_TYPE, &b"{}"[..])],
        );
        let f = facts_in(d.path(), "dawn:9b").unwrap();
        assert_eq!((f.renderer.as_deref(), f.chat_template), (Some("qwen3.5"), Some(true)));
        assert_eq!(f.why_not(), None, "the pass-through TEMPLATE layer changes nothing");
        // The same model without a template in the file has only Ollama's renderer to go on.
        let without = gguf(None);
        store(d.path(), "bare:1", json!({"model_format": "gguf", "renderer": "qwen3.5"}), &[(MODEL_MEDIA_TYPE, &without[..]), (PARAMS_MEDIA_TYPE, &b"{}"[..])]);
        assert!(facts_in(d.path(), "bare:1").unwrap().why_not().unwrap().contains("no chat template"));
        // A file that cannot be read says nothing about a template: the renderer keeps it on Ollama.
        store(d.path(), "odd:1", json!({"model_format": "gguf", "renderer": "x"}), &[(MODEL_MEDIA_TYPE, "gguf")]);
        assert!(facts_in(d.path(), "odd:1").unwrap().why_not().is_some());
    }

    #[test]
    fn a_renderer_a_parser_or_a_template_keeps_the_model_on_ollama() {
        let d = crate::llama::testkit::TempDir::new();
        store(
            d.path(),
            "dawn:9b",
            json!({"model_format": "gguf", "renderer": "qwen3.5", "parser": "qwen3.5"}),
            &[(MODEL_MEDIA_TYPE, "gguf"), ("application/vnd.ollama.image.template", "<s>{{ .System }}{{ .Prompt }}")],
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
