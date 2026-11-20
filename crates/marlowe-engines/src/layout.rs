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

/// The developer override for the server binary (the engine's own lookup is in `marlowe-provider`).
pub const SERVER_ENV: &str = "MARLOWE_LLAMA_SERVER";

/// Whether the process image at `path` is Marlowe's own engine server: one under the engines root,
/// or the `MARLOWE_LLAMA_SERVER` binary. The GPU budget counts its memory as Marlowe's (the engine
/// stops it before it starts another); any other `llama-server` is someone else's.
pub fn is_own_server(path: &str) -> bool {
    own_server_in(path, default_root().as_deref(), std::env::var(SERVER_ENV).ok().as_deref())
}

/// [`is_own_server`] with the root and the override given.
pub fn own_server_in(path: &str, root: Option<&Path>, override_path: Option<&str>) -> bool {
    let norm = |p: &str| p.replace('/', "\\").trim_end_matches('\\').to_ascii_lowercase();
    let image = norm(path);
    if override_path.is_some_and(|o| !o.trim().is_empty() && norm(o) == image) {
        return true;
    }
    let name = image.rsplit('\\').next().unwrap_or(&image);
    root.is_some_and(|r| name.starts_with("llama-server") && image.starts_with(&format!("{}\\", norm(&r.display().to_string()))))
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
    fn only_a_server_under_the_engines_root_or_the_override_is_marlowes_own() {
        let root = Path::new("C:/Users/m/AppData/Local/MarloweCowork/engines");
        let ours = r"C:\Users\m\AppData\Local\MarloweCowork\engines\llama.cpp\v1.0.0\llama-server.exe";
        assert!(own_server_in(ours, Some(root), None));
        assert!(!own_server_in(r"C:\Users\m\AppData\Local\Programs\Ollama\lib\ollama\llama-server.exe", Some(root), None));
        assert!(!own_server_in(r"C:\Users\m\AppData\Local\MarloweCowork\engines-old\llama-server.exe", Some(root), None));
        assert!(!own_server_in(ours, None, None));
        assert!(own_server_in(r"C:\src\llama.cpp\build\bin\llama-server.exe", None, Some("C:/src/llama.cpp/build/bin/llama-server.exe")));
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
