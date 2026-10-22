//! Where models and the old harness profile live.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Environment override for the models directory.
pub const MODELS_ENV: &str = "MARLOWE_MODELS";
/// Embedder subdirectory of the models dir.
pub const EMBEDDER_DIR: &str = "jina-embeddings-v2-small-en";
/// Cross-encoder subdirectory of the models dir.
pub const RERANKER_DIR: &str = "ms-marco-MiniLM-L-2-v2-ft-session-j";

/// File in the fixed install dir naming the models dir last used with `$MARLOWE_MODELS` set, so a
/// process that did not inherit the variable (a Windows Terminal relaunch) still finds it.
pub const MODELS_POINTER: &str = "models.path";

/// The fixed install location: `%LOCALAPPDATA%\MarloweCowork`, independent of `$MARLOWE_HOME`.
pub fn install_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()).map(|l| PathBuf::from(l).join("MarloweCowork"))
}

/// Whether `dir` holds the embedder and reranker directories.
pub fn has_models(dir: &Path) -> bool {
    dir.join(EMBEDDER_DIR).is_dir() && dir.join(RERANKER_DIR).is_dir()
}

/// The models dir: the first candidate holding both model directories (see
/// [`models_dir_candidates`]), else the first that exists, else `<home>\models`.
pub fn resolve_models_dir(home: &Path) -> PathBuf {
    let env = std::env::var_os(MODELS_ENV);
    let exe = std::env::current_exe().ok();
    let fixed = install_dir();
    let candidates = models_dir_candidates(env, exe.as_deref(), fixed.as_deref(), home);
    candidates
        .iter()
        .find(|p| has_models(p))
        .or_else(|| candidates.iter().find(|p| p.is_dir()))
        .cloned()
        .unwrap_or_else(|| home.join("models"))
}

/// Candidate model dirs in priority order: `$MARLOWE_MODELS`, the remembered pointer in `fixed`,
/// `models` beside the exe, the dev tree (`<exe dir>\..\..\models`), `<fixed>\models`, `<home>\models`.
pub fn models_dir_candidates(
    env: Option<OsString>,
    exe: Option<&Path>,
    fixed: Option<&Path>,
    home: &Path,
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if !out.contains(&p) {
            out.push(p);
        }
    };
    if let Some(e) = env.filter(|v| !v.is_empty()) {
        push(PathBuf::from(e));
    }
    if let Some(p) = fixed.and_then(|f| std::fs::read_to_string(f.join(MODELS_POINTER)).ok()) {
        let p = p.trim();
        if !p.is_empty() {
            push(PathBuf::from(p));
        }
    }
    if let Some(dir) = exe.and_then(Path::parent) {
        push(dir.join("models"));
    }
    if let Some(dev) = exe.and_then(Path::parent).and_then(Path::parent).and_then(Path::parent) {
        push(dev.join("models"));
    }
    if let Some(f) = fixed {
        push(f.join("models"));
    }
    push(home.join("models"));
    out
}

/// Remember `$MARLOWE_MODELS` in the fixed install dir when it names a real models dir, so later
/// processes that did not inherit it find the same models. Best effort; returns whether it wrote.
pub fn remember_models_dir() -> bool {
    let Some(env) = std::env::var_os(MODELS_ENV).filter(|v| !v.is_empty()) else { return false };
    let Some(fixed) = install_dir() else { return false };
    remember_models_dir_in(Path::new(&env), &fixed)
}

/// [`remember_models_dir`] against an explicit install dir.
pub fn remember_models_dir_in(models: &Path, fixed: &Path) -> bool {
    if !has_models(models) {
        return false;
    }
    let file = fixed.join(MODELS_POINTER);
    let want = models.to_string_lossy().into_owned();
    if std::fs::read_to_string(&file).is_ok_and(|have| have.trim() == want) {
        return false;
    }
    std::fs::create_dir_all(fixed).is_ok() && std::fs::write(&file, want).is_ok()
}

/// Copy the model directories from `src` into `<fixed>\models`. Returns the destination.
pub fn install_models(src: &Path, fixed: &Path) -> std::io::Result<PathBuf> {
    if !has_models(src) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{} does not hold {EMBEDDER_DIR} and {RERANKER_DIR}", src.display()),
        ));
    }
    let dest = fixed.join("models");
    for sub in [EMBEDDER_DIR, RERANKER_DIR] {
        copy_dir(&src.join(sub), &dest.join(sub))?;
    }
    Ok(dest)
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// The journal Profile directory inside the cowork home.
pub fn profile_dir(home: &Path) -> PathBuf {
    marlowe_proto::paths::profile_dir(home)
}

/// The old harness profile: `%LOCALAPPDATA%\marlowe\default-profile`. Read-only, always.
pub fn default_legacy_profile() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .map(|l| PathBuf::from(l).join("marlowe").join("default-profile"))
}
