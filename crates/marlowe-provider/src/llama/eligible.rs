//! What Marlowe's llama-server runs a model with, read from the GGUF on disk and, for a model in
//! Ollama's store, its manifest: Ollama is a source of model files, it never serves, and it need
//! not run. A model from the Hugging Face cache or a model folder is the file alone
//! ([`crate::sources`]): its draft is the `mtp-<name>.gguf` beside it, its projector the
//! `mmproj-*.gguf`, and it has no Modelfile to carry over.
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
//! GGUF are refused with the reason: "can't run: ...". A GGUF holding the tensor types only an MS
//! build writes (44..48, 200..215), or the `msffn.*` metadata of an MS file, needs ms-llama.cpp
//! ([`EngineModel::needs`]), and the installed ms-llama.cpp must run the file's generation
//! ([`EngineModel::refuses_on`]). Any other type Marlowe's table lacks may be one a newer llama.cpp
//! added: the server is asked to load it, and a failed load says so ([`EngineModel::unknown_types`]).

use std::path::{Path, PathBuf};

use marlowe_engines::Backend;
use serde_json::Value;

use crate::ollama::ModelMeta;
use crate::ollama_store::{manifest_relative_path, resolve_in, Sampling, MODEL_MEDIA_TYPE, PARAMS_MEDIA_TYPE};
use crate::sources::{Found, Source, Sources, FILE_PREFIX};

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
    /// The name shown: the Ollama name, or a model file's own.
    pub display: String,
    pub blob: PathBuf,
    pub draft: Option<Draft>,
    pub mmproj: Option<PathBuf>,
    pub defaults: ModelDefaults,
    /// Tensor type ids stock ggml (as Marlowe knows it) does not know (`ModelHeader::foreign_types`).
    pub foreign_types: Vec<u32>,
    /// The MS (Mad Shift) mark of the model file, or of its MTP head (`ModelHeader::ms`).
    pub ms: Option<marlowe_hw::MsInfo>,
    /// The model file's bytes.
    pub size: u64,
    /// What the catalog says about it: capabilities, window, family.
    pub meta: ModelMeta,
    /// Where the weights of the files go, from their headers; `None` when a header could not be
    /// analysed (the checks then count whole files).
    pub parts: Option<WeightParts>,
}

/// The weights of a model's files by where llama.cpp places them, bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WeightParts {
    /// On the GPU when every layer is: blocks, output layer, vision, the draft's and the projector.
    pub gpu: u64,
    /// The input layer (token and n-gram embedding tables): on the CPU whatever `-ngl` says.
    pub host: u64,
    /// The routed experts of each block (inside `gpu`), for `--n-cpu-moe`.
    pub experts: Vec<u64>,
}

impl WeightParts {
    /// Reads them from the model's header, its draft's and the projector's size.
    pub fn of(header: &marlowe_hw::ModelHeader, draft: Option<&Path>, mmproj: Option<&Path>) -> Option<WeightParts> {
        use marlowe_hw::arch::{ArchProfile, Role};
        let main = ArchProfile::analyze(header, Role::Main).ok()?;
        let mut p = WeightParts { gpu: main.weights.gpu() + main.weights.vision, host: main.weights.host, experts: main.weights.experts };
        if let Some(d) = draft {
            let d = ArchProfile::analyze(&marlowe_hw::gguf::read_header(d).ok()?, Role::Draft).ok()?;
            p.gpu += d.weights.gpu();
            p.host += d.weights.host;
        }
        p.gpu += mmproj.and_then(|m| std::fs::metadata(m).ok()).map_or(0, |m| m.len());
        Some(p)
    }

    /// On the GPU with the experts of the first `cpu_moe` blocks on the CPU.
    pub fn gpu_with(&self, cpu_moe: u32) -> u64 {
        self.gpu.saturating_sub(self.experts.iter().take(cpu_moe as usize).sum())
    }
}

/// The backend the engine runs: the selected one and the version installed. A developer's own
/// binary is `None` where this is taken: its kind is not second-guessed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chosen {
    pub backend: Backend,
    /// The installed tag (`ms-v1.0`); `None` when not known.
    pub tag: Option<String>,
}

/// Why the chosen backend cannot run a model, in the words shown to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub message: String,
    /// The backend that can run it, when switching would help.
    pub switch_to: Option<Backend>,
}

impl Refusal {
    /// The error a request gets: the reason, and where to switch when switching helps.
    pub fn engine_error(&self) -> String {
        match self.switch_to {
            Some(b) => format!("{} Select {b} in the engine window (click `engine` in the title bar).", self.message),
            None => self.message.clone(),
        }
    }
}

impl EngineModel {
    /// The backend the model needs, when only one can load it: an MS file, by its `msffn.*`
    /// metadata or the tensor types only an MS build writes ([`marlowe_hw::ms::only_ms_type`]).
    pub fn needs(&self) -> Option<Backend> {
        self.ms.is_some().then_some(Backend::MsLlamaCpp)
    }

    /// Tensor types that are neither upstream's nor MS's as far as Marlowe knows: a llama.cpp
    /// built before upstream added them cannot load the file, a newer one can.
    pub fn unknown_types(&self) -> Vec<u32> {
        self.foreign_types.iter().copied().filter(|id| !marlowe_hw::ms::only_ms_type(*id)).collect()
    }

    /// What the picker calls it: `MS Gen 1`, or `MS`.
    pub fn ms_label(&self) -> Option<String> {
        self.needs().map(|_| self.ms.as_ref().map_or_else(|| "MS".to_string(), marlowe_hw::MsInfo::label))
    }

    /// Why the backend `chosen` cannot load this model, or `None`. `None` for `chosen` means the
    /// binary's kind is unknown (a developer's `MARLOWE_LLAMA_SERVER`), which is not checked.
    pub fn refuses_on(&self, chosen: Option<&Chosen>) -> Option<Refusal> {
        let (need, have) = (self.needs()?, chosen?);
        let name = &self.display;
        if need != have.backend {
            return Some(Refusal { message: format!("{name} is an MS (Mad Shift) model: it runs on {need}."), switch_to: Some(need) });
        }
        // The right backend, but a version that no longer runs this generation of file.
        let (gen, tag) = (self.ms.as_ref()?.generation?, have.tag.as_deref()?);
        if marlowe_engines::ms::runs(tag, gen.number()) {
            return None;
        }
        let (runs, needs, cure) = match gen {
            marlowe_hw::MsGen::Gen1 => ("Gen 2", "ms-llama.cpp ms-v1.0", "Install ms-v1.0 (the engine window, `engine` in the title bar) or migrate the file."),
            marlowe_hw::MsGen::Gen2 => ("Gen 1", "a newer ms-llama.cpp", "Install a newer ms-llama.cpp in the engine window (`engine` in the title bar)."),
        };
        Some(Refusal { message: format!("{name} is an {gen} model: it needs {needs}, and the installed {tag} only runs {runs} files. {cure}"), switch_to: None })
    }
}

/// The speculative draft count Ollama passes (`--spec-draft-n-max 3`).
pub const SPEC_DRAFT_N: u32 = 3;

/// Reads `model` from wherever it is stored, or says why the engine can't run it (the error reads
/// after "<model> can't run: ").
pub fn resolve(sources: &Sources, model: &str) -> Result<EngineModel, String> {
    if model.starts_with(FILE_PREFIX) {
        resolve_file(sources, model)
    } else {
        resolve_ollama(sources.ollama_root(), model)
    }
}

/// A model file from the Hugging Face cache or a model folder: the file alone, its MTP head and
/// projector found beside it.
fn resolve_file(sources: &Sources, model: &str) -> Result<EngineModel, String> {
    let found = sources
        .find_file(model)
        .ok_or_else(|| format!("no model file is named that in {}. Add its folder under `[models] dirs` in settings.toml", sources.describe()))?;
    file_model(found)
}

/// [`resolve`] for a model a scan just found: the file is read from where the scan saw it, with
/// no second walk of every folder (a listing resolves each model it finds).
pub fn resolve_found(sources: &Sources, found: &Found) -> Result<EngineModel, String> {
    match found.source {
        Source::Ollama => resolve_ollama(sources.ollama_root(), &found.id),
        _ => file_model(found.clone()),
    }
}

fn file_model(found: Found) -> Result<EngineModel, String> {
    let model = found.id.clone();
    let header = marlowe_hw::gguf::read_header(&found.path).map_err(|e| format!("its model file {} did not read: {e}", found.path.display()))?;
    if header.a("pooling_type").is_some() {
        return Err("it is an embedding model, not a chat model".into());
    }
    let template = header.kv.get("tokenizer.chat_template").and_then(Value::as_str).filter(|t| !t.trim().is_empty()).map(str::to_string);
    let Some(template) = template else {
        return Err("the model file has no chat template, and llama.cpp renders with the model file's own".into());
    };
    let draft = mtp_sidecar(&found.path).map(|path| Draft { path, mtp: true });
    let mmproj = mmproj_sidecar(&found.path);
    let ms = header.ms().or_else(|| draft.as_ref().and_then(|d| marlowe_hw::gguf::read_header(&d.path).ok()?.ms()));
    let meta = meta_of(&header, &template, mmproj.is_some(), None, None);
    let parts = WeightParts::of(&header, draft.as_ref().map(|d| d.path.as_path()), mmproj.as_deref());
    Ok(EngineModel { model, display: found.display, blob: found.path, draft, mmproj, defaults: ModelDefaults::default(), foreign_types: header.foreign_types(), ms, size: found.size, meta, parts })
}

/// A model in Ollama's store at `root`: its manifest's layers on top of the file.
fn resolve_ollama(root: &Path, model: &str) -> Result<EngineModel, String> {
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
    let ms = header.ms().or_else(|| draft.as_ref().and_then(|d| marlowe_hw::gguf::read_header(&d.path).ok()?.ms()));
    let size = std::fs::metadata(&resolved.blob).map_or(0, |m| m.len());
    let parts = WeightParts::of(&header, draft.as_ref().map(|d| d.path.as_path()), mmproj.as_deref());
    Ok(EngineModel { model: model.to_string(), display: model.to_string(), blob: resolved.blob, draft, mmproj, defaults, foreign_types: header.foreign_types(), ms, size, meta, parts })
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
    sidecar(model, "mtp-")
}

/// The vision projector beside a model file: `mmproj-<base>.gguf` the same way, else the only
/// `mmproj*.gguf` of a folder that holds just this one model (`mmproj-F16.gguf`, as the Hugging
/// Face repos name it).
pub fn mmproj_sidecar(model: &Path) -> Option<PathBuf> {
    if let Some(p) = sidecar(model, "mmproj-") {
        return Some(p);
    }
    let dir = model.parent()?;
    let (mut projectors, mut models) = (Vec::new(), 0);
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_ascii_lowercase) else { continue };
        if name.starts_with("mmproj") && name.ends_with(".gguf") && path.is_file() {
            projectors.push(path);
        } else if crate::sources::is_model_file(&name) {
            models += 1;
        }
    }
    (projectors.len() == 1 && models == 1).then(|| projectors.remove(0))
}

fn sidecar(model: &Path, prefix: &str) -> Option<PathBuf> {
    let stem = model.file_stem()?.to_str()?.to_ascii_lowercase();
    let dirs = [model.parent(), model.parent().and_then(Path::parent)];
    let mut best: Option<(usize, PathBuf)> = None;
    for dir in dirs.into_iter().flatten() {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_ascii_lowercase) else { continue };
            let Some(base) = name.strip_prefix(prefix).and_then(|n| n.strip_suffix(".gguf")) else { continue };
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
        let m = resolve(&Sources::from(d.path()), "dusk:22b").unwrap();
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
        let m = resolve(&Sources::from(d.path()), "dawn:9b").unwrap();
        assert!(m.draft.as_ref().is_some_and(|dr| dr.mtp), "{:?}", m.draft);
        assert!(m.mmproj.is_some() && m.meta.has("vision"));
        assert_eq!(m.defaults.system.as_deref(), Some("You are Dawn."));
        assert_eq!(m.defaults.messages, vec![("user".into(), "hi".into()), ("assistant".into(), "hello".into())]);
        // A plain small draft model is not an MTP head.
        let small = gguf(Some(TEMPLATE));
        plain(d.path(), "dawn:spec", &[(DRAFT_MEDIA_TYPE, &small[..])]);
        assert!(resolve(&Sources::from(d.path()), "dawn:spec").unwrap().draft.is_some_and(|dr| !dr.mtp));
    }

    #[test]
    fn what_llama_cpp_cannot_run_is_refused_with_the_reason() {
        let d = crate::llama::testkit::TempDir::new();
        plain(d.path(), "lora:1", &[(ADAPTER_MEDIA_TYPE, b"lora")]);
        assert!(resolve(&Sources::from(d.path()), "lora:1").unwrap_err().contains("LoRA adapter"));
        let bare = gguf(None);
        store(d.path(), "go:1", json!({"model_format": "gguf"}), &[(MODEL_MEDIA_TYPE, &bare[..]), (TEMPLATE_MEDIA_TYPE, &b"<s>{{ .System }}{{ .Prompt }}"[..])]);
        assert!(resolve(&Sources::from(d.path()), "go:1").unwrap_err().contains("only chat template is an Ollama Go template"));
        store(d.path(), "ren:1", json!({"model_format": "gguf", "renderer": "qwen3.5"}), &[(MODEL_MEDIA_TYPE, &bare[..])]);
        assert!(resolve(&Sources::from(d.path()), "ren:1").unwrap_err().contains("renderer qwen3.5"));
        store(d.path(), "st:1", json!({"model_format": "safetensors"}), &[(MODEL_MEDIA_TYPE, &bare[..])]);
        assert!(resolve(&Sources::from(d.path()), "st:1").unwrap_err().contains("not GGUF"));
        assert!(resolve(&Sources::from(d.path()), "nope:1").unwrap_err().contains("ollama pull"));
        // A Go template beside the file's own template is no obstacle: the file's renders.
        let own = gguf(Some(TEMPLATE));
        store(d.path(), "both:1", json!({"model_format": "gguf", "renderer": "x"}), &[(MODEL_MEDIA_TYPE, &own[..]), (TEMPLATE_MEDIA_TYPE, &b"<s>{{ .Prompt }}"[..])]);
        assert!(resolve(&Sources::from(d.path()), "both:1").is_ok());
    }

    /// A GGUF the way ms-llama.cpp writes one: the `msffn.*` mark and MS tensor types.
    pub(crate) fn ms_gguf(types: &[u32], quant: &str) -> Vec<u8> {
        let tensors: Vec<(String, u32, Vec<u64>)> = types.iter().enumerate().map(|(i, t)| (format!("blk.{i}.ffn_up.weight"), *t, vec![256])).collect();
        let refs: Vec<(&str, u32, Vec<u64>)> = tensors.iter().map(|(n, t, s)| (n.as_str(), *t, s.clone())).collect();
        marlowe_hw::gguf::write_header(
            &[
                ("general.architecture", json!("qwen35")),
                ("tokenizer.chat_template", json!(TEMPLATE)),
                ("msffn.quant_name", json!(quant)),
                ("msffn.hadamard", json!(true)),
            ],
            &refs,
        )
    }

    fn chosen(backend: Backend, tag: &str) -> Chosen {
        Chosen { backend, tag: Some(tag.into()) }
    }

    #[test]
    fn an_ms_model_needs_ms_llama_cpp_by_its_metadata_and_never_by_its_name() {
        let d = crate::llama::testkit::TempDir::new();
        let ms = ms_gguf(&[45, 0], "MS2.7");
        store(d.path(), "plain-name:1", json!({"model_format": "gguf"}), &[(MODEL_MEDIA_TYPE, &ms[..])]);
        let m = resolve(&Sources::from(d.path()), "plain-name:1").unwrap();
        assert_eq!((m.needs(), m.ms_label().as_deref()), (Some(Backend::MsLlamaCpp), Some("MS Gen 1")));
        let r = m.refuses_on(Some(&chosen(Backend::LlamaCpp, "v1.0.0"))).unwrap();
        assert_eq!(r.message, "plain-name:1 is an MS (Mad Shift) model: it runs on ms-llama.cpp.");
        assert_eq!(r.switch_to, Some(Backend::MsLlamaCpp));
        assert!(r.engine_error().ends_with("Select ms-llama.cpp in the engine window (click `engine` in the title bar)."), "{}", r.engine_error());
        assert_eq!(m.refuses_on(Some(&chosen(Backend::MsLlamaCpp, "ms-v1.0"))), None);
        assert_eq!(m.refuses_on(None), None, "a developer's own build is not second-guessed");
        // MS in the name, nothing MS in the file: stock runs it.
        plain(d.path(), "ms-in-the-name:1", &[]);
        let plain_ms = resolve(&Sources::from(d.path()), "ms-in-the-name:1").unwrap();
        assert_eq!((plain_ms.needs(), plain_ms.ms_label()), (None, None));
        // A type Marlowe's table lacks that is not an MS one may be a newer llama.cpp's: it is not
        // proof of MS, nothing is refused or switched, and the server is asked.
        let bare = marlowe_hw::gguf::write_header(
            &[("general.architecture", json!("qwen35")), ("tokenizer.chat_template", json!(TEMPLATE))],
            &[("blk.0.ffn_up.weight", 100, vec![256])],
        );
        store(d.path(), "types-only:1", json!({"model_format": "gguf"}), &[(MODEL_MEDIA_TYPE, &bare[..])]);
        let t = resolve(&Sources::from(d.path()), "types-only:1").unwrap();
        assert_eq!((t.needs(), t.unknown_types()), (None, vec![100]));
        assert_eq!(t.refuses_on(Some(&chosen(Backend::LlamaCpp, "v1.0.0"))), None);
        // The types only an MS build writes do need the fork, keys or not.
        let only_ms = marlowe_hw::gguf::write_header(
            &[("general.architecture", json!("qwen35")), ("tokenizer.chat_template", json!(TEMPLATE))],
            &[("blk.0.ffn_up.weight", 46, vec![256])],
        );
        store(d.path(), "types-ms:1", json!({"model_format": "gguf"}), &[(MODEL_MEDIA_TYPE, &only_ms[..])]);
        let t = resolve(&Sources::from(d.path()), "types-ms:1").unwrap();
        assert_eq!((t.needs(), t.unknown_types()), (Some(Backend::MsLlamaCpp), vec![]));
    }

    #[test]
    fn a_newer_ms_llama_cpp_that_only_runs_gen_2_refuses_a_gen_1_file_in_plain_words() {
        let d = crate::llama::testkit::TempDir::new();
        let g1 = ms_gguf(&[47], "MS2.7");
        let g2 = ms_gguf(&[202], "MS4");
        store(d.path(), "old:ms", json!({"model_format": "gguf"}), &[(MODEL_MEDIA_TYPE, &g1[..])]);
        store(d.path(), "new:ms", json!({"model_format": "gguf"}), &[(MODEL_MEDIA_TYPE, &g2[..])]);
        let s = Sources::from(d.path());
        let (old, new) = (resolve(&s, "old:ms").unwrap(), resolve(&s, "new:ms").unwrap());
        assert_eq!(new.ms_label().as_deref(), Some("MS Gen 2"));
        let v2 = chosen(Backend::MsLlamaCpp, "ms-v2.0");
        let v1 = chosen(Backend::MsLlamaCpp, "ms-v1.0");
        let r = old.refuses_on(Some(&v2)).unwrap();
        assert_eq!(
            r.message,
            "old:ms is an MS Gen 1 model: it needs ms-llama.cpp ms-v1.0, and the installed ms-v2.0 only runs Gen 2 files. Install ms-v1.0 (the engine window, `engine` in the title bar) or migrate the file."
        );
        assert_eq!(r.switch_to, None);
        assert!(new.refuses_on(Some(&v1)).unwrap().message.contains("needs a newer ms-llama.cpp"));
        assert_eq!((old.refuses_on(Some(&v1)), new.refuses_on(Some(&v2))), (None, None));
        // A build whose tag says nothing about its generation is not second-guessed.
        assert_eq!(old.refuses_on(Some(&chosen(Backend::MsLlamaCpp, "b17"))), None);
    }

    #[test]
    fn an_mtp_head_that_is_ms_makes_a_plain_looking_model_need_ms_llama_cpp() {
        let d = crate::llama::testkit::TempDir::new();
        let head = marlowe_hw::gguf::write_header(
            &[("general.architecture", json!("qwen35")), ("qwen35.nextn_predict_layers", json!(1)), ("msffn.quant_name", json!("MS-MTP"))],
            &[("blk.0.w", 47, vec![256])],
        );
        plain(d.path(), "dawn:ms", &[(DRAFT_MEDIA_TYPE, &head[..])]);
        let m = resolve(&Sources::from(d.path()), "dawn:ms").unwrap();
        assert!(m.ms.as_ref().is_some_and(|ms| ms.generation == Some(marlowe_hw::MsGen::Gen1)));
        assert_eq!(m.needs(), Some(Backend::MsLlamaCpp));
    }

    fn folder(d: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
        let p = d.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn a_file_in_a_model_folder_runs_with_its_mtp_head_and_projector_beside_it() {
        let d = crate::llama::testkit::TempDir::new();
        let ms = ms_gguf(&[46], "MS2.7");
        let head = marlowe_hw::gguf::write_header(&[("general.architecture", json!("qwen35")), ("msffn.quant_name", json!("MS-MTP"))], &[("a", 47, vec![32])]);
        folder(d.path(), "models/Qwen3.8-27B-MS2.7.gguf", &ms);
        folder(d.path(), "models/mtp-Qwen3.8-27B-MS.gguf", &head);
        folder(d.path(), "models/mmproj-F16.gguf", b"proj");
        let s = Sources::new(d.path().join("none"), Vec::new(), vec![d.path().join("models")]);
        let m = resolve(&s, "gguf/Qwen3.8-27B-MS2.7").unwrap();
        assert_eq!(m.blob, d.path().join("models").join("Qwen3.8-27B-MS2.7.gguf"));
        assert!(m.draft.as_ref().is_some_and(|dr| dr.mtp && dr.path.ends_with("mtp-Qwen3.8-27B-MS.gguf")), "{:?}", m.draft);
        assert!(m.mmproj.as_ref().is_some_and(|p| p.ends_with("mmproj-F16.gguf")) && m.meta.has("vision"));
        assert_eq!((m.needs(), m.defaults, m.size), (Some(Backend::MsLlamaCpp), ModelDefaults::default(), ms.len() as u64));
        assert!(m.meta.has("tools") && m.meta.can_chat());
        // Not found, or a file that cannot chat, says why.
        assert!(resolve(&s, "gguf/Missing").unwrap_err().contains("Add its folder under `[models] dirs`"));
        let bare = gguf(None);
        folder(d.path(), "models/NoTemplate.gguf", &bare);
        assert!(resolve(&s, "gguf/NoTemplate").unwrap_err().contains("no chat template"));
        let embed = gguf_with(Some(TEMPLATE), &[("qwen35.pooling_type", json!(1))]);
        folder(d.path(), "models/Embed.gguf", &embed);
        assert!(resolve(&s, "gguf/Embed").unwrap_err().contains("embedding"));
    }

    #[test]
    fn a_projector_is_taken_only_when_it_is_the_folders_only_one_beside_its_only_model() {
        let d = crate::llama::testkit::TempDir::new();
        let a = folder(d.path(), "one/Model-Q4.gguf", b"m");
        let proj = folder(d.path(), "one/mmproj-F16.gguf", b"p");
        assert_eq!(mmproj_sidecar(&a), Some(proj));
        // Two models share a folder: which one a lone projector belongs to is not guessed.
        let b = folder(d.path(), "two/A-Q4.gguf", b"m");
        folder(d.path(), "two/B-Q4.gguf", b"m");
        folder(d.path(), "two/mmproj-F16.gguf", b"p");
        assert_eq!(mmproj_sidecar(&b), None);
        // By the name, it is.
        let named = folder(d.path(), "three/Dusk-27B-Q4.gguf", b"m");
        let np = folder(d.path(), "three/mmproj-Dusk-27B.gguf", b"p");
        folder(d.path(), "three/Other-Q4.gguf", b"m");
        assert_eq!(mmproj_sidecar(&named), Some(np));
    }

    #[test]
    fn a_hugging_face_file_resolves_by_its_stable_id() {
        let d = crate::llama::testkit::TempDir::new();
        let hub = d.path().join("hub");
        let file = crate::sources::tests::hub_file(&hub, "unsloth/Qwen3-8B-GGUF", "rev1", "Qwen3-8B-Q4_K_M.gguf", &gguf(Some(TEMPLATE)));
        let s = Sources::new(d.path().join("none"), vec![hub], Vec::new());
        let m = resolve(&s, "gguf/hf/unsloth/Qwen3-8B-GGUF/Qwen3-8B-Q4_K_M").unwrap();
        assert_eq!((m.blob.clone(), m.needs()), (file, None));
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
