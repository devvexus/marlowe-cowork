//! GitHub, as the installer talks to it: the API for release lists, release downloads from
//! `github.com`. Unauthenticated (60 requests an hour), with `GITHUB_TOKEN` honoured when set.
//! A spent limit is remembered in the state file so nothing asks again before it resets.

use std::path::Path;

use marlowe_engines::state;
use marlowe_net::download::{self, Ask, DownloadError, Policy, Transport};

use crate::EngineError;

/// Environment variable holding an optional GitHub token.
pub const TOKEN_ENV: &str = "GITHUB_TOKEN";

const ACCEPT: &str = "application/vnd.github+json";
/// A release list of 30 builds with their files is well under this.
const API_CAP: usize = 8 * 1024 * 1024;

/// Hosts a release download may touch: GitHub, its API and its release storage.
pub fn github_policy() -> Policy {
    Policy::new(&[], &["github.com", "api.github.com", "objects.githubusercontent.com", "release-assets.githubusercontent.com"])
}

/// The transport, the token and the policy.
pub struct Http<'a> {
    pub transport: &'a dyn Transport,
    pub token: Option<String>,
    policy: Policy,
}

impl<'a> Http<'a> {
    pub fn new(transport: &'a dyn Transport, token: Option<String>) -> Self {
        Self { transport, token: token.filter(|t| !t.trim().is_empty()), policy: github_policy() }
    }

    /// With `$GITHUB_TOKEN`.
    pub fn from_env(transport: &'a dyn Transport) -> Self {
        Self::new(transport, std::env::var(TOKEN_ENV).ok())
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// `GET https://api.github.com<path>`, skipped while the saved rate limit is still spent. A
    /// 403 that says the limit is used up saves its reset time.
    pub fn api(&self, root: &Path, path: &str, now: u64) -> Result<Vec<u8>, EngineError> {
        if let Some(until) = state::load(root).rate_until.filter(|u| *u > now) {
            return Err(DownloadError::RateLimited { url: format!("https://api.github.com{path}"), reset: Some(until) }.into());
        }
        let url = format!("https://api.github.com{path}");
        let ask = Ask { accept: ACCEPT, bearer: self.token.as_deref() };
        match download::get_bytes(self.transport, &self.policy, &url, &ask, API_CAP) {
            Ok(b) => Ok(b),
            Err(DownloadError::RateLimited { url, reset }) => {
                // No reset time given: an hour is GitHub's window.
                let until = reset.unwrap_or(now + 3600);
                let _ = state::update(root, |s| s.rate_until = Some(until));
                Err(DownloadError::RateLimited { url, reset: Some(until) }.into())
            }
            Err(e) => Err(e.into()),
        }
    }

    /// A small release file read whole, checked against its published digest.
    pub fn small_file(&self, url: &str, sha256: Option<&str>, name: &str) -> Result<Vec<u8>, EngineError> {
        let want = sha256.ok_or_else(|| EngineError::NoDigest { name: name.to_string() })?;
        let bytes = download::get_bytes(self.transport, &self.policy, url, &Ask::default(), 64 * 1024)?;
        let got = sha256_hex(&bytes);
        if !got.eq_ignore_ascii_case(want) {
            return Err(DownloadError::Digest { name: name.to_string(), got, want: want.to_string() }.into());
        }
        Ok(bytes)
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    download::sha256_bytes(bytes)
}
