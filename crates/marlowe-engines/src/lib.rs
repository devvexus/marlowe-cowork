//! The llama.cpp backends: which two exist, where they are installed, which one is selected, and
//! what the last look upstream found. Everything here is the filesystem and pure rules; the
//! network side is `marlowe-engine-install`.
//!
//! | Module | What |
//! |---|---|
//! | [`backend`] | the registry: the two backends, their repos and descriptions |
//! | [`assets`] | which release asset to take on this platform and driver |
//! | [`layout`] | `<install dir>\engines\<backend>\<tag>\` and the root override |
//! | [`state`] | `state.json`: the selected backend, current and previous tags, ask flags |
//! | [`installed`] | the record an install leaves, and resolving the server binary |
//! | [`ms`] | which MS file generations an ms-llama.cpp version runs |
//! | [`remote`] | GitHub's release JSON, tag ordering, the cached lookup |
//! | [`status`] | what the window shows for a backend |
//! | [`testkit`] | fixtures for tests |

#![forbid(unsafe_code)]

pub mod assets;
pub mod backend;
pub mod installed;
pub mod layout;
pub mod ms;
pub mod remote;
pub mod state;
pub mod status;
pub mod testkit;

pub use assets::{cuda_for, plan as asset_plan, Arch, AssetPlan, Cuda};
pub use backend::{Backend, DEFAULT};
pub use installed::{server, Device, Installed, Kind, NotInstalled, Server};
pub use layout::{default_root, is_own_server, root_from, ROOT_ENV, SERVER_ENV, SERVER_EXE};
pub use status::{status, Status, Update};

/// Seconds since the epoch.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}
