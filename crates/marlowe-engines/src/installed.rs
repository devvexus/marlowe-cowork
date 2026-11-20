//! What an install leaves behind (`marlowe-install.json` in the tag folder) and how the engine
//! finds the `llama-server` of the selected backend.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::backend::Backend;
use crate::layout::{backend_dir, tag_dir, SERVER_EXE};
use crate::state;

/// The record's file name inside the tag folder.
pub const RECORD: &str = "marlowe-install.json";

/// A release (`prerelease: false` on GitHub) or a build (`prerelease: true`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Release,
    Build,
}

/// What the smoke test saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Device {
    Gpu,
    /// `--list-devices` named no GPU: models would load on the CPU.
    Cpu,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Installed {
    pub backend: String,
    /// The tag folder: `v1.0.0`, `b12556`, `ms-v1.0`.
    pub tag: String,
    pub kind: Kind,
    /// The release this is, or sits on top of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    /// The tag the binaries came from (`b12359` for upstream `v1.0.0`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    /// That build's number, upstream only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_number: Option<u32>,
    /// How many builds this is ahead of `release` (0 for the release itself).
    #[serde(default)]
    pub builds_on_release: u32,
    /// When GitHub published it (ISO 8601).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
    pub installed_at: u64,
    pub device: Device,
    /// The smoke test's note: why it is on the CPU.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The first line of `llama-server --version`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// `12.4` or `13.5` for a CUDA build of the stock backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cuda: Option<String>,
}

impl Installed {
    /// `v1.0.0`, or `v1.0.0 + 197 builds` for a build ahead of its release, else the tag.
    pub fn label(&self) -> String {
        match (&self.kind, &self.release) {
            (Kind::Build, Some(r)) if self.builds_on_release > 0 => {
                format!("{r} + {} build{}", self.builds_on_release, if self.builds_on_release == 1 { "" } else { "s" })
            }
            (Kind::Build, Some(r)) => r.clone(),
            _ => self.tag.clone(),
        }
    }
}

/// Writes the record into `dir` (a tag folder, or its staging folder).
pub fn write_record(dir: &Path, rec: &Installed) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(rec).map_err(std::io::Error::other)?;
    std::fs::write(dir.join(RECORD), json)
}

/// The record of `tag`, if it is installed and readable.
pub fn read(root: &Path, backend: Backend, tag: &str) -> Option<Installed> {
    let dir = tag_dir(root, backend, tag)?;
    serde_json::from_str(&std::fs::read_to_string(dir.join(RECORD)).ok()?).ok()
}

/// Every installed tag of `backend`, newest install first.
pub fn list(root: &Path, backend: Backend) -> Vec<Installed> {
    let Ok(dir) = std::fs::read_dir(backend_dir(root, backend)) else { return Vec::new() };
    let mut out: Vec<Installed> = dir
        .filter_map(Result::ok)
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|e| serde_json::from_str(&std::fs::read_to_string(e.path().join(RECORD)).ok()?).ok())
        .collect();
    out.sort_by(|a, b| b.installed_at.cmp(&a.installed_at).then_with(|| b.tag.cmp(&a.tag)));
    out
}

/// The installed copy the engine launches, `None` when the backend is not installed.
pub fn current(root: &Path, backend: Backend) -> Option<Installed> {
    let tag = state::load(root).of(backend).current?;
    read(root, backend, &tag)
}

/// A backend's server, ready to launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    pub backend: Backend,
    pub tag: String,
    pub dir: PathBuf,
    pub binary: PathBuf,
    /// `ggml-cuda.dll` beside the binary, when the install has one.
    pub backend_dll: Option<PathBuf>,
}

/// The selected backend is not installed (or its folder is damaged).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{backend} is not installed (looked for {looked})")]
pub struct NotInstalled {
    pub backend: Backend,
    pub looked: String,
}

impl NotInstalled {
    /// What the user can do about it.
    pub fn remedy(&self) -> String {
        format!(
            "{self}. Marlowe runs every local model on a llama.cpp backend it installs itself: open the engine window \
             (click `engine` in the title bar, or the palette) and install {}, or set MARLOWE_LLAMA_SERVER to a llama-server binary",
            self.backend
        )
    }
}

/// `backend`'s current server under `root`.
pub fn server(root: &Path, backend: Backend) -> Result<Server, NotInstalled> {
    let missing = |looked: String| NotInstalled { backend, looked };
    let Some(tag) = state::load(root).of(backend).current else {
        return Err(missing(backend_dir(root, backend).display().to_string()));
    };
    let dir = tag_dir(root, backend, &tag).ok_or_else(|| missing(format!("a folder named {tag:?}")))?;
    let binary = dir.join(SERVER_EXE);
    if !binary.is_file() {
        return Err(missing(binary.display().to_string()));
    }
    let dll = dir.join("ggml-cuda.dll");
    Ok(Server { backend, tag, dir, binary, backend_dll: dll.is_file().then_some(dll) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{lay_out, record, scratch};

    #[test]
    fn an_uninstalled_backend_is_refused_with_a_remedy_and_no_ollama_fallback() {
        let root = scratch("none");
        let e = server(&root, Backend::LlamaCpp).unwrap_err();
        let text = e.remedy();
        assert!(text.contains("llama.cpp is not installed") && text.contains("engine window") && !text.to_lowercase().contains("ollama"), "{text}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_server_is_the_current_tags_binary_with_its_cuda_backend_beside_it() {
        let root = scratch("server");
        let rec = record(Backend::LlamaCpp, "v1.0.0", Kind::Release, 5);
        lay_out(&root, &rec, true);
        // Installed but not current: not served.
        assert!(server(&root, Backend::LlamaCpp).is_err());
        state::update(&root, |s| s.entry(Backend::LlamaCpp).current = Some("v1.0.0".into())).unwrap();
        let s = server(&root, Backend::LlamaCpp).unwrap();
        assert_eq!(s.binary, tag_dir(&root, Backend::LlamaCpp, "v1.0.0").unwrap().join(SERVER_EXE));
        assert!(s.backend_dll.is_some());
        // The other backend has its own folder and its own current tag.
        assert!(server(&root, Backend::MsLlamaCpp).is_err());
        // A current tag whose binary vanished is a refusal naming the file.
        std::fs::remove_file(&s.binary).unwrap();
        assert!(server(&root, Backend::LlamaCpp).unwrap_err().looked.contains(SERVER_EXE));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_build_ahead_of_its_release_reads_as_the_release_plus_builds() {
        let mut r = record(Backend::LlamaCpp, "b12556", Kind::Build, 1);
        assert_eq!(r.label(), "b12556");
        r.release = Some("v1.0.0".into());
        r.builds_on_release = 197;
        assert_eq!(r.label(), "v1.0.0 + 197 builds");
        r.builds_on_release = 1;
        assert_eq!(r.label(), "v1.0.0 + 1 build");
        let rel = record(Backend::LlamaCpp, "v1.0.0", Kind::Release, 1);
        assert_eq!(rel.label(), "v1.0.0");
    }

    #[test]
    fn list_skips_staging_folders_and_orders_newest_first() {
        let root = scratch("list");
        lay_out(&root, &record(Backend::LlamaCpp, "b1", Kind::Build, 1), false);
        lay_out(&root, &record(Backend::LlamaCpp, "b2", Kind::Build, 2), false);
        std::fs::create_dir_all(backend_dir(&root, Backend::LlamaCpp).join(".staging-b3")).unwrap();
        let tags: Vec<String> = list(&root, Backend::LlamaCpp).into_iter().map(|i| i.tag).collect();
        assert_eq!(tags, ["b2", "b1"]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
