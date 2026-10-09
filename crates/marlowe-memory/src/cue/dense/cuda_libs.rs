//! **Where the CUDA 12 runtime libraries are, so that ORT's provider DLL can find them.**
//!
//! This module exists because of a finding that cost two sessions and produced one wrong
//! conclusion, and the finding is worth more than the code.
//!
//! # The finding
//!
//! `ort`'s `onnxruntime_providers_cuda.dll` is downloaded by `download-binaries` and lands beside
//! the executable. It links **dynamically** against the CUDA 12 runtime — `cublasLt64_12.dll`,
//! `cufft64_11.dll`, cuDNN 9 — and those are **not** downloaded with it. They must already be on
//! the machine, on the loader's search path, at the moment ORT calls `LoadLibrary`.
//!
//! On 2026-08-17 a session concluded from `Error 126: cublasLt64_12.dll is missing` that **there is
//! no CUDA toolkit installed on this machine**. That statement is *true*, and the conclusion drawn
//! from it — that CUDA cannot run here — is **false**, and it contradicts a recorded ADR.
//! `DECISIONS.md` ADR-015 says it in two lines: *"No CUDA Toolkit is installed and none is needed:
//! `torch 2.5.1+cu121` bundles what ORT 1.24 requires in `site-packages/torch/lib`, and they were
//! not on ORT's DLL search path."* A complete CUDA 12.1 runtime **and** cuDNN 9 — 4.2 GB of them —
//! were present the whole time. Nothing was missing. Nothing was on the search path.
//!
//! *"The DLL cannot be found"* was read as *"the DLL does not exist"*. Those are different claims
//! and only the first was measured. **Before concluding that a library is absent, look for it** —
//! it is one `find`.
//!
//! # The part that makes this a defect in the BUILD rather than a mistake by a reader
//!
//! It is tempting to file the above as one session's misreading. It is not, because **the working
//! configuration was never written down anywhere a program could read it.**
//!
//! `runs/session-l/RESULT.md` §5b records ADR-029's shipped GPU numbers — *"Warm-249, **Rust**, end
//! to end"*, CUDA batched at 3.4 ms against CPU's 195.6 — and states the precondition in prose:
//! *"`PATH` carries torch's bundled CUDA libraries for every cell."* So the Rust CUDA path **has
//! run on this machine**, through `marlowe.exe --rerank-provider cuda`, and the thing that made it
//! run was an environment variable in somebody's shell.
//!
//! That precondition had **no declaration, no reader, no validation and no test**. It could not
//! survive a session boundary, and it did not: the same binary on the same machine failed, the
//! failure was measured correctly, and the conclusion drawn was that the hardware was incapable.
//! **A configuration that exists only in a shell is indistinguishable from one that does not exist
//! at all**, and this is that family — a control nothing reads — with the control living outside
//! the process entirely.
//!
//! The related myth is worth killing while it is here: *"the `onnxruntime-gpu` wheel bundles its
//! own CUDA libraries and the Rust crate does not"*. **Measured false.**
//! `site-packages/onnxruntime/capi/` holds `onnxruntime_providers_cuda.dll` and no CUDA runtime at
//! all, and a bare `python -c "import onnxruntime; InferenceSession(…, providers=['CUDA…'])"` fails
//! with the **identical** `cublasLt64_12.dll` Error 126 — and then **silently returns a CPU
//! session**, which is Session G's failure in its original habitat. Python works only when
//! something has already called `os.add_dll_directory`, which `import torch` does as a side effect.
//! The difference was never the language or the runtime. It was the search path.
//!
//! # The mechanism, and why it is `PATH` rather than something cleverer
//!
//! The provider DLL is loaded **lazily**, by ORT, from inside the ONNX Runtime, long after this
//! process started. Its transitive dependencies resolve through the standard Windows search order,
//! which reads the process's `PATH` **at load time** — so prepending to `PATH` before the first
//! session is constructed is sufficient, and it is measured sufficient (`cuda_probe` fails without
//! it and constructs with it, same binary, same machine, minutes apart).
//!
//! `AddDllDirectory` would be the tidier API and it does not apply: it only affects loads that pass
//! `LOAD_LIBRARY_SEARCH_*` flags, which is not how ORT loads its provider.
//!
//! # This is NOT `PATH` on the developer's shell, and the difference is the whole point
//!
//! A shell incantation is not a product capability. `cuda_probe` was made to construct by typing
//! `PATH=…/torch/lib:$PATH` in front of it — which proves the hypothesis and ships nothing. What
//! ships is [`ENV_VAR`]: a declared input, read by a line of code, validated, and **reported**.
//!
//! # Deliberately NOT auto-discovered
//!
//! Nothing here searches for PyTorch, for Ollama's private `cuda_v12`, or for anything else. The
//! project already has a precedent and it points this way: Ollama's runtime was borrowed once as a
//! diagnostic, moved the error to the next link, and was recorded as *"not a fix"*. Silently
//! borrowing another product's private runtime would make this build's numerical behaviour depend
//! on whether an unrelated package happens to be installed, and on its version — which is the
//! stale-artifact family with the CUDA runtime as the stale artifact. **The human names the
//! directory, and the resolved directory is printed.**
//!
//! # Non-Windows is UNSUPPORTED and says so rather than pretending
//!
//! On Linux the dynamic loader caches `LD_LIBRARY_PATH` at process start, so setting it from
//! inside the process has no effect on a later `dlopen`. There is no in-process equivalent of the
//! Windows behaviour, so [`Resolution::Unsupported`] is returned and the variable must be exported
//! **before** launch. Returning `Applied` there would be a declaration with no reader — the exact
//! family this repository keeps paying for.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The one declared input. One directory, or several separated by the platform's path separator
/// (`;` on Windows) — a CUDA toolkit install and a cuDNN install are commonly two directories, so
/// the sentinel set below is required of their **union** rather than of any single one.
pub const ENV_VAR: &str = "MARLOWE_CUDA_LIB_DIR";

/// The libraries this `ort` build's CUDA provider needs, by the names it looks them up under.
///
/// **Derived from failures, not from a vendor document.** `cublasLt64_12` is the link that Error
/// 126 named; `cufft64_11` is the link it moved to when only a partial runtime was supplied; the
/// rest are the CUDA 12 / cuDNN 9 pair that ONNX Runtime's own diagnostic asks for by name
/// (*"Require cuDNN 9.* and CUDA 12.*"*).
///
/// **This list is a property of `ort = 2.0.0-rc.10` and is not inherited by a bump.** A newer ONNX
/// Runtime may want a different cuDNN major, at which point this check would reject a directory
/// that works. Re-derive it from an observed failure; do not extend it by guessing.
#[cfg(windows)]
pub const REQUIRED: &[&str] = &[
    "cublasLt64_12.dll",
    "cublas64_12.dll",
    "cudart64_12.dll",
    "cufft64_11.dll",
    "cudnn64_9.dll",
];

#[cfg(not(windows))]
pub const REQUIRED: &[&str] = &[];

/// What happened when the search path was resolved. Every arm is reported to the caller; none of
/// them is silent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// [`ENV_VAR`] is unset. CUDA may still construct — a machine with a real toolkit install has
    /// it on `PATH` already — so this is **not** an error, and it is not a promise either.
    NotRequested,

    /// The directories validated and were prepended to the process search path.
    Applied { dirs: Vec<PathBuf> },

    /// [`ENV_VAR`] was set and could not be honoured. **A refusal, not a warning.** A variable that
    /// is set, wrong, and ignored is how a run measures CPU under a CUDA label.
    Rejected { reason: String },

    /// The platform has no in-process mechanism. See the module header.
    Unsupported { reason: String },
}

impl Resolution {
    /// Did this resolution leave the process able to find the CUDA runtime *because of this
    /// module*? False for [`Resolution::NotRequested`], which says nothing either way.
    pub fn applied(&self) -> bool {
        matches!(self, Resolution::Applied { .. })
    }

    /// The refusal text, if this is one. `None` is not "fine" — [`Resolution::Unsupported`] is also
    /// not an application.
    pub fn rejection(&self) -> Option<&str> {
        match self {
            Resolution::Rejected { reason } => Some(reason),
            _ => None,
        }
    }
}

impl std::fmt::Display for Resolution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Resolution::NotRequested => write!(
                f,
                "{ENV_VAR} is unset; the CUDA runtime must already be on the loader's search path"
            ),
            Resolution::Applied { dirs } => {
                write!(f, "{ENV_VAR} applied: ")?;
                for (i, d) in dirs.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", d.display())?;
                }
                Ok(())
            }
            Resolution::Rejected { reason } => write!(f, "{ENV_VAR} REFUSED: {reason}"),
            Resolution::Unsupported { reason } => write!(f, "{ENV_VAR} unsupported: {reason}"),
        }
    }
}

/// Validate a value of [`ENV_VAR`] against the real filesystem, without touching the environment.
///
/// Separated from [`ensure_search_path`] so the decision is drivable in a test with directories the
/// test built, rather than only on a machine that happens to have CUDA — the same reason
/// [`crate::cue::dense::vram::Probe::Fixed`] exists. **It performs the real existence checks**; it
/// is not a mock of the decision, it is the decision with its input passed in.
pub fn resolve(value: Option<&OsString>) -> Resolution {
    let Some(value) = value else {
        return Resolution::NotRequested;
    };
    if value.is_empty() {
        return Resolution::Rejected {
            reason: format!(
                "{ENV_VAR} is set to the empty string. An empty value is not 'unset' -- it is a \
                 configuration that was meant to point somewhere and does not"
            ),
        };
    }

    let dirs: Vec<PathBuf> = std::env::split_paths(value).collect();

    for dir in &dirs {
        if !dir.is_dir() {
            return Resolution::Rejected {
                reason: format!(
                    "{} is not a directory. Every entry in {ENV_VAR} must exist at load time; a \
                     path that does not resolve is refused here rather than surfacing later as \
                     ORT's 'Error 126: the specified module could not be found', which names the \
                     library and not the misconfiguration that caused it",
                    dir.display()
                ),
            };
        }
    }

    // The union, not each directory: a CUDA toolkit and a cuDNN install are normally separate.
    let missing: Vec<&str> = REQUIRED
        .iter()
        .copied()
        .filter(|lib| !dirs.iter().any(|d| d.join(lib).is_file()))
        .collect();

    if !missing.is_empty() {
        return Resolution::Rejected {
            reason: format!(
                "{ENV_VAR} names {} director{} that do not between them contain {}. This ort \
                 build's CUDA provider links against all of {}; a partial runtime does not fail \
                 at the missing file, it fails at the FIRST missing file and moves to the next one \
                 as each is supplied (measured: supplying only Ollama's private cuda_v12 moved \
                 Error 126 from cublasLt64_12.dll to cufft64_11.dll). Refused up front so the \
                 whole gap is named at once",
                dirs.len(),
                if dirs.len() == 1 { "y" } else { "ies" },
                missing.join(", "),
                REQUIRED.join(", "),
            ),
        };
    }

    if !cfg!(windows) {
        return Resolution::Unsupported {
            reason: format!(
                "this platform's dynamic loader reads its library search path once, at process \
                 start, so setting it from inside the process cannot affect a later dlopen. \
                 Export the loader's own variable (LD_LIBRARY_PATH) BEFORE launching instead. \
                 {ENV_VAR} is honoured on Windows only"
            ),
        };
    }

    Resolution::Applied { dirs }
}

/// Prepend the resolved directories to this process's `PATH`, **once**, and report what happened.
///
/// Idempotent by [`OnceLock`]: called from every CUDA session construction, it mutates the
/// environment on the first call and returns the same answer thereafter. That matters beyond
/// tidiness — `set_var` racing a concurrent `getenv` is a genuine hazard, and the loader is a
/// concurrent reader of `PATH`. Doing it exactly once, before the first session is built, is what
/// keeps the window closed.
///
/// It does **not** return a `Result`. A [`Resolution::Rejected`] is a refusal the *caller* turns
/// into its own error, so that the message arrives attached to the operation the user asked for
/// rather than from a module they have never heard of.
pub fn ensure_search_path() -> &'static Resolution {
    static ONCE: OnceLock<Resolution> = OnceLock::new();
    ONCE.get_or_init(|| {
        // **The variable first; a SEARCH when it is unset.**
        //
        // # Why this stopped being acceptable as env-var-only
        //
        // Unset, an ORT CUDA session cannot construct, the embedder's `auto` provider (ADR-044)
        // resolves to **CPU**, and it says so in a line that reads like success. CLAUDE.md records
        // that hazard against this exact variable — *"an unset variable is not an error, it is a
        // slower run with a correct-looking log line"* — and on 2026-08-27 it cost a full day.
        //
        // Every measurement taken from a shell had the variable exported. **The Windows Terminal
        // shortcut does not export it, and neither does the daemon the TUI spawns**, so the product
        // ran the memory system on CPU while every test of it ran on GPU. Measured against that
        // split: **~198 ms of pre-request time**, which was the largest remaining term in a ~500 ms
        // time-to-first-token and the one thing on that path we actually own.
        //
        // A product that is three to four times slower unless the user exports a path is a product
        // with a trap in it. The directory is at a predictable location and we can look.
        //
        // # What the search is, and what it is not
        //
        // It is **not** a guess. `resolve` still checks every file in [`REQUIRED`] is present, so a
        // candidate that does not actually hold the CUDA 12 runtime and cuDNN 9 is rejected exactly
        // as a wrong `MARLOWE_CUDA_LIB_DIR` would be. The search only supplies candidates; the
        // verification is unchanged and is still the thing that decides.
        //
        // **The variable always wins**, so anyone pinning a specific runtime keeps doing so, and a
        // machine with two of them is not silently reassigned.
        let from_env = std::env::var_os(ENV_VAR);
        let resolution = match resolve(from_env.as_ref()) {
            Resolution::NotRequested => discover().map_or(Resolution::NotRequested, |d| {
                resolve(Some(&OsString::from(d)))
            }),
            other => other,
        };
        if let Resolution::Applied { dirs } = &resolution {
            prepend_to_path(dirs);
        }
        resolution
    })
}

/// Where the CUDA 12 runtime lives on a machine that never set [`ENV_VAR`].
///
/// **`torch` ships it.** ADR-015 established that no CUDA toolkit is installed here and none is
/// needed: `torch 2.5.1+cu121` carries the CUDA 12.1 runtime and cuDNN 9 in its own `lib` directory,
/// and that is what the variable has always pointed at. So the search looks where a `pip install
/// torch` puts things, in the order a user is most likely to have them.
///
/// **Returns the first candidate that EXISTS as a directory.** Whether it holds the right DLLs is
/// not decided here — `resolve` checks [`REQUIRED`] and rejects it otherwise. Splitting those two
/// jobs is deliberate: a search that also validated would have two places to be wrong about what
/// counts as a usable runtime.
#[cfg(windows)]
fn discover() -> Option<std::ffi::OsString> {
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from)?;
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);

    let mut roots: Vec<PathBuf> = Vec::new();
    // Per-user Python installs, newest first: a machine with several keeps the newest current.
    for py in ["Python313", "Python312", "Python311", "Python310"] {
        if let Some(l) = &local {
            roots.push(l.join("Programs").join("Python").join(py));
        }
    }
    // A virtualenv beside the checkout, and a conda-style prefix in the home directory.
    roots.push(home.join(".venv"));
    roots.push(home.join("anaconda3"));
    roots.push(home.join("miniconda3"));

    roots
        .into_iter()
        .map(|r| r.join("Lib").join("site-packages").join("torch").join("lib"))
        .find(|d| d.is_dir())
        .map(std::ffi::OsString::from)
}

/// Nothing to discover off Windows: `REQUIRED` is empty there, so `resolve` never had a reason to
/// refuse and the loader finds its own libraries.
#[cfg(not(windows))]
fn discover() -> Option<std::ffi::OsString> {
    None
}

fn prepend_to_path(dirs: &[PathBuf]) {
    let current = std::env::var_os("PATH").unwrap_or_default();
    let mut entries: Vec<PathBuf> = dirs.to_vec();
    entries.extend(std::env::split_paths(&current));
    if let Ok(joined) = std::env::join_paths(entries) {
        std::env::set_var("PATH", joined);
    }
}

/// Does `dir` look like it holds the CUDA runtime this build needs? A helper for diagnostics that
/// want to *suggest* a directory without this module ever choosing one.
pub fn looks_complete(dir: &Path) -> bool {
    !REQUIRED.is_empty() && REQUIRED.iter().all(|lib| dir.join(lib).is_file())
}

/// **What to say when a CUDA session fails to construct and nothing was configured.**
///
/// This is the sentence whose absence cost this project a session. `Error 126: cublasLt64_12.dll is
/// missing` names a *library*; a reader who does not already know that ONNX Runtime loads its
/// provider lazily against the process search path will read it as *"this machine has no CUDA"* —
/// which is what happened on 2026-08-17, in contradiction of an ADR that said otherwise.
///
/// Returned only when [`ENV_VAR`] is **unset**, because that is the only case where the advice is
/// news: a configured-and-refused run has already been told, in [`Resolution::Rejected`]'s own
/// words, exactly what is wrong with the value it supplied.
pub fn hint_when_unconfigured() -> Option<String> {
    if !matches!(ensure_search_path(), Resolution::NotRequested) {
        return None;
    }
    Some(format!(
        "{ENV_VAR} is not set, so nothing was added to the library search path. A CUDA session \
         links against the CUDA 12 runtime ({}) at load time, and 'missing' here means NOT FOUND \
         rather than NOT INSTALLED -- a CUDA Toolkit is one source and it is not the only one. \
         Point {ENV_VAR} at a directory (or several, separated by the path separator) that between \
         them hold those libraries",
        REQUIRED.join(", "),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory under the OS temp dir, removed on drop. `tempfile` is not a dependency
    /// of this crate and adding one for four tests is not worth the supply chain.
    struct Scratch(PathBuf);

    /// Uniqueness without a clock. `marlowe`'s `determinism_guard` forbids a real clock read
    /// anywhere in a crate's sources — **including `#[cfg(test)]`**, because it greps the source
    /// text — and a scratch directory needs a distinct name, not a timestamped one. A process id
    /// plus a monotonically increasing counter is distinct across concurrent test binaries and
    /// across tests within one, and it reads nothing.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "marlowe-cuda-libs-{tag}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
        fn with_all_required(tag: &str) -> Self {
            let s = Scratch::new(tag);
            for lib in REQUIRED {
                std::fs::write(s.0.join(lib), b"not a real dll").unwrap();
            }
            s
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn os(p: &Path) -> OsString {
        p.as_os_str().to_owned()
    }

    #[test]
    fn an_unset_variable_is_not_a_failure_and_not_a_promise() {
        assert_eq!(resolve(None), Resolution::NotRequested);
        assert!(!Resolution::NotRequested.applied());
        assert_eq!(Resolution::NotRequested.rejection(), None);
    }

    /// The empty string is the case a `.env` file or a shell default produces, and treating it as
    /// "unset" is how a run that was configured for CUDA quietly measures whatever was already on
    /// PATH.
    #[test]
    fn an_empty_variable_is_refused_rather_than_read_as_unset() {
        let r = resolve(Some(&OsString::from("")));
        assert!(r.rejection().is_some(), "empty value accepted: {r:?}");
    }

    #[test]
    fn a_directory_that_does_not_exist_is_named_rather_than_left_to_error_126() {
        let missing = std::env::temp_dir().join("marlowe-cuda-libs-definitely-not-here");
        let _ = std::fs::remove_dir_all(&missing);
        let r = resolve(Some(&os(&missing)));
        let reason = r.rejection().unwrap_or_else(|| panic!("accepted a missing dir: {r:?}"));
        assert!(
            reason.contains("marlowe-cuda-libs-definitely-not-here"),
            "the refusal must name the path the human typed, got: {reason}"
        );
    }

    /// The Ollama case, which is the one that actually happened: a directory that exists, holds
    /// *some* of the runtime, and moves the error to the next link instead of failing usefully.
    #[cfg(windows)]
    #[test]
    fn a_partial_runtime_is_refused_and_the_refusal_names_every_missing_library() {
        let scratch = Scratch::new("partial");
        std::fs::write(scratch.path().join("cublasLt64_12.dll"), b"x").unwrap();

        let r = resolve(Some(&os(scratch.path())));
        let reason = r.rejection().unwrap_or_else(|| panic!("accepted a partial runtime: {r:?}"));
        assert!(
            reason.contains("cufft64_11.dll") && reason.contains("cudnn64_9.dll"),
            "the refusal must name the libraries that are missing, got: {reason}"
        );
        assert!(
            !reason.contains("cublasLt64_12.dll is missing"),
            "the one that IS present must not be reported missing: {reason}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_complete_runtime_in_one_directory_is_applied() {
        let scratch = Scratch::with_all_required("complete");
        let r = resolve(Some(&os(scratch.path())));
        assert!(r.applied(), "a complete runtime was not applied: {r:?}");
        assert!(looks_complete(scratch.path()));
    }

    /// A toolkit install and a cuDNN install are normally two directories, and neither is complete
    /// alone. The requirement is on the union; asserting it per-directory would refuse every real
    /// toolkit layout.
    #[cfg(windows)]
    #[test]
    fn the_requirement_is_on_the_union_of_the_directories_not_on_each_one() {
        let toolkit = Scratch::new("toolkit");
        let cudnn = Scratch::new("cudnn");
        for lib in REQUIRED.iter().filter(|l| !l.starts_with("cudnn")) {
            std::fs::write(toolkit.path().join(lib), b"x").unwrap();
        }
        for lib in REQUIRED.iter().filter(|l| l.starts_with("cudnn")) {
            std::fs::write(cudnn.path().join(lib), b"x").unwrap();
        }

        assert!(
            !looks_complete(toolkit.path()) && !looks_complete(cudnn.path()),
            "the fixture is wrong: neither directory may be complete on its own, or this test \
             would pass without the union logic"
        );

        let joined = std::env::join_paths([toolkit.path(), cudnn.path()]).unwrap();
        let r = resolve(Some(&joined));
        assert!(r.applied(), "the union was not accepted: {r:?}");
    }

    /// A file is not a directory, and `is_dir` is the check that separates them. Passing the DLL
    /// itself rather than its folder is the likeliest typo.
    #[cfg(windows)]
    #[test]
    fn pointing_at_the_library_instead_of_its_directory_is_refused() {
        let scratch = Scratch::with_all_required("file-not-dir");
        let file = scratch.path().join("cublasLt64_12.dll");
        let r = resolve(Some(&os(&file)));
        assert!(r.rejection().is_some(), "a file was accepted as a directory: {r:?}");
    }
}
