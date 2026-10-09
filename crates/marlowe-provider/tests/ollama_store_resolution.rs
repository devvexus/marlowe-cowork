//! **Every way reading Ollama's store can fail, refused at load time and by name.**
//!
//! # Why this is a whole test file rather than a `?`
//!
//! ADR-060 §7 is explicit that pointing `llama-server` at a blob Ollama downloaded is *"the same
//! class of dependency as parsing another program's cache directory"*. It works because it was
//! measured working, on this machine, on one day, and it can break on any Ollama update. What
//! bounds the damage is not that it cannot break — it is that when it does, the refusal names the
//! path it looked at and the command that fixes it, at startup, rather than becoming a wrong answer
//! four minutes into a run.
//!
//! **A store is built in a temp directory for each case**, so these assert on the resolver rather
//! than on whatever this machine happens to have pulled. `resolve_in` takes the root as a
//! parameter for exactly that reason; `resolve` is the one-line product wrapper around it.
//!
//! # And the positive control is not optional here
//!
//! Every assertion below is *"this input is refused"*. A `resolve_in` that returned an error for
//! **everything** passes all of them. `a_well_formed_store_resolves...` is what makes the rest
//! discriminating, and it is the first test in the file for that reason.

use std::path::{Path, PathBuf};

use marlowe_provider::LaunchPlan;
use marlowe_provider::ollama_store::{
    self, ResolveError, MODEL_MEDIA_TYPE, PARAMS_MEDIA_TYPE,
};

/// A scratch directory that cleans itself up. `std::env::temp_dir` plus the test's own name, so two
/// cases cannot collide and a failure leaves something a person can look at.
struct Store(PathBuf);

impl Store {
    fn new(case: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("marlowe-ollama-store-{case}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch store");
        Self(dir)
    }

    fn root(&self) -> &Path {
        &self.0
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
        std::fs::write(&path, contents).expect("write");
    }

    /// A manifest exactly as Ollama writes one: a config blob and one layer per media type.
    fn manifest(&self, model_path: &str, layers: &[(&str, &str)]) {
        let entries: Vec<String> = layers
            .iter()
            .map(|(media, digest)| {
                format!("{{\"mediaType\":\"{media}\",\"digest\":\"{digest}\",\"size\":1}}")
            })
            .collect();
        self.write(
            model_path,
            &format!(
                "{{\"schemaVersion\":2,\
                  \"config\":{{\"mediaType\":\"application/vnd.docker.container.image.v1+json\",\
                  \"digest\":\"sha256:be595b49\",\"size\":1}},\
                  \"layers\":[{}]}}",
                entries.join(",")
            ),
        );
    }

    fn blob(&self, digest: &str, contents: &str) {
        self.write(&format!("blobs/{}", digest.replace(':', "-")), contents);
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const QWEN_MANIFEST: &str = "manifests/registry.ollama.ai/library/qwen3.5/9b";
const MODEL_DIGEST: &str =
    "sha256:dec52a44569a2a25341c4e4d3fee25846eed4f6f0b936278e3a3c900bb99d37c";
const PARAMS_DIGEST: &str = "sha256:9371364b";

/// **THE CONTROL, and it is first because everything after it is a refusal.**
///
/// Without this, a `resolve_in` that returned `Err` unconditionally would pass every other test in
/// this file — the shape CLAUDE.md logs as a guard that is green on a broken build.
#[test]
fn a_well_formed_store_resolves_to_the_blob_and_the_published_sampler() {
    let store = Store::new("well-formed");
    store.manifest(
        QWEN_MANIFEST,
        &[
            (MODEL_MEDIA_TYPE, MODEL_DIGEST),
            ("application/vnd.ollama.image.license", "sha256:00000001"),
            (PARAMS_MEDIA_TYPE, PARAMS_DIGEST),
        ],
    );
    store.blob(MODEL_DIGEST, "GGUF");
    // Verbatim: `qwen3.5:9b`'s `.params` layer is 65 bytes and says exactly this.
    store.blob(
        PARAMS_DIGEST,
        "{\"presence_penalty\":1.5,\"temperature\":1,\"top_k\":20,\"top_p\":0.95}",
    );

    let r = ollama_store::resolve_in(store.root(), "qwen3.5:9b").expect("a well-formed store");
    assert_eq!(r.blob, store.root().join("blobs").join(MODEL_DIGEST.replace(':', "-")));
    let s = r.sampling.expect("the .params layer is there and it is not empty");
    assert_eq!(s.temperature, Some(1.0));
    assert_eq!(s.top_k, Some(20));
    assert_eq!(s.presence_penalty, Some(1.5));
}

/// A model with **no** `.params` layer is a reported state, not a failure. `marlowe-red:9b` is
/// like this, and refusing it would make an ordinary Ollama model unusable through this provider.
#[test]
fn a_model_with_no_params_layer_resolves_with_no_sampler_rather_than_refusing() {
    let store = Store::new("no-params");
    store.manifest(
        "manifests/registry.ollama.ai/library/marlowe-red/9b",
        &[(MODEL_MEDIA_TYPE, MODEL_DIGEST)],
    );
    store.blob(MODEL_DIGEST, "GGUF");

    let r = ollama_store::resolve_in(store.root(), "marlowe-red:9b").expect("this is legitimate");
    assert!(r.sampling.is_none(), "absent is not the same fact as unreadable");
}

#[test]
fn a_store_that_is_not_there_is_refused_by_path_rather_than_treated_as_empty() {
    let e = ollama_store::resolve_in(Path::new("no-such-store-anywhere"), "qwen3.5:9b")
        .expect_err("a missing store cannot resolve anything");
    assert!(matches!(e, ResolveError::NoStore { .. }), "{e}");
    assert!(e.remedy().contains("OLLAMA_MODELS"), "the remedy must be actionable: {}", e.remedy());
}

#[test]
fn a_model_that_was_never_pulled_is_refused_with_the_pull_command() {
    let store = Store::new("never-pulled");
    let e = ollama_store::resolve_in(store.root(), "qwen3.5:9b").expect_err("nothing is there");
    assert!(matches!(e, ResolveError::NoManifest { .. }), "{e}");
    assert!(e.to_string().contains(QWEN_MANIFEST.replace('/', "\\").as_str())
        || e.to_string().contains(QWEN_MANIFEST));
    assert!(
        e.remedy().contains("ollama pull qwen3.5:9b"),
        "`llamacpp` never downloads, so the remedy has to name what does: {}",
        e.remedy()
    );
}

/// **The version-drift case, which is the one ADR-060 §7 is actually about.**
///
/// `application/vnd.ollama.image.*` is Ollama's own namespace, versioned by them. A layout that
/// parses and means something else is the failure mode this class of dependency has, and the
/// refusal has to say what it DID find or the reader has nothing to compare against.
#[test]
fn a_manifest_with_no_model_layer_names_the_media_types_it_did_find() {
    let store = Store::new("no-model-layer");
    store.manifest(
        QWEN_MANIFEST,
        &[
            ("application/vnd.ollama.image.license", "sha256:00000001"),
            ("application/vnd.ollama.image.weights.v2", "sha256:00000002"),
        ],
    );
    let e = ollama_store::resolve_in(store.root(), "qwen3.5:9b").expect_err("no model layer");
    assert!(matches!(e, ResolveError::NoModelLayer { .. }), "{e}");
    assert!(
        e.to_string().contains("application/vnd.ollama.image.weights.v2"),
        "the refusal must show what the layout actually held: {e}"
    );
    assert!(e.remedy().contains("/provider ollama"), "{}", e.remedy());
}

#[test]
fn a_manifest_that_will_not_parse_is_refused_rather_than_half_read() {
    let store = Store::new("bad-manifest");
    store.write(QWEN_MANIFEST, "{ this is not json");
    let e = ollama_store::resolve_in(store.root(), "qwen3.5:9b").expect_err("unparseable");
    assert!(matches!(e, ResolveError::UnreadableManifest { .. }), "{e}");
}

#[test]
fn a_manifest_that_names_a_blob_the_store_does_not_hold_is_refused_before_any_launch() {
    // The manifest and the blob store disagreeing is a real state after an interrupted pull. Left
    // unchecked it becomes `llama-server` exiting with a file-not-found several seconds later,
    // attributed to whatever the user did last.
    let store = Store::new("missing-blob");
    store.manifest(QWEN_MANIFEST, &[(MODEL_MEDIA_TYPE, MODEL_DIGEST)]);
    let e = ollama_store::resolve_in(store.root(), "qwen3.5:9b").expect_err("no blob");
    assert!(matches!(e, ResolveError::MissingBlob { .. }), "{e}");
    assert!(e.to_string().contains("dec52a44"), "the refusal must name the digest: {e}");
}

/// **Absent is fine; unreadable is not**, and the pair is what makes that a rule rather than a
/// coincidence. A `.params` layer that is there and will not parse means the sampler this model
/// was published with is unknown, and continuing would substitute llama.cpp's own defaults —
/// temperature 0.8 where the model says 1.0, `presence_penalty` 0 where it says 1.5 — invisibly.
#[test]
fn a_params_layer_that_will_not_parse_refuses_rather_than_silently_using_llama_cpp_defaults() {
    let store = Store::new("bad-params");
    store.manifest(
        QWEN_MANIFEST,
        &[(MODEL_MEDIA_TYPE, MODEL_DIGEST), (PARAMS_MEDIA_TYPE, PARAMS_DIGEST)],
    );
    store.blob(MODEL_DIGEST, "GGUF");
    store.blob(PARAMS_DIGEST, "temperature = 1  # not json");

    let e = ollama_store::resolve_in(store.root(), "qwen3.5:9b").expect_err("unreadable params");
    assert!(matches!(e, ResolveError::UnreadableParams { .. }), "{e}");
    assert!(
        e.remedy().contains("changes the sampler silently"),
        "the remedy must say why this is not survivable: {}",
        e.remedy()
    );
}

/// **The launch command is what a person copies AND what `hybrid::start` spawns**, built from one
/// `LaunchPlan`. This asserts the printed half; the spawn reads `path_value()` off the same field.
///
/// # The two-variable rule, measured, because one variable was worth 10x
///
/// Same binary, same blob, same argv, back to back, differing only in environment
/// (`runs/llamacpp/product/DLL-CONTROL.md`):
///
/// | launch | VRAM taken | tok/s |
/// |---|---|---|
/// | `GGML_BACKEND_PATH` alone | **none** | **10.6** |
/// | `GGML_BACKEND_PATH` **+** the `PATH` prefix | **+5,523 MiB** | **107.4** |
///
/// `GGML_BACKEND_PATH` names which file to load; the Windows loader resolves *that file's own*
/// imports (`cudart`, `cublas`) against `PATH`. So the DLL is found, fails to initialise, and the
/// log reads `load_backend: failed to load …ggml-cuda.dll:` with an **empty reason**.
///
/// # What this test asserted before, and why that was green on the broken command
///
/// `assert!(cmd.contains("GGML_BACKEND_PATH"))` — a check that the *word* appeared, on a function
/// whose doc comment said it existed to prevent the CPU trap, and which reproduced it. The
/// assertions below name the **directories that must be on the prefix**, not the presence of a
/// variable name. A command containing `PATH=` and neither directory passes a word check and
/// produces a CPU server.
#[test]
fn the_launch_command_carries_the_blob_the_backend_dll_the_window_and_the_path_prefix() {
    let store = Store::new("launch-command");
    store.manifest(QWEN_MANIFEST, &[(MODEL_MEDIA_TYPE, MODEL_DIGEST)]);
    store.blob(MODEL_DIGEST, "GGUF");
    let r = ollama_store::resolve_in(store.root(), "qwen3.5:9b").expect("resolves");

    let binary = store.root().join("llama-server.exe");
    let dll = store.root().join("cuda_v12").join("ggml-cuda.dll");
    let plan = r.launch_plan(&binary, Some(&dll), 11435, 32_768);
    let cmd = plan.command_line();

    assert!(cmd.contains("GGML_BACKEND_PATH"), "{cmd}");
    assert!(cmd.contains("ggml-cuda.dll"), "the DLL must be named, not its directory: {cmd}");

    // ── The half that was missing, and it is worth 10x ────────────────────────────────
    assert!(cmd.contains("PATH="), "the backend DLL's own imports resolve against PATH: {cmd}");
    let lib = store.root().display().to_string().replace('\\', "/");
    assert!(
        cmd.contains(&format!("{lib}/cuda_v12")),
        "the CUDA directory must be ON the prefix, not merely named by GGML_BACKEND_PATH: {cmd}"
    );
    assert!(
        cmd.contains(&lib),
        "the binary's own directory holds ggml-base and must be on the prefix too: {cmd}"
    );
    assert!(cmd.contains("$PATH"), "the inherited PATH must be prefixed, never replaced: {cmd}");

    // **The two renderings share the DIRECTORY LIST, not a string**, because the spawn needs
    // `;`-joined native paths and the printed line is pasted into a POSIX shell. This is the
    // assertion that they cannot name different directories.
    let spawned = plan.path_value().to_string_lossy().to_lowercase();
    assert!(!plan.path_prefix.is_empty(), "a GPU launch must carry a prefix");
    for d in &plan.path_prefix {
        let d = d.to_string_lossy().to_lowercase();
        assert!(
            spawned.contains(d.as_str()),
            "a directory in the printed command is missing from the spawned PATH: {d} vs {spawned}"
        );
    }

    assert!(cmd.contains(MODEL_DIGEST.replace(':', "-").as_str()), "{cmd}");
    // `-c` is where the context window lives for this runtime, so the command has to carry the
    // daemon's own number or the two disagree the moment the server starts.
    assert!(cmd.contains("-c 32768"), "{cmd}");
    assert!(cmd.contains("-ngl 99"), "the whole point is GPU offload: {cmd}");
    // Pinned rather than inherited: `--jinja` is the DEFAULT on the bundled build, and a default
    // is a property of a build. `--reasoning-format deepseek` is what keeps reasoning on its own
    // channel instead of back inside `content`.
    assert!(cmd.contains("--jinja"), "{cmd}");
    assert!(cmd.contains("--reasoning-format deepseek"), "{cmd}");
    assert!(cmd.contains("--port 11435"), "{cmd}");

    // ── The control ───────────────────────────────────────────────────────────────────
    //
    // With no backend DLL the command must NOT quietly omit the variables and read as complete,
    // and it must NOT emit an empty `PATH=`, which would clear the inherited one and stop the
    // server starting at all.
    let no_gpu_plan = r.launch_plan(&binary, None, 11435, 32_768);
    let no_gpu = no_gpu_plan.command_line();
    assert!(!no_gpu.starts_with("GGML_BACKEND_PATH="), "{no_gpu}");
    // **The CUDA directory is gone and the binary's own directory remains**, which is correct and
    // is not what an earlier version of this test asserted. It said *"an empty prefix must emit no
    // PATH at all"* and then checked it on a plan whose prefix is NOT empty — conflating "no
    // backend DLL" with "no prefix". The run caught it, and the assertion was the thing that was
    // wrong.
    assert!(
        !no_gpu.contains("cuda_v12"),
        "with no backend DLL there is no CUDA directory to add: {no_gpu}"
    );
    assert_eq!(
        no_gpu_plan.path_prefix.len(),
        1,
        "the binary's own directory stays; only the backend's goes: {:?}",
        no_gpu_plan.path_prefix
    );
    // The property the muddled assertion was reaching for, stated correctly: an EMPTY prefix must
    // emit no `PATH=` at all. `PATH=":$PATH"` would put the current directory on the search path of
    // a process that loads DLLs by name, which is a real hazard and not a cosmetic one.
    let empty = LaunchPlan { path_prefix: Vec::new(), ..no_gpu_plan.clone() }.command_line();
    assert!(!empty.contains("PATH="), "an empty prefix must emit no PATH at all: {empty}");
    assert!(
        no_gpu.contains("FAILS OPEN to CPU"),
        "silence here is the CPU trap; the command must say what will happen: {no_gpu}"
    );
    // **The warning no longer promises `no usable GPU found`, and that is deliberate.** That
    // string was ABSENT from one failing launch on this machine and PRESENT in another, an hour
    // apart, both on the CPU. Telling a user to grep for a signal that has been observed to lie is
    // worse than telling them nothing; the health-200 / ~10 tok/s description is what actually
    // identifies the state.
    assert!(
        no_gpu.contains("~10 tok/s"),
        "the warning must describe the observable symptom rather than a log string: {no_gpu}"
    );
}

#[test]
fn a_missing_server_binary_is_refused_by_name_with_every_path_it_tried() {
    // The one product-path read of the machine in this file, and it is deliberately allowed to
    // succeed: on a box that HAS Ollama this returns the bundled binary, which is the correct
    // answer. What is asserted is the failure's shape, forced by pointing the override at nothing.
    let previous = std::env::var("MARLOWE_LLAMA_SERVER").ok();
    std::env::set_var("MARLOWE_LLAMA_SERVER", "no-such-llama-server-anywhere");
    let outcome = ollama_store::server_binary();
    match previous {
        Some(v) => std::env::set_var("MARLOWE_LLAMA_SERVER", v),
        None => std::env::remove_var("MARLOWE_LLAMA_SERVER"),
    }

    // The override does not win over a real bundled binary being absent -- it is tried FIRST and
    // then the bundled paths are, so on a machine with Ollama installed this legitimately
    // succeeds. Both outcomes are asserted, and neither is a silent pass.
    match outcome {
        Ok(found) => assert!(
            found.ends_with("llama-server.exe") || found.ends_with("llama-server"),
            "an override that does not exist must not become the answer: {}",
            found.display()
        ),
        Err(e) => {
            assert!(matches!(e, ResolveError::NoServerBinary { .. }), "{e}");
            assert!(
                e.to_string().contains("no-such-llama-server-anywhere"),
                "the refusal must name the override it was given: {e}"
            );
            assert!(e.remedy().contains("MARLOWE_LLAMA_SERVER"), "{}", e.remedy());
        }
    }
}
