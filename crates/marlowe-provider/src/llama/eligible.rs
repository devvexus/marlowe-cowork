//! What Marlowe's llama-server runs a model from Ollama's store with, read from the manifest and
//! the GGUF on disk: Ollama is a source of model files, it never serves, and it need not run.
//!
//! Every model goes to the engine, and what Ollama would add to it is copied:
//!
//! | Ollama's | The engine's |
//! |---|---|
//! | a draft layer (an MTP head or a draft model) | `--spec-type draft-mtp` / `draft-simple`, `--spec-draft-model`, `--spec-draft-n-max 3` |
//! | an `mtp-<name>.gguf` beside a model file (folder models) | the same, by name prefix |
//! | a projector layer | `--mmproj`, and images on the request |
//! | `PARAMETER`s (the `.params` layer) | the request's sampler defaults; `num_ctx` caps the default window |
//! | `SYSTEM` | the system prompt when the request has none |
//! | `MESSAGE`s | put before the conversation, after the system prompt |
//!
//! The model file's own chat template renders the prompt (`--jinja`). A model that has none, so
//! that only an Ollama Go template or Ollama's own renderer could, an `ADAPTER`, and anything but
//! GGUF are refused with the reason: "can't run: ...". A GGUF holding tensor types stock ggml does
//! not know needs ms-llama.cpp ([`EngineModel::needs`]).

use std::path::{Path, PathBuf};

use marlowe_engines::Backend;
use serde_json::Value;

use crate::ollama::ModelMeta;
use crate::ollama_store::{manifest_relative_path, resolve_in, Sampling, MODEL_MEDIA_TYPE, PARAMS_MEDIA_TYPE};

/// The `TEMPLATE` layer.
const TEMPLATE_MEDIA_TYPE: &str = "application/vnd.ollama.image.template";
/// Licence text is carried along and never changes the prompt.
const LICENSE_MEDIA_TYPE: &str = "application/vnd.ollama.image.license";
/// A speculative-decoding draft: an MTP head or a small model.
pub const DRAFT_MEDIA_TYPE: &str = "application/vnd.ollama.image.draft";
/// A vision projector.
pub const PROJECTOR_MEDIA_TYPE: &str = "application/vnd.ollama.image.projector";
const SYSTEM_MEDIA_TYPE: &str = "application/vnd.ollama.image.system";
const MESSAGES_MEDIA_TYPE: &str = "application/vnd.ollama.image.messages";
const ADAPTER_MEDIA_TYPE: &str = "application/vnd.ollama.image.adapter";

/// What Ollama applies to every request from the Modelfile.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelDefaults {
    /// `PARAMETER` sampler values; the request's own values win.
    pub sampling: Option<Sampling>,
    /// `PARAMETER num_ctx`: the window Ollama would load the model with. Caps the default window.
    pub num_ctx: Option<u32>,
    /// `SYSTEM`: used only when the request carries no system prompt, as Ollama does.
    pub system: Option<String>,
    /// `MESSAGE`s as (role, content), before the conversation.
    pub messages: Vec<(String, String)>,
}

/// The draft speculative decoding runs beside the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub path: PathBuf,
    /// A multi-token-prediction head (`--spec-type draft-mtp`) rather than a separate small model.
    pub mtp: bool,
}

/// A model the engine can run, and with what.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineModel {
    pub model: String,
    pub blob: PathBuf,
    pub draft: Option<Draft>,
    pub mmproj: Option<PathBuf>,
    pub defaults: ModelDefaults,
    /// Tensor type ids stock ggml does not know (`ModelHeader::foreign_types`).
    pub foreign_types: Vec<u32>,
    /// What the catalog says about it: capabilities, window, family.
    pub meta: ModelMeta,
}

impl EngineModel {
    /// The backend the model needs, when only one can load it: a GGUF with types stock ggml does
    /// not know was written by ms-llama.cpp's MS quantizer.
    pub fn needs(&self) -> Option<Backend> {
        (!self.foreign_types.is_empty()).then_some(Backend::MsLlamaCpp)
    }

    /// Why `backend` cannot load this model, or `None`. `None` for `backend` means the binary's
    /// kind is unknown (a developer's `MARLOWE_LLAMA_SERVER`), which is not checked.
    pub fn refuses_on(&self, backend: Option<Backend>) -> Option<String> {
        let (need, have) = (self.needs()?, backend?);
        (need != have).then(|| {
            format!(
                "it uses tensor types stock llama.cpp does not know (ggml types {}): it needs {need}, and the selected backend is {have}. Select {need} in the engine window (click `engine` in the title bar)",
                self.foreign_types.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
            )
        })
    }
}

/// The speculative draft count Ollama passes (`--spec-draft-n-max 3`).
pub const SPEC_DRAFT_N: u32 = 3;

/// Reads `model` from the store at `root`, or says why the engine can't run it (the error reads
/// after "<model> can't run: ").
pub fn resolve(root: &Path, model: &str) -> Result<EngineModel, String> {
    let resolved = resolve_in(root, model).map_err(|e| e.remedy())?;
    let manifest = root.join(manifest_relative_path(model).map_err(|e| e.to_string())?);
    let text = std::fs::read_to_string(&manifest).map_err(|e| format!("{}: {e}", manifest.display()))?;
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", manifest.display()))?;
    let blob = |l: &Value| Some(root.join("blobs").join(l.get("digest")?.as_str()?.replace(':', "-")));
    let config: Value = v
        .pointer("/config/digest")
        .and_then(Value::as_str)
        .and_then(|d| std::fs::read_to_string(root.join("blobs").join(d.replace(':', "-"))).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    let name = |k: &str| config.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    if let Some(f) = name("model_format").filter(|f| f != "gguf") {
        return Err(format!("it is {f}, not GGUF, and llama.cpp loads GGUF only"));
    }
    let header = marlowe_hw::gguf::read_header(&resolved.blob).map_err(|e| format!("its model file {} did not read: {e}", resolved.blob.display()))?;
    let template = header.kv.get("tokenizer.chat_template").and_then(Value::as_str).filter(|t| !t.trim().is_empty()).map(str::to_string);

    let (mut draft, mut mmproj, mut go_template, mut system, mut messages) = (None, None, false, None, Vec::new());
    for l in v.get("layers").and_then(Value::as_array).into_iter().flatten() {
        let media = l.get("mediaType").and_then(Value::as_str).unwrap_or_default();
        let path = blob(l);
        let read = || path.as_deref().and_then(|p| std::fs::read_to_string(p).ok());
        match media {
            MODEL_MEDIA_TYPE | PARAMS_MEDIA_TYPE | LICENSE_MEDIA_TYPE => {}
            DRAFT_MEDIA_TYPE => draft = path.map(|p| Draft { mtp: is_mtp_head(&p), path: p }),
            PROJECTOR_MEDIA_TYPE => mmproj = path,
            // `{{ .Prompt }}` hands the prompt through untouched: it changes nothing.
            TEMPLATE_MEDIA_TYPE => go_template |= !read().is_some_and(|t| is_passthrough(&t)),
            SYSTEM_MEDIA_TYPE => system = read().filter(|s| !s.trim().is_empty()),
            MESSAGES_MEDIA_TYPE => messages = read().and_then(|t| serde_json::from_str::<Value>(&t).ok()).map(|v| modelfile_messages(&v)).unwrap_or_default(),
            ADAPTER_MEDIA_TYPE => return Err("it applies a LoRA adapter (Modelfile ADAPTER), which the engine does not".into()),
            _ => {}
        }
    }
    if template.is_none() {
        return Err(match (name("renderer"), go_template) {
            (Some(r), _) => format!("Ollama renders its prompt with its own code (renderer {r}) and the model file has no chat template of its own"),
            (None, true) => "its only chat template is an Ollama Go template; llama.cpp renders with the model file's own chat template and it has none".into(),
            (None, false) => "the model file has no chat template".into(),
        });
    }
    // A folder model's MTP head sits beside it as `mtp-<name>.gguf`.
    if draft.is_none() {
        draft = mtp_sidecar(&resolved.blob).map(|path| Draft { path, mtp: true });
    }
    let defaults = ModelDefaults {
        sampling: resolved.sampling.clone(),
        num_ctx: params_value(root, &v, "num_ctx").and_then(|n| n.as_u64()).map(|n| n.min(u64::from(u32::MAX)) as u32),
        system,
        messages,
    };
    let meta = meta_of(&header, template.as_deref().unwrap_or(""), mmproj.is_some(), name("model_family"), defaults.num_ctx);
    Ok(EngineModel { model: model.to_string(), blob: resolved.blob, draft, mmproj, defaults, foreign_types: header.foreign_types(), meta })
}

/// The catalog's view of a model file: chat or embedding, tools and thinking from what its chat
/// template handles, vision when there is a projector, the window from the header.
fn meta_of(header: &marlowe_hw::ModelHeader, template: &str, projector: bool, family: Option<String>, num_ctx: Option<u32>) -> ModelMeta {
    let mut caps: Vec<String> = Vec::new();
    caps.push(if header.a("pooling_type").is_some() { "embedding" } else { "completion" }.into());
    if template.contains("tools") {
        caps.push("tools".into());
    }
    if ["enable_thinking", "reasoning_content", "<think>", "thinking"].iter().any(|k| template.contains(k)) {
        caps.push("thinking".into());
    }
    if projector {
        caps.push("vision".into());
    }
    let ctx = header.a_u64("context_length").map(|c| c.min(u64::from(u32::MAX)) as u32);
    ModelMeta {
        capabilities: Some(caps),
        context: match (ctx, num_ctx) {
            (Some(c), Some(n)) => Some(c.min(n)),
            (c, n) => c.or(n),
        },
        family: family.or_else(|| header.arch().map(str::to_string)),
    }
}

/// One key of the `.params` layer.
fn params_value(root: &Path, manifest: &Value, key: &str) -> Option<Value> {
    let layer = manifest.get("layers")?.as_array()?.iter().find(|l| l.get("mediaType").and_then(Value::as_str) == Some(PARAMS_MEDIA_TYPE))?;
    let path = root.join("blobs").join(layer.get("digest")?.as_str()?.replace(':', "-"));
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    v.get(key).cloned()
}

/// Ollama's `MESSAGE` layer: `[{"role": ..., "content": ...}]`.
fn modelfile_messages(v: &Value) -> Vec<(String, String)> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| Some((m.get("role")?.as_str()?.to_string(), m.get("content")?.as_str()?.to_string())))
        .collect()
}

/// Whether a draft GGUF is a multi-token-prediction head (`<arch>.nextn_predict_layers`).
fn is_mtp_head(path: &Path) -> bool {
    marlowe_hw::gguf::read_header(path).is_ok_and(|h| h.a_u64("nextn_predict_layers").is_some_and(|n| n > 0))
}

/// The MTP head beside a model file: `mtp-<base>.gguf` in its folder or the one above, where
/// `<base>` begins the model's file name (`mtp-Qwen3.8-27B-MS.gguf` for every
/// `Qwen3.8-27B-MS2.7.gguf`, `...-MS4.gguf` of the family). The longest base wins.
pub fn mtp_sidecar(model: &Path) -> Option<PathBuf> {
    let stem = model.file_stem()?.to_str()?.to_ascii_lowercase();
    let dirs = [model.parent(), model.parent().and_then(Path::parent)];
    let mut best: Option<(usize, PathBuf)> = None;
    for dir in dirs.into_iter().flatten() {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_ascii_lowercase) else { continue };
            let Some(base) = name.strip_prefix("mtp-").and_then(|n| n.strip_suffix(".gguf")) else { continue };
            if base.is_empty() || !stem.starts_with(base) || path == model || !path.is_file() {
                continue;
            }
            if best.as_ref().is_none_or(|(n, _)| base.len() > *n) {
                best = Some((base.len(), path));
            }
        }
        if best.is_some() {
            break;
        }
    }
    best.map(|(_, p)| p)
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
        gguf_with(template, &[])
    }

    /// [`gguf`] with extra keys.
    pub(crate) fn gguf_with(template: Option<&str>, extra: &[(&str, Value)]) -> Vec<u8> {
        let mut kv = vec![("general.architecture", json!("qwen35")), ("qwen35.context_length", json!(262_144))];
        if let Some(t) = template {
            kv.push(("tokenizer.chat_template", json!(t)));
        }
        kv.extend(extra.iter().cloned());
        marlowe_hw::gguf::write_header(&kv, &[])
    }

    pub(crate) const TEMPLATE: &str = "{% for m in messages %}{{ m.content }}{% endfor %}{% if tools %}{{ tools }}{% endif %}{% if enable_thinking %}<think>{% endif %}";

    /// Writes a store holding `name` (`family/tag`) with the given config and layer media types.
    pub(crate) fn store<B: AsRef<[u8]>>(dir: &Path, name: &str, config: Value, layers: &[(&str, B)]) {
        let blobs = dir.join("blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        let tag = name.replace([':', '/'], "_");
        let put = |digest: &str, body: &[u8]| std::fs::write(blobs.join(format!("sha256-{digest}")), body).unwrap();
        put(&format!("cfg{tag}"), config.to_string().as_bytes());
        let mut ls: Vec<Value> = Vec::new();
        for (i, (media, body)) in layers.iter().enumerate() {
            let d = format!("{tag}l{i}");
            put(&d, body.as_ref());
            ls.push(json!({"mediaType": media, "digest": format!("sha256:{d}"), "size": body.as_ref().len()}));
        }
        let path = dir.join(manifest_relative_path(name).unwrap());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, json!({"config": {"digest": format!("sha256:cfg{tag}")}, "layers": ls}).to_string()).unwrap();
    }

    fn plain(d: &Path, name: &str, extra: &[(&str, &[u8])]) {
        let model = gguf(Some(TEMPLATE));
        let mut layers: Vec<(&str, &[u8])> = vec![(MODEL_MEDIA_TYPE, &model[..]), (PARAMS_MEDIA_TYPE, br#"{"temperature":1,"top_k":20,"num_ctx":65536,"stop":["<|im_end|>"]}"#)];
        layers.extend_from_slice(extra);
        store(d, name, json!({"model_format": "gguf", "model_family": "qwen35"}), &layers);
    }

    #[test]
    fn a_plain_gguf_runs_with_its_parameters_as_defaults_and_its_capabilities_from_the_template() {
        let d = crate::llama::testkit::TempDir::new();
        plain(d.path(), "dusk:22b", &[]);
        let m = resolve(d.path(), "dusk:22b").unwrap();
        let s = m.defaults.sampling.as_ref().unwrap();
        assert_eq!((s.temperature, s.top_k, s.stop.clone()), (Some(1.0), Some(20), vec!["<|im_end|>".to_string()]));
        assert_eq!(m.defaults.num_ctx, Some(65_536));
        assert_eq!(m.meta.context, Some(65_536), "num_ctx caps the window");
        assert!(m.meta.has("tools") && m.meta.has("thinking") && !m.meta.has("vision"), "{:?}", m.meta);
        assert_eq!((m.draft.clone(), m.mmproj.clone(), m.needs()), (None, None, None));
    }

    #[test]
    fn a_draft_layer_a_projector_a_system_prompt_and_messages_are_carried_over() {
        let d = crate::llama::testkit::TempDir::new();
        let head = gguf_with(None, &[("qwen35.nextn_predict_layers", json!(1))]);
        let msgs = json!([{"role": "user", "content": "hi"}, {"role": "assistant", "content": "hello"}]).to_string();
        plain(
            d.path(),
            "dawn:9b",
            &[
                (DRAFT_MEDIA_TYPE, &head[..]),
                (PROJECTOR_MEDIA_TYPE, b"mmproj"),
                (SYSTEM_MEDIA_TYPE, b"You are Dawn."),
                (MESSAGES_MEDIA_TYPE, msgs.as_bytes()),
                (TEMPLATE_MEDIA_TYPE, b"{{ .System }} {{ .Prompt }}"),
            ],
        );
        let m = resolve(d.path(), "dawn:9b").unwrap();
        assert!(m.draft.as_ref().is_some_and(|dr| dr.mtp), "{:?}", m.draft);
        assert!(m.mmproj.is_some() && m.meta.has("vision"));
        assert_eq!(m.defaults.system.as_deref(), Some("You are Dawn."));
        assert_eq!(m.defaults.messages, vec![("user".into(), "hi".into()), ("assistant".into(), "hello".into())]);
        // A plain small draft model is not an MTP head.
        let small = gguf(Some(TEMPLATE));
        plain(d.path(), "dawn:spec", &[(DRAFT_MEDIA_TYPE, &small[..])]);
        assert!(resolve(d.path(), "dawn:spec").unwrap().draft.is_some_and(|dr| !dr.mtp));
    }

    #[test]
    fn what_llama_cpp_cannot_run_is_refused_with_the_reason() {
        let d = crate::llama::testkit::TempDir::new();
        plain(d.path(), "lora:1", &[(ADAPTER_MEDIA_TYPE, b"lora")]);
        assert!(resolve(d.path(), "lora:1").unwrap_err().contains("LoRA adapter"));
        let bare = gguf(None);
        store(d.path(), "go:1", json!({"model_format": "gguf"}), &[(MODEL_MEDIA_TYPE, &bare[..]), (TEMPLATE_MEDIA_TYPE, &b"<s>{{ .System }}{{ .Prompt }}"[..])]);
        assert!(resolve(d.path(), "go:1").unwrap_err().contains("only chat template is an Ollama Go template"));
        store(d.path(), "ren:1", json!({"model_format": "gguf", "renderer": "qwen3.5"}), &[(MODEL_MEDIA_TYPE, &bare[..])]);
        assert!(resolve(d.path(), "ren:1").unwrap_err().contains("renderer qwen3.5"));
        store(d.path(), "st:1", json!({"model_format": "safetensors"}), &[(MODEL_MEDIA_TYPE, &bare[..])]);
        assert!(resolve(d.path(), "st:1").unwrap_err().contains("not GGUF"));
        assert!(resolve(d.path(), "nope:1").unwrap_err().contains("ollama pull"));
        // A Go template beside the file's own template is no obstacle: the file's renders.
        let own = gguf(Some(TEMPLATE));
        store(d.path(), "both:1", json!({"model_format": "gguf", "renderer": "x"}), &[(MODEL_MEDIA_TYPE, &own[..]), (TEMPLATE_MEDIA_TYPE, &b"<s>{{ .Prompt }}"[..])]);
        assert!(resolve(d.path(), "both:1").is_ok());
    }

    #[test]
    fn a_model_with_ms_tensor_types_needs_ms_llama_cpp_whatever_its_name() {
        let d = crate::llama::testkit::TempDir::new();
        let ms = marlowe_hw::gguf::write_header(
            &[("general.architecture", json!("qwen35")), ("tokenizer.chat_template", json!(TEMPLATE))],
            &[("blk.0.ffn_up.weight", 45, vec![256]), ("blk.0.attn_norm.weight", 0, vec![256])],
        );
        store(d.path(), "plain-name:1", json!({"model_format": "gguf"}), &[(MODEL_MEDIA_TYPE, &ms[..])]);
        let m = resolve(d.path(), "plain-name:1").unwrap();
        assert_eq!(m.needs(), Some(Backend::MsLlamaCpp));
        assert!(m.refuses_on(Some(Backend::LlamaCpp)).unwrap().contains("needs ms-llama.cpp"));
        assert_eq!(m.refuses_on(Some(Backend::MsLlamaCpp)), None);
        assert_eq!(m.refuses_on(None), None, "a developer's own build is not second-guessed");
        plain(d.path(), "ms-in-the-name:1", &[]);
        assert_eq!(resolve(d.path(), "ms-in-the-name:1").unwrap().needs(), None);
    }

    #[test]
    fn an_mtp_head_beside_a_model_file_is_paired_by_its_base_name() {
        let d = crate::llama::testkit::TempDir::new();
        let snap = d.path().join("snap");
        std::fs::create_dir_all(snap.join("MS2.7")).unwrap();
        let model = snap.join("MS2.7").join("Qwen3.8-27B-MS2.7.gguf");
        std::fs::write(&model, b"x").unwrap();
        std::fs::write(snap.join("mtp-Qwen3.8-27B-MS.gguf"), b"x").unwrap();
        std::fs::write(snap.join("mtp-Qwen3.8.gguf"), b"x").unwrap();
        std::fs::write(snap.join("mtp-Other.gguf"), b"x").unwrap();
        assert_eq!(mtp_sidecar(&model), Some(snap.join("mtp-Qwen3.8-27B-MS.gguf")), "the longest base that begins the name");
        let lone = d.path().join("lone.gguf");
        std::fs::write(&lone, b"x").unwrap();
        assert_eq!(mtp_sidecar(&lone), None);
    }
}
