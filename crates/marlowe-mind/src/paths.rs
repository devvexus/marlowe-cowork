//! Where models and the old harness profile live.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Environment override for the models directory.
pub const MODELS_ENV: &str = "MARLOWE_MODELS";
/// Embedder subdirectory of the models dir.
pub const EMBEDDER_DIR: &str = "jina-embeddings-v2-small-en";
/// Cross-encoder subdirectory of the models dir.
pub const RERANKER_DIR: &str = "ms-marco-MiniLM-L-2-v2-ft-session-j";

/// The models dir: `$MARLOWE_MODELS`, else `<exe dir>\..\..\models` (dev tree), else
/// `<home>\models`. The first that exists wins; if none does, `<home>\models`.
pub fn resolve_models_dir(home: &Path) -> PathBuf {
    let env = std::env::var_os(MODELS_ENV);
    let exe = std::env::current_exe().ok();
    let candidates = models_dir_candidates(env, exe.as_deref(), home);
    candidates.iter().find(|p| p.is_dir()).cloned().unwrap_or_else(|| home.join("models"))
}

/// Candidate model dirs in priority order.
pub fn models_dir_candidates(env: Option<OsString>, exe: Option<&Path>, home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(e) = env.filter(|v| !v.is_empty()) {
        out.push(PathBuf::from(e));
    }
    if let Some(dev) = exe.and_then(Path::parent).and_then(Path::parent).and_then(Path::parent) {
        out.push(dev.join("models"));
    }
    out.push(home.join("models"));
    out
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
