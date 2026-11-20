//! Fixtures for tests in this crate and in `marlowe-engine-install`: a scratch folder, an install
//! record, and a laid-out install with a fake binary. Not used outside tests.

use std::path::{Path, PathBuf};

use crate::backend::Backend;
use crate::installed::{write_record, Device, Installed, Kind};
use crate::layout::{tag_dir, SERVER_EXE};

/// A fresh, empty folder under the system temp dir.
pub fn scratch(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!("marlowe-engines-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

    pub fn record(backend: Backend, tag: &str, kind: Kind, at: u64) -> Installed {
        Installed {
            backend: backend.id().into(),
            tag: tag.into(),
            kind,
            release: None,
            build: None,
            build_number: None,
            builds_on_release: 0,
            published: None,
            installed_at: at,
            device: Device::Gpu,
            note: None,
            version: None,
            cuda: None,
        }
    }

    /// Lays out an installed tag with a fake binary and (optionally) the CUDA backend.
    pub fn lay_out(root: &Path, rec: &Installed, cuda_dll: bool) {
        let backend = Backend::parse(&rec.backend).unwrap();
        let dir = tag_dir(root, backend, &rec.tag).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(SERVER_EXE), b"MZ").unwrap();
        if cuda_dll {
            std::fs::write(dir.join("ggml-cuda.dll"), b"MZ").unwrap();
        }
        write_record(&dir, rec).unwrap();
    }
