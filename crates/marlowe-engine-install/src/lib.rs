//! Keeping the llama.cpp backends installed and up to date: looking at GitHub's releases, the
//! checked download, unpacking, the smoke test, rollback and pruning.
//!
//! | Module | What |
//! |---|---|
//! | [`http`] | the GitHub client: hosts, token, rate limit, the answers |
//! | [`discover`] | the release / build lookup and its per-day check |
//! | [`unpack`] | zip extraction that cannot leave its folder |
//! | [`smoke`] | `--version` and `--list-devices` on a fresh install |
//! | [`driver`] | the CUDA version the NVIDIA driver supports |
//! | [`install`] | install, upgrade, sync, rollback, prune |
//!
//! The transport and the process runner are traits, so every path runs against fixtures; the
//! real ones are [`marlowe_net::download::Net`] and [`smoke::SystemRunner`].

#![forbid(unsafe_code)]

pub mod discover;
pub mod driver;
pub mod http;
pub mod install;
pub mod smoke;
pub mod unpack;

/// Why a backend operation failed.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("{0}")]
    Download(#[from] marlowe_net::download::DownloadError),
    #[error("{0}")]
    Parse(String),
    /// No prebuilt backend for this OS or architecture.
    #[error("no prebuilt backend for this platform: {0}")]
    Platform(String),
    /// The release has no file with the name this platform needs.
    #[error("{tag} has no {name}")]
    NoAsset { tag: String, name: String },
    /// GitHub did not publish a sha256 for the file, so it cannot be checked.
    #[error("{name} has no published sha256 to check it against; it was not downloaded")]
    NoDigest { name: String },
    #[error("{0}")]
    Unpack(String),
    #[error("the new install failed its smoke test: {0}")]
    Smoke(String),
    #[error("{0}")]
    State(String),
}

impl From<std::io::Error> for EngineError {
    fn from(e: std::io::Error) -> Self {
        EngineError::State(e.to_string())
    }
}
