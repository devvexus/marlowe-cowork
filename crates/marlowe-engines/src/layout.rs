//! Where engines live: `<install dir>\engines\<backend>\<tag>\`.

use std::path::{Path, PathBuf};

use crate::backend::Backend;

/// Overrides the engines root (tests, a developer's own tree).
pub const ROOT_ENV: &str = "MARLOWE_ENGINES_DIR";

/// The server binary's file name.
pub const SERVER_EXE: &str = if cfg!(windows) { "llama-server.exe" } else { "llama-server" };

/// The engines root for an install dir: `$MARLOWE_ENGINES_DIR` when set, else `<install dir>\engines`.
/// The daemon passes `marlowe_mind::paths::install_dir()`.
pub fn root_from(install_dir: Option<PathBuf>) -> Option<PathBuf> {
    match std::env::var_os(ROOT_ENV).filter(|v| !v.is_empty()) {
        Some(env) => Some(PathBuf::from(env)),
        None => install_dir.map(|d| d.join("engines")),
    }
}

/// [`root_from`] with the fixed install location, `%LOCALAPPDATA%\MarloweCowork`: the same
/// folder `marlowe_mind::paths::install_dir()` names.
pub fn default_root() -> Option<PathBuf> {
    root_from(std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()).map(|l| PathBuf::from(l).join("MarloweCowork")))
}

/// `<root>\<backend>`.
pub fn backend_dir(root: &Path, backend: Backend) -> PathBuf {
    root.join(backend.id())
}

/// `<root>\<backend>\<tag>`. A tag with a path separator or `..` is not a folder name.
pub fn tag_dir(root: &Path, backend: Backend, tag: &str) -> Option<PathBuf> {
    let ok = !tag.is_empty() && !tag.contains(['/', '\\', ':']) && !tag.starts_with('.');
    ok.then(|| backend_dir(root, backend).join(tag))
}

/// Where an install is unpacked before it is moved into place.
pub fn staging_dir(root: &Path, backend: Backend, tag: &str) -> Option<PathBuf> {
    tag_dir(root, backend, tag).map(|d| d.with_file_name(format!(".staging-{tag}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tag_that_would_leave_the_backend_folder_is_not_a_folder() {
        let root = Path::new("C:/e");
        assert!(tag_dir(root, Backend::LlamaCpp, "v1.0.0").is_some());
        for bad in ["", "..", "../x", "a/b", "a\\b", "c:x", ".staging-x"] {
            assert!(tag_dir(root, Backend::LlamaCpp, bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn the_root_is_the_install_dirs_engines_folder() {
        // The override variable is not set in the unit tests; `root_from` is pure otherwise.
        if std::env::var_os(ROOT_ENV).is_none() {
            assert_eq!(root_from(Some(PathBuf::from("C:/i"))), Some(PathBuf::from("C:/i").join("engines")));
            assert_eq!(root_from(None), None);
        }
    }
}
