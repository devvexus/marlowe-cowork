//! Profile home layout and `daemon.json`.
//!
//! ```text
//! %LOCALAPPDATA%\MarloweCowork\   (or $MARLOWE_HOME)
//!   profile\      journal Profile (daemon only)
//!   cowork.db     operational state
//!   daemon.json   {port, token, pid}
//! ```

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Environment override for the profile home (tests, alternate profiles).
pub const HOME_ENV: &str = "MARLOWE_HOME";

/// The profile home: `$MARLOWE_HOME`, else `%LOCALAPPDATA%\MarloweCowork`, else
/// `$HOME/.local/share/MarloweCowork`, else `./MarloweCowork`.
pub fn cowork_home() -> PathBuf {
    if let Some(h) = std::env::var_os(HOME_ENV).filter(|v| !v.is_empty()) {
        return PathBuf::from(h);
    }
    if let Some(l) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
        return PathBuf::from(l).join("MarloweCowork");
    }
    if let Some(h) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(h).join(".local").join("share").join("MarloweCowork");
    }
    PathBuf::from("MarloweCowork")
}

/// The journal Profile directory. Opened only by the daemon.
pub fn profile_dir(home: &Path) -> PathBuf {
    home.join("profile")
}

/// The operational sqlite database.
pub fn cowork_db(home: &Path) -> PathBuf {
    home.join("cowork.db")
}

/// The daemon discovery file.
pub fn daemon_json(home: &Path) -> PathBuf {
    home.join("daemon.json")
}

/// Contents of `daemon.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub port: u16,
    pub token: String,
    pub pid: u32,
}

impl DaemonInfo {
    /// The loopback address the daemon listens on.
    pub fn addr(&self) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, self.port))
    }

    /// Reads `daemon.json`; `Ok(None)` when absent.
    pub fn read(home: &Path) -> io::Result<Option<DaemonInfo>> {
        match std::fs::read(daemon_json(home)) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Writes `daemon.json` atomically (temp file + rename).
    pub fn write(&self, home: &Path) -> io::Result<()> {
        std::fs::create_dir_all(home)?;
        let path = daemon_json(home);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).map_err(io::Error::other)?)?;
        std::fs::rename(&tmp, &path)
    }

    /// Removes `daemon.json` if it still names this pid.
    pub fn remove_if_ours(home: &Path, pid: u32) -> io::Result<()> {
        match Self::read(home) {
            Ok(Some(info)) if info.pid == pid => std::fs::remove_file(daemon_json(home)),
            _ => Ok(()),
        }
    }
}
