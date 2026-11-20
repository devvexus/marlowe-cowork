//! The llama.cpp backends on the wire: what is installed, what is newer upstream, and the
//! progress of an install. Backends are named by their string id (`llama.cpp`, `ms-llama.cpp`);
//! `marlowe-engines` owns the registry.

use serde::{Deserialize, Serialize};

/// What a backend operation does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendOp {
    /// First install, or an upgrade to the newest release.
    Install,
    /// Looking at the newest build.
    Check,
    /// Installing the newest build, even within the same release.
    Sync,
    /// Making a backend the one that serves models.
    Select,
}

/// Whether the installed copy is the newest release.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendUpdate {
    NotInstalled,
    UpToDate,
    UpdateAvailable,
    /// Installed, but no release has been looked up yet (or none exists).
    #[default]
    Unknown,
}

/// One installed build.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendBuild {
    /// The folder and GitHub tag: `v1.0.0`, `b12556`, `ms-v1.0`.
    pub tag: String,
    /// What to show: `v1.0.0`, or `v1.0.0 + 197 builds` for a build ahead of its release.
    pub label: String,
    /// The release it is, or sits on top of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    /// The build tag the binaries are from (`b12359` for upstream `v1.0.0`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    /// When GitHub published it (ISO 8601).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    /// `gpu`, or `cpu` when the smoke test saw no GPU.
    pub device: String,
    /// Why it is `cpu`, or the version line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A tag looked up upstream.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteTag {
    pub tag: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
}

/// A running operation, for the window's progress line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendBusy {
    pub op: BackendOp,
    pub message: String,
    pub done: u64,
    pub total: u64,
}

/// Everything the backends window shows for one backend.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendReport {
    pub id: String,
    pub title: String,
    pub description: String,
    pub repo: String,
    /// About how much a first install downloads, for the offer that asks first.
    #[serde(default)]
    pub approx_bytes: u64,
    pub selected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed: Option<BackendBuild>,
    /// The version kept for rollback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_release: Option<RemoteTag>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest_build: Option<RemoteTag>,
    #[serde(default)]
    pub state: BackendUpdate,
    /// How many builds newer than the installed one exist upstream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builds_newer: Option<u32>,
    /// Unix seconds of the last lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_checked: Option<u64>,
    /// The user said not to offer release upgrades.
    #[serde(default)]
    pub dont_ask: bool,
    /// What upgrading to the newest release would cost: the MS models it would stop running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade_warning: Option<String>,
    /// The same for syncing to the newest build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_warning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub busy: Option<BackendBusy>,
    /// The last failure, or a rate-limit note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}
