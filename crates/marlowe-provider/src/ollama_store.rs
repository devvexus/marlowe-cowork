//! Reading Ollama's on-disk model store: Marlowe's llama.cpp engine runs the GGUF Ollama already
//! downloaded, and Ollama itself never serves a Marlowe model (it need not even run).
//!
//! `~/.ollama/models` (or `OLLAMA_MODELS`) is not a documented public interface: this is the same
//! class of dependency as parsing another program's cache directory, and an Ollama update can
//! change it. Every read here happens at load time and every failure is a distinct variant that
//! names the path it looked at and the remedy, rather than a wrong answer four minutes into a run.
//!
//! The `.params` layer is read because Ollama applies it to every request: `llama-server` pointed
//! at the raw blob would apply its own sampler defaults instead, silently. A model with no
//! `.params` layer is a reported state; one that will not parse refuses.

use std::path::{Path, PathBuf};

/// The layer whose digest names the GGUF. Ollama's own namespace, versioned by them.
pub const MODEL_MEDIA_TYPE: &str = "application/vnd.ollama.image.model";
/// The layer holding the sampling parameters Ollama applies on every request.
pub const PARAMS_MEDIA_TYPE: &str = "application/vnd.ollama.image.params";

/// What could not be read, specifically enough to fix.
///
/// **Every variant names a path or a value that was actually looked at.** A resolver that reported
/// "not found" without saying where it looked sends the reader to the wrong directory, and on a
/// layout that has changed under us that is the whole cost of the change.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    #[error("no Ollama model store at {root}: {detail}")]
    NoStore { root: String, detail: String },
    #[error("`{model}` is not a name Ollama's store can hold: {detail}")]
    UnreadableModelName { model: String, detail: String },
    #[error("no manifest for `{model}` at {path}")]
    NoManifest { model: String, path: String },
    #[error("the manifest for `{model}` at {path} did not parse: {detail}")]
    UnreadableManifest { model: String, path: String, detail: String },
    #[error(
        "the manifest for `{model}` at {path} has no `{MODEL_MEDIA_TYPE}` layer; it has [{present}]"
    )]
    NoModelLayer { model: String, path: String, present: String },
    #[error("`{model}`'s model layer names blob {digest}, which is not at {path}")]
    MissingBlob { model: String, digest: String, path: String },
    #[error("`{model}`'s `{PARAMS_MEDIA_TYPE}` layer at {path} did not parse: {detail}")]
    UnreadableParams { model: String, path: String, detail: String },
    #[error("no `llama-server` binary for {backend}. Looked in: {looked}")]
    NoServerBinary { backend: String, looked: String },
}

impl ResolveError {
    /// The line the user sees. **Names the remedy**, because a refusal a user cannot act on is a
    /// crash with better manners — the same rule every provider error follows.
    pub fn remedy(&self) -> String {
        match self {
            ResolveError::NoStore { root, .. } => format!(
                "{self}. Marlowe runs the GGUF Ollama already downloaded, so it needs Ollama's \
                 store. Install Ollama, or set OLLAMA_MODELS to the store's location (looked at {root})"
            ),
            ResolveError::UnreadableModelName { .. } => format!(
                "{self}. Names look like `qwen3.5:9b`, `library/qwen3.5:9b` or \
                 `hf.co/unsloth/model:TAG`"
            ),
            ResolveError::NoManifest { model, .. } => {
                format!("{self}. Run `ollama pull {model}` first; Marlowe reads Ollama's store and never downloads into it")
            }
            ResolveError::UnreadableManifest { .. } | ResolveError::NoModelLayer { .. } => format!(
                "{self}. This is Ollama's own on-disk layout and it is not a documented \
                 interface; an Ollama upgrade may have changed it"
            ),
            ResolveError::MissingBlob { model, .. } => format!(
                "{self}. The manifest and the blob store disagree. `ollama pull {model}` \
                 rewrites both"
            ),
            ResolveError::UnreadableParams { .. } => format!(
                "{self}. Ollama applies this layer to every request it serves; running the same \
                 model without it changes the sampler silently, so it is read or it refuses"
            ),
            ResolveError::NoServerBinary { backend, .. } => format!(
                "{self}. Marlowe runs every local model on a llama.cpp backend it installs itself: install {backend} in the \
                 engine window (click `engine` in the title bar, or the palette), or set MARLOWE_LLAMA_SERVER to a llama-server binary"
            ),
        }
    }
}

/// The sampling Ollama would have applied. **Every field optional**, because the layer is a
/// partial override and inventing a value for an absent key would be this module writing sampling
/// policy rather than reporting it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sampling {
    pub temperature: Option<f64>,
    pub top_k: Option<i64>,
    pub top_p: Option<f64>,
    pub min_p: Option<f64>,
    pub presence_penalty: Option<f64>,
    pub frequency_penalty: Option<f64>,
    pub repeat_penalty: Option<f64>,
    pub stop: Vec<String>,
}

impl Sampling {
    /// Read one `.params` blob. Unknown keys are **ignored, not refused**: Ollama's parameter set
    /// is larger than the sampler's and a `num_ctx` in there is not an error, it is a key this
    /// driver sets from `config.context_tokens` instead.
    pub fn from_params_json(value: &serde_json::Value) -> Option<Self> {
        let obj = value.as_object()?;
        let f = |k: &str| obj.get(k).and_then(serde_json::Value::as_f64);
        let i = |k: &str| obj.get(k).and_then(serde_json::Value::as_i64);
        let stop = match obj.get("stop") {
            Some(serde_json::Value::Array(a)) => {
                a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()
            }
            Some(serde_json::Value::String(s)) => vec![s.clone()],
            _ => Vec::new(),
        };
        Some(Self {
            temperature: f("temperature"),
            top_k: i("top_k"),
            top_p: f("top_p"),
            min_p: f("min_p"),
            presence_penalty: f("presence_penalty"),
            frequency_penalty: f("frequency_penalty"),
            repeat_penalty: f("repeat_penalty"),
            stop,
        })
    }

    /// Whether any field was set. An empty `.params` layer is the same fact as an absent one, and
    /// announcing "sampling from Ollama's layer" over nothing would be a claim with no content.
    pub fn is_empty(&self) -> bool {
        *self == Sampling::default()
    }

    /// The keys to merge into a `/v1/chat/completions` body. `llama-server` accepts `top_k`,
    /// `min_p` and `repeat_penalty` alongside the OpenAI-standard fields — **measured**, not
    /// assumed: ADR-060's tool-call probe sent exactly these to both servers and both answered.
    pub fn apply_to(&self, body: &mut serde_json::Value) {
        if let Some(v) = self.temperature {
            body["temperature"] = serde_json::json!(v);
        }
        if let Some(v) = self.top_k {
            body["top_k"] = serde_json::json!(v);
        }
        if let Some(v) = self.top_p {
            body["top_p"] = serde_json::json!(v);
        }
        if let Some(v) = self.min_p {
            body["min_p"] = serde_json::json!(v);
        }
        if let Some(v) = self.presence_penalty {
            body["presence_penalty"] = serde_json::json!(v);
        }
        if let Some(v) = self.frequency_penalty {
            body["frequency_penalty"] = serde_json::json!(v);
        }
        if let Some(v) = self.repeat_penalty {
            body["repeat_penalty"] = serde_json::json!(v);
        }
        if !self.stop.is_empty() {
            body["stop"] = serde_json::json!(self.stop);
        }
    }

    /// One line naming every value, for the startup announcement. ADR-029: announced, never
    /// inferred — and a sampler that differs from Ollama's is a difference the user must be able
    /// to read rather than deduce.
    pub fn disclosure(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(v) = self.temperature {
            parts.push(format!("temperature {v}"));
        }
        if let Some(v) = self.top_k {
            parts.push(format!("top_k {v}"));
        }
        if let Some(v) = self.top_p {
            parts.push(format!("top_p {v}"));
        }
        if let Some(v) = self.min_p {
            parts.push(format!("min_p {v}"));
        }
        if let Some(v) = self.presence_penalty {
            parts.push(format!("presence_penalty {v}"));
        }
        if let Some(v) = self.frequency_penalty {
            parts.push(format!("frequency_penalty {v}"));
        }
        if let Some(v) = self.repeat_penalty {
            parts.push(format!("repeat_penalty {v}"));
        }
        if !self.stop.is_empty() {
            parts.push(format!("stop {:?}", self.stop));
        }
        if parts.is_empty() {
            "no sampling parameters".to_string()
        } else {
            parts.join(", ")
        }
    }
}

/// What the store said about one model.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedModel {
    /// The name as asked for, unchanged.
    pub model: String,
    /// The GGUF `llama-server` is pointed at.
    pub blob: PathBuf,
    /// The manifest this was read from, so an announcement can say where it looked.
    pub manifest: PathBuf,
    /// `None` when the model has **no** `.params` layer. A reported state, not a failure —
    /// `qwen3.5:9b` is like this — and the announcement says so rather than implying Ollama's
    /// sampling was applied.
    pub sampling: Option<Sampling>,
}

impl ResolvedModel {
    /// The exact command that starts a server for this model, GPU offload and all.
    ///
    /// # This is a reader of the resolution, and it is why the resolution is not decoration
    ///
    /// In v1 the user launches the server, so the remedy IS the product of the resolver. A remedy
    /// that said *"start llama-server"* would be one nobody can act on: `GGML_BACKEND_PATH` **must
    /// name the DLL, not its directory**, and without it the bundled binary prints one warning and
    /// then serves happily **on CPU** at ~10 tok/s against ~107 — health 200, `/props` complete,
    /// tool calls still correct. ADR-060's probe found that, and it is ADR-044's embedder problem in
    /// a new subsystem: an unset variable is not an error, it is a slower run with a
    /// correct-looking log line.
    pub fn launch_command(
        &self,
        binary: &Path,
        backend_dll: Option<&Path>,
        port: u16,
        context_tokens: u32,
    ) -> String {
        self.launch_plan(binary, backend_dll, port, context_tokens).command_line()
    }

    /// The launch, **as values**: a binary, an argv and one environment variable.
    ///
    /// # This exists because ADR-060's hybrid makes Marlowe the thing that spawns it
    ///
    /// Until the hybrid was accepted the launch was only ever a *string* a person copied into a
    /// shell, so a string was the whole product. Now a supervisor spawns the server itself,
    /// and a second place assembling `-ngl 99 -np 1 --jinja --reasoning-format deepseek` would be
    /// two definitions of the flags that decide whether the model can call a tool at all — with
    /// the printed hint and the spawned process free to disagree, and only the hint ever read by
    /// a human.
    ///
    /// So the argv is the definition and [`LaunchPlan::command_line`] renders it. **The hint a
    /// user is shown is the process Marlowe would have started**, by construction.
    pub fn launch_plan(
        &self,
        binary: &Path,
        backend_dll: Option<&Path>,
        port: u16,
        context_tokens: u32,
    ) -> LaunchPlan {
        LaunchPlan {
            binary: binary.to_path_buf(),
            backend_dll: backend_dll.map(Path::to_path_buf),
            // **The binary's own directory and the backend's, in that order.** `$LIB` holds
            // `ggml-base.dll` and friends; `$LIB/cuda_v12` holds `ggml-cuda.dll` AND the CUDA
            // runtime DLLs it imports. Naming the backend file with GGML_BACKEND_PATH and leaving
            // its imports unresolvable is the 10x defect this list closes — see [`LaunchPlan`].
            //
            // **Deduped, and deliberately NOT filtered by `is_dir()`.** Both inputs are paths to
            // files that were found to exist -- `server_binary()` checks `is_file()` and
            // `backend_dll()` returns only a file it located -- so a parent that does not exist is
            // not a state this can be in. Touching the filesystem here would make the printed
            // remedy depend on the machine it was printed on, which is the one thing a launch
            // command a person copies must not do.
            path_prefix: {
                let mut dirs: Vec<PathBuf> = Vec::new();
                for d in [binary.parent(), backend_dll.and_then(Path::parent)].into_iter().flatten()
                {
                    let d = d.to_path_buf();
                    if !dirs.contains(&d) {
                        dirs.push(d);
                    }
                }
                dirs
            },
            args: vec![
                "-m".into(),
                self.blob.display().to_string(),
                // The window is a LAUNCH flag on this runtime, not a per-request field. It is the
                // daemon's `context_tokens` threaded here, so `Availability::ContextTooSmall`
                // cannot fire against a server this process started.
                "-c".into(),
                context_tokens.to_string(),
                "-ngl".into(),
                "99".into(),
                "-np".into(),
                "1".into(),
                // 168/168 tool calls parsed correctly with this on; it renders the GGUF's OWN
                // `tokenizer.chat_template`. Pinned rather than inherited, because a default is a
                // property of a build and this argv outlives the build it was written against.
                "--jinja".into(),
                // Keeps reasoning on `reasoning_content` and out of `content`. `deepseek-legacy`
                // would put `<think>` back into the answer text.
                "--reasoning-format".into(),
                "deepseek".into(),
                "--host".into(),
                "127.0.0.1".into(),
                "--port".into(),
                port.to_string(),
            ],
        }
    }
}

/// A `llama-server` launch as values rather than as a shell line. See [`ResolvedModel::launch_plan`].
///
/// # THE ENVIRONMENT IS PART OF THE LAUNCH, AND GETTING IT HALF-RIGHT COSTS 10x
///
/// Measured 2026-08-27, same binary, same blob, same argv, back to back, differing **only** in
/// environment (`runs/llamacpp/product/DLL-CONTROL.md`):
///
/// | launch | VRAM taken | tok/s |
/// |---|---|---|
/// | `GGML_BACKEND_PATH` alone | **none** | **10.6** |
/// | `GGML_BACKEND_PATH` **+** the `PATH` prefix | **+5,523 MiB** | **107.4** |
///
/// `GGML_BACKEND_PATH` says *which file* to load. It says nothing about where **that file's own
/// imports** — `cudart64_*.dll`, `cublas64_*.dll` — live, and the Windows loader resolves those
/// against `PATH`. So `ggml-cuda.dll` is found, fails to initialise, and the log reads
/// `E load_backend: failed to load …ggml-cuda.dll:` **with an empty reason string**.
///
/// This function's predecessor carried a doc comment saying it existed to prevent exactly that
/// failure, and it caused it — the sharpest form this project's failure family takes. A user who
/// copied the printed remedy verbatim got a CPU server.
///
/// # One directory list, two renderings, and why it cannot be one string
///
/// [`Self::path_prefix`] is the single definition. It is rendered twice because the two consumers
/// genuinely differ:
///
/// * **the spawn** — `Command::env("PATH", …)` reaches the Windows loader directly, so it wants
///   `;`-joined native paths, which is what [`Self::path_value`] produces via
///   `std::env::join_paths`;
/// * **the printed line** — pasted into Git Bash, where the verified command used `:` and forward
///   slashes and msys performed the conversion. [`Self::command_line`] renders that form.
///
/// They share the **directories**, not the string, and that is the strongest guarantee available:
/// a directory added for the spawn appears in the printed remedy in the same edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    pub binary: PathBuf,
    pub args: Vec<String>,
    /// `GGML_BACKEND_PATH`. **Names the DLL FILE, never its directory.**
    pub backend_dll: Option<PathBuf>,
    /// Directories prepended to `PATH` so the backend DLL's own imports resolve. See the type's
    /// header — without these the server runs on the CPU at a tenth of the speed, healthy, with
    /// `/props` complete and `supports_tools: true`.
    pub path_prefix: Vec<PathBuf>,
}

impl LaunchPlan {
    /// The `PATH` this launch needs, in the platform's own form, **prefixed to the inherited one**.
    ///
    /// Prefixed rather than replaced: the server is a normal Windows process and needs the system
    /// directories to start at all.
    pub fn path_value(&self) -> std::ffi::OsString {
        let inherited = std::env::var_os("PATH").unwrap_or_default();
        let mut dirs: Vec<PathBuf> = self.path_prefix.clone();
        dirs.extend(std::env::split_paths(&inherited));
        // `join_paths` uses the platform separator — `;` here, `:` elsewhere — so this is the one
        // place the separator is decided for the spawned process.
        std::env::join_paths(dirs).unwrap_or(inherited)
    }

    /// The same launch, rendered for a human to read or paste **into a POSIX shell**.
    pub fn command_line(&self) -> String {
        let mut out = String::new();
        if let Some(dll) = &self.backend_dll {
            out.push_str(&format!("GGML_BACKEND_PATH=\"{}\" \\\n    ", dll.display()));
        }
        if !self.path_prefix.is_empty() {
            // `:` and forward slashes, because this line is for a shell, and that is the form the
            // 107 tok/s control was actually run in. The spawn uses `path_value()` instead.
            let joined = self
                .path_prefix
                .iter()
                .map(|p| p.display().to_string().replace('\\', "/"))
                .collect::<Vec<_>>()
                .join(":");
            out.push_str(&format!("PATH=\"{joined}:$PATH\" \\\n    "));
        }
        out.push_str(&format!("\"{}\"", self.binary.display()));
        // Quoted only where it could contain a space or a separator — a blob path does, a `99`
        // does not, and a command line full of quoted integers is harder to read than one with
        // none.
        for a in &self.args {
            if a.contains(' ') || a.contains('\\') || a.contains('/') {
                out.push_str(&format!(" \"{a}\""));
            } else {
                out.push_str(&format!(" {a}"));
            }
        }
        if self.backend_dll.is_none() {
            // **Said HERE, at the one place the command is built, rather than at a call site.**
            // A caller that forgot it would emit a command that reads complete and lands the user
            // on CPU with health 200 and every tool call still correct.
            out.push_str(
                "\n# NO ggml-cuda backend was found beside the binary. Without it ggml FAILS OPEN \
                 to CPU: health 200, /props complete, supports_tools true, tool calls correct, \
                 ~10 tok/s against ~107.",
            );
        }
        out
    }
}

/// The store root. `OLLAMA_MODELS` when set — **Ollama's own documented variable**, so this reads
/// the other program's configuration rather than inventing a second one.
pub fn store_root() -> PathBuf {
    if let Ok(v) = std::env::var("OLLAMA_MODELS") {
        if !v.trim().is_empty() {
            return PathBuf::from(v);
        }
    }
    home().join(".ollama").join("models")
}

fn home() -> PathBuf {
    for var in ["USERPROFILE", "HOME"] {
        if let Ok(v) = std::env::var(var) {
            if !v.trim().is_empty() {
                return PathBuf::from(v);
            }
        }
    }
    PathBuf::from(".")
}

/// The `llama-server` of the selected backend (`marlowe-engines`), or every path that was tried.
///
/// `MARLOWE_LLAMA_SERVER` overrides it. That is an escape hatch for a user who built their own,
/// **not** a fallback: an unset variable takes the selected backend's install, and a set one that
/// does not exist is still a refusal naming the path. Ollama's own `llama-server` is never used.
pub fn server_binary() -> Result<PathBuf, ResolveError> {
    server_binary_in(std::env::var("MARLOWE_LLAMA_SERVER").ok().as_deref(), marlowe_engines::default_root().as_deref())
}

/// [`server_binary`] with the override and the engines root given.
pub fn server_binary_in(override_path: Option<&str>, engines_root: Option<&Path>) -> Result<PathBuf, ResolveError> {
    let mut looked: Vec<String> = Vec::new();
    if let Some(v) = override_path {
        let p = PathBuf::from(v);
        if p.is_file() {
            return Ok(p);
        }
        looked.push(format!("{} (MARLOWE_LLAMA_SERVER)", p.display()));
    }
    let Some(root) = engines_root else {
        looked.push("no install folder (LOCALAPPDATA is not set)".into());
        return Err(ResolveError::NoServerBinary { backend: marlowe_engines::DEFAULT.id().into(), looked: looked.join(", ") });
    };
    let backend = marlowe_engines::state::selected(root);
    match marlowe_engines::server(root, backend) {
        Ok(s) => Ok(s.binary),
        Err(e) => {
            looked.push(e.looked);
            Err(ResolveError::NoServerBinary { backend: backend.id().into(), looked: looked.join(", ") })
        }
    }
}

/// `qwen3.5:9b` → `manifests/registry.ollama.ai/library/qwen3.5/9b`, relative to the store root.
///
/// The tag defaults to `latest`, exactly as Ollama's CLI does. One, two or three path components
/// are accepted — `name`, `namespace/name`, `host/namespace/name` — because the store already
/// holds both `registry.ollama.ai/library/...` and `hf.co/...` on this machine.
pub fn manifest_relative_path(model: &str) -> Result<PathBuf, ResolveError> {
    let bad = |detail: &str| ResolveError::UnreadableModelName {
        model: model.to_string(),
        detail: detail.to_string(),
    };
    let model = model.trim();
    if model.is_empty() {
        return Err(bad("it is empty"));
    }
    // The tag is after the LAST colon, and only when that colon follows the last `/` — otherwise a
    // `host:port/ns/name` would lose its port to the tag.
    let last_slash = model.rfind('/');
    let (name, tag) = match model.rfind(':') {
        Some(i) if last_slash.is_none_or(|s| i > s) => (&model[..i], &model[i + 1..]),
        _ => (model, "latest"),
    };
    if tag.is_empty() {
        return Err(bad("the tag after `:` is empty"));
    }
    let parts: Vec<&str> = name.split('/').filter(|p| !p.is_empty()).collect();
    let (host, namespace, short) = match parts.as_slice() {
        [n] => ("registry.ollama.ai", "library", *n),
        [ns, n] => ("registry.ollama.ai", *ns, *n),
        [h, ns, n] => (*h, *ns, *n),
        [] => return Err(bad("it has no name")),
        _ => return Err(bad("it has more than three `/`-separated components")),
    };
    // **Refused rather than sanitised.** A component that walks out of the store is a path
    // traversal through a string that reaches this function from a config file and, one day, from
    // a `/model` typed at a picker. Guessing at a safe rewrite is how a traversal guard becomes a
    // comment; the name is either one Ollama could have written or it is refused by name.
    for c in [host, namespace, short, tag] {
        if c == "." || c == ".." || c.contains('\\') {
            return Err(bad("a component is `.`, `..`, or contains a backslash"));
        }
    }
    Ok(PathBuf::from("manifests").join(host).join(namespace).join(short).join(tag))
}

/// Resolve a model name against a store root. **Pure with respect to the environment** — the root
/// is a parameter — so a test can build a store and assert every failure without touching the
/// machine's real one.
pub fn resolve_in(root: &Path, model: &str) -> Result<ResolvedModel, ResolveError> {
    if !root.is_dir() {
        return Err(ResolveError::NoStore {
            root: root.display().to_string(),
            detail: "not a directory".into(),
        });
    }
    let manifest = root.join(manifest_relative_path(model)?);
    let text = std::fs::read_to_string(&manifest).map_err(|_| ResolveError::NoManifest {
        model: model.to_string(),
        path: manifest.display().to_string(),
    })?;
    let parsed: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| ResolveError::UnreadableManifest {
            model: model.to_string(),
            path: manifest.display().to_string(),
            detail: e.to_string(),
        })?;
    let layers = parsed.get("layers").and_then(|l| l.as_array()).ok_or_else(|| {
        ResolveError::UnreadableManifest {
            model: model.to_string(),
            path: manifest.display().to_string(),
            detail: "no `layers` array".into(),
        }
    })?;

    let digest_of = |media: &str| -> Option<String> {
        layers
            .iter()
            .find(|l| l.get("mediaType").and_then(|m| m.as_str()) == Some(media))
            .and_then(|l| l.get("digest").and_then(|d| d.as_str()))
            .map(str::to_string)
    };

    let Some(model_digest) = digest_of(MODEL_MEDIA_TYPE) else {
        let present: Vec<&str> = layers
            .iter()
            .filter_map(|l| l.get("mediaType").and_then(|m| m.as_str()))
            .collect();
        return Err(ResolveError::NoModelLayer {
            model: model.to_string(),
            path: manifest.display().to_string(),
            present: present.join(", "),
        });
    };

    let blob = blob_path(root, &model_digest);
    if !blob.is_file() {
        return Err(ResolveError::MissingBlob {
            model: model.to_string(),
            digest: model_digest,
            path: blob.display().to_string(),
        });
    }

    // **Absent is fine; unreadable is not.** A model with no `.params` layer is a reported state.
    // A layer that is there and will not parse means the sampler this model was published with is
    // unknown, and continuing would silently substitute llama.cpp's own defaults.
    let sampling = match digest_of(PARAMS_MEDIA_TYPE) {
        None => None,
        Some(d) => {
            let path = blob_path(root, &d);
            let raw = std::fs::read_to_string(&path).map_err(|e| ResolveError::UnreadableParams {
                model: model.to_string(),
                path: path.display().to_string(),
                detail: e.to_string(),
            })?;
            let value: serde_json::Value =
                serde_json::from_str(&raw).map_err(|e| ResolveError::UnreadableParams {
                    model: model.to_string(),
                    path: path.display().to_string(),
                    detail: e.to_string(),
                })?;
            Sampling::from_params_json(&value).filter(|s| !s.is_empty())
        }
    };

    Ok(ResolvedModel { model: model.to_string(), blob, manifest, sampling })
}

/// Every model the store at `root` holds, by the name Ollama gives it (`qwen3.5:9b`,
/// `ns/name:tag`, `hf.co/ns/name:tag`): every manifest with a model layer. A missing store is no
/// models; cloud stubs carry no weights and are left out.
pub fn list_in(root: &Path) -> Vec<String> {
    let base = root.join("manifests");
    let dirs = |p: &Path| -> Vec<(String, PathBuf)> {
        std::fs::read_dir(p)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| Some((e.file_name().to_str()?.to_string(), e.path())))
            .collect()
    };
    let mut out = Vec::new();
    for (host, h) in dirs(&base) {
        for (ns, n) in dirs(&h) {
            for (name, m) in dirs(&n) {
                for (tag, file) in dirs(&m) {
                    if !file.is_file() {
                        continue;
                    }
                    let full = match (host.as_str(), ns.as_str()) {
                        ("registry.ollama.ai", "library") => format!("{name}:{tag}"),
                        ("registry.ollama.ai", _) => format!("{ns}/{name}:{tag}"),
                        _ => format!("{host}/{ns}/{name}:{tag}"),
                    };
                    let has_weights = std::fs::read_to_string(&file)
                        .ok()
                        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                        .and_then(|v| v.get("layers").and_then(|l| l.as_array()).cloned())
                        .is_some_and(|ls| ls.iter().any(|l| l.get("mediaType").and_then(|m| m.as_str()) == Some(MODEL_MEDIA_TYPE)));
                    if has_weights {
                        out.push(full);
                    }
                }
            }
        }
    }
    out.sort();
    out
}

/// Resolve against the machine's own store. The product path; tests use [`resolve_in`].
pub fn resolve(model: &str) -> Result<ResolvedModel, ResolveError> {
    resolve_in(&store_root(), model)
}

fn blob_path(root: &Path, digest: &str) -> PathBuf {
    root.join("blobs").join(digest.replace(':', "-"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_name_resolves_to_the_library_namespace_and_the_latest_tag() {
        assert_eq!(
            manifest_relative_path("qwen3.5:9b").unwrap(),
            PathBuf::from("manifests/registry.ollama.ai/library/qwen3.5/9b")
        );
        assert_eq!(
            manifest_relative_path("qwen3.5").unwrap(),
            PathBuf::from("manifests/registry.ollama.ai/library/qwen3.5/latest")
        );
    }

    #[test]
    fn a_hugging_face_pull_keeps_its_own_host_and_namespace() {
        // The store on this machine holds `hf.co/unsloth/...` beside `registry.ollama.ai/...`, so
        // a resolver that assumed one host would work until the day somebody pulled from the other.
        assert_eq!(
            manifest_relative_path("hf.co/unsloth/Qwen3-8B-GGUF:Q4_K_M").unwrap(),
            PathBuf::from("manifests/hf.co/unsloth/Qwen3-8B-GGUF/Q4_K_M")
        );
    }

    #[test]
    fn a_registry_port_is_not_mistaken_for_a_tag() {
        // `rfind(':')` alone would take `5000/ns/name` as the tag and leave `localhost` as the
        // whole name. The colon only starts a tag when it comes after the last `/`.
        assert_eq!(
            manifest_relative_path("localhost:5000/ns/name").unwrap(),
            PathBuf::from("manifests/localhost:5000/ns/name/latest")
        );
    }

    #[test]
    fn a_name_that_would_walk_out_of_the_store_is_refused_rather_than_rewritten() {
        for bad in ["../../etc/passwd", "ns/../../x", "a/b\\c"] {
            let e = manifest_relative_path(bad).expect_err("this name must be refused");
            assert!(matches!(e, ResolveError::UnreadableModelName { .. }), "{bad}: {e}");
        }
    }

    #[test]
    fn params_are_read_as_ollama_writes_them_and_unknown_keys_are_ignored() {
        // Verbatim from `qwen3.5:9b`'s 65-byte `.params` blob, plus a key that is not a sampler
        // setting: `num_ctx` comes from `config.context_tokens`, and refusing it here would make
        // an ordinary Ollama model unusable through this provider.
        let v = serde_json::json!({
            "presence_penalty": 1.5, "temperature": 1, "top_k": 20, "top_p": 0.95, "num_ctx": 4096
        });
        let s = Sampling::from_params_json(&v).unwrap();
        assert_eq!(s.temperature, Some(1.0));
        assert_eq!(s.top_k, Some(20));
        assert_eq!(s.presence_penalty, Some(1.5));
        assert!(!s.is_empty());
    }

    #[test]
    fn an_empty_params_layer_reads_as_no_sampling_rather_than_as_a_claim() {
        // `is_empty` is what stops the announcement saying "sampling from Ollama's layer" over
        // nothing — a disclosure with no content is worse than the absent one it replaces.
        assert!(Sampling::from_params_json(&serde_json::json!({})).unwrap().is_empty());
    }

    #[test]
    fn the_store_lists_every_model_with_weights_by_the_name_ollama_gives_it() {
        let d = crate::llama::testkit::TempDir::new();
        let weights = [(MODEL_MEDIA_TYPE, "gguf")];
        crate::llama::eligible::tests::store(d.path(), "qwen3.5:9b", serde_json::json!({}), &weights);
        crate::llama::eligible::tests::store(d.path(), "devvexus/dusk:27b", serde_json::json!({}), &weights);
        crate::llama::eligible::tests::store(d.path(), "hf.co/unsloth/m:Q4", serde_json::json!({}), &weights);
        // A cloud stub has no model layer.
        crate::llama::eligible::tests::store(d.path(), "glm-5:cloud", serde_json::json!({}), &[("application/vnd.ollama.image.license", "x")]);
        assert_eq!(list_in(d.path()), vec!["devvexus/dusk:27b", "hf.co/unsloth/m:Q4", "qwen3.5:9b"]);
        assert!(list_in(&d.path().join("absent")).is_empty());
    }
}
