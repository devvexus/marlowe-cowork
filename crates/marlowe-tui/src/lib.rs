//! The terminal client: Conversation + Situation Board, the Library, approvals.
//!
//! [`State`] is the whole client. Daemon events go through [`State::on_client`], keys through
//! [`State::on_key`]; both return the requests to send. [`render::draw`] is a pure function of
//! the state, so every screen is testable with ratatui's `TestBackend`.

#![forbid(unsafe_code)]

use std::net::SocketAddr;

use marlowe_proto::DaemonInfo;

pub mod client;
pub mod composer;
pub mod context;
pub mod crew;
pub mod images;
mod input;
mod osc;
pub mod library;
pub mod memory;
pub mod onboard;
pub mod render;
pub mod state;
mod term;
pub mod text;
pub mod wake;

pub use client::{Client, Resolver};
pub use input::palette_matches;
pub use state::{ClientEvent, Conn, Entry, State, View};

/// Where to connect.
#[derive(Clone)]
pub struct ConnectInfo {
    pub addr: SocketAddr,
    pub token: String,
}

impl std::fmt::Debug for ConnectInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectInfo").field("addr", &self.addr).finish_non_exhaustive()
    }
}

impl From<&DaemonInfo> for ConnectInfo {
    fn from(d: &DaemonInfo) -> Self {
        Self { addr: d.addr(), token: d.token.clone() }
    }
}

/// A client failure.
#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("daemon refused the connection: {0}")]
    Refused(String),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("{0}")]
    Theme(String),
}

/// Runs the TUI against the daemon at `conn` until the user quits. On reconnect, `daemon.json`
/// in the profile home is re-read so a restarted daemon is found; `conn` is the fallback.
pub fn run(conn: ConnectInfo) -> Result<(), TuiError> {
    let mut first = Some(conn.clone());
    let resolve: Resolver = Box::new(move || {
        if let Some(c) = first.take() {
            return Some(c);
        }
        let home = marlowe_proto::paths::cowork_home();
        match DaemonInfo::read(&home) {
            Ok(Some(info)) => Some(ConnectInfo::from(&info)),
            _ => Some(conn.clone()),
        }
    });
    run_with(resolve)
}

/// Runs the TUI with a custom resolver, called before every connection attempt (the bin uses
/// this to respawn a daemon that went away).
pub fn run_with(resolve: Resolver) -> Result<(), TuiError> {
    let theme = marlowe_surface::Theme::resolve(|k| std::env::var(k).ok(), None)
        .map_err(|e| TuiError::Theme(e.to_string()))?;
    let workspace = std::env::current_dir().ok().filter(|d| !too_broad(d)).map(|p| p.display().to_string());
    let mut state = State::new(workspace);
    state.now_ms = chrono::Local::now().timestamp_millis();
    state.start_boot(last_launch(state.now_ms));
    let client = Client::spawn(resolve);
    term::run(client, state, theme)
}

/// The previous launch time, from `<home>/tui-launch`, and records this one. Failures only cost
/// the short boot.
fn last_launch(now_ms: i64) -> Option<i64> {
    let path = marlowe_proto::paths::cowork_home().join("tui-launch");
    let prev = std::fs::read_to_string(&path).ok().and_then(|s| s.trim().parse::<i64>().ok());
    let _ = std::fs::write(&path, now_ms.to_string());
    prev
}

/// A start directory that should not become a new session's workspace: a drive root, or the
/// home directory or above (a fresh terminal opens there). The daemon's tool host applies the
/// full check; this only avoids proposing one.
fn too_broad(dir: &std::path::Path) -> bool {
    let canon = |p: &std::path::Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let dir = canon(dir);
    if dir.parent().is_none() {
        return true;
    }
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .is_some_and(|home| canon(std::path::Path::new(&home)).starts_with(&dir))
}
