//! What GitHub says about a repo's releases, and the cached result of the last look.
//!
//! A RELEASE is a GitHub release with `prerelease: false` (upstream `v1.0.0`; the fork's
//! `ms-v1.0`); a BUILD is one with `prerelease: true` (upstream `b12556`, several an hour). An
//! upstream release has no binaries, only `nightly-tag.txt` naming the build it stands for.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::backend::Backend;
use crate::layout::backend_dir;

const CACHE: &str = "remote.json";

/// One downloadable file of a release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub name: String,
    pub size: u64,
    /// The lower-case hex sha256 GitHub publishes in the asset's `digest` field.
    pub sha256: Option<String>,
    pub url: String,
}

/// A release or build with its files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    pub tag: String,
    /// `published_at`, ISO 8601.
    pub published: String,
    pub assets: Vec<Asset>,
}

impl Release {
    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

/// A build seen in the list, without its files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildRef {
    pub tag: String,
    pub published: String,
    /// Upstream's build number (`12556` for `b12556`).
    pub number: Option<u32>,
}

/// The cached result of a look upstream.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remote {
    /// Epoch seconds of the lookup.
    pub fetched: u64,
    /// When the builds list was fetched (a release-only check leaves it).
    #[serde(default)]
    pub builds_fetched: u64,
    pub latest_release: Option<Release>,
    /// The build number `nightly-tag.txt` of `latest_release` names (upstream only).
    pub release_build: Option<u32>,
    /// The newest build that has this platform's files.
    pub newest_build: Option<Release>,
    /// The newest builds, newest first.
    #[serde(default)]
    pub builds: Vec<BuildRef>,
}

// ------------------------------------------------------------------ GitHub's JSON

#[derive(Debug, Deserialize)]
struct GhAsset {
    name: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    digest: Option<String>,
    browser_download_url: String,
}

#[derive(Debug, Deserialize)]
pub struct GhRelease {
    pub tag_name: String,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

impl GhRelease {
    /// The release with its files, digests stripped of their `sha256:` prefix.
    pub fn release(&self) -> Release {
        Release {
            tag: self.tag_name.clone(),
            published: self.published_at.clone().unwrap_or_default(),
            assets: self
                .assets
                .iter()
                .map(|a| Asset {
                    name: a.name.clone(),
                    size: a.size,
                    sha256: a.digest.as_deref().and_then(|d| d.strip_prefix("sha256:")).map(|h| h.to_ascii_lowercase()),
                    url: a.browser_download_url.clone(),
                })
                .collect(),
        }
    }
}

pub fn parse_release(body: &[u8]) -> Result<GhRelease, String> {
    serde_json::from_slice(body).map_err(|e| format!("GitHub's release answer did not parse: {e}"))
}

pub fn parse_releases(body: &[u8]) -> Result<Vec<GhRelease>, String> {
    serde_json::from_slice(body).map_err(|e| format!("GitHub's release list did not parse: {e}"))
}

// ------------------------------------------------------------------ tags

/// `12556` for `b12556`: upstream's build tags.
pub fn build_number(tag: &str) -> Option<u32> {
    let n = tag.strip_prefix('b')?;
    (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())).then(|| n.parse().ok()).flatten()
}

/// The numbers of a release tag: `v1.0.0` and `ms-v1.0` give `[1, 0, 0]` and `[1, 0]`.
pub fn release_key(tag: &str) -> Vec<u32> {
    let start = tag.find(|c: char| c.is_ascii_digit()).unwrap_or(tag.len());
    tag[start..].split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).filter_map(|p| p.parse().ok()).collect()
}

/// Orders release tags by their numbers, a missing part counting as zero.
pub fn release_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut x, mut y) = (release_key(a), release_key(b));
    let n = x.len().max(y.len());
    x.resize(n, 0);
    y.resize(n, 0);
    x.cmp(&y)
}

/// The build tag inside `nightly-tag.txt`.
pub fn parse_nightly_tag(text: &str) -> Option<String> {
    let t = text.trim();
    build_number(t).map(|_| t.to_string())
}

// ------------------------------------------------------------------ the cache

pub fn load(root: &Path, backend: Backend) -> Option<Remote> {
    serde_json::from_str(&std::fs::read_to_string(backend_dir(root, backend).join(CACHE)).ok()?).ok()
}

pub fn save(root: &Path, backend: Backend, remote: &Remote) -> std::io::Result<()> {
    let dir = backend_dir(root, backend);
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!("{CACHE}.tmp"));
    std::fs::write(&tmp, serde_json::to_string(remote).map_err(std::io::Error::other)?)?;
    std::fs::rename(&tmp, dir.join(CACHE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::{Equal, Greater, Less};

    #[test]
    fn upstream_build_tags_have_numbers_and_nothing_else_does() {
        assert_eq!(build_number("b12556"), Some(12556));
        for no in ["v1.0.0", "ms-v1.0", "b", "b12x", "B12", ""] {
            assert_eq!(build_number(no), None, "{no}");
        }
    }

    #[test]
    fn release_tags_order_by_their_numbers() {
        assert_eq!(release_key("v1.0.0"), [1, 0, 0]);
        assert_eq!(release_key("ms-v1.0"), [1, 0]);
        assert_eq!(release_cmp("v1.0.0", "v0.9.0"), Greater);
        assert_eq!(release_cmp("v1.10.0", "v1.9.0"), Greater, "numbers, not text");
        assert_eq!(release_cmp("ms-v1.0", "ms-v1.0.0"), Equal);
        assert_eq!(release_cmp("ms-v1.0", "ms-v1.1"), Less);
    }

    #[test]
    fn the_nightly_file_names_a_build() {
        assert_eq!(parse_nightly_tag("b12359\n").as_deref(), Some("b12359"));
        assert_eq!(parse_nightly_tag("<html>"), None);
    }

    #[test]
    fn githubs_json_becomes_a_release_with_checked_digests() {
        let json = r#"{"tag_name":"v1.0.0","prerelease":false,"draft":false,"published_at":"2026-11-13T10:20:19Z",
            "assets":[{"name":"nightly-tag.txt","size":7,"digest":"sha256:E8352FAF","browser_download_url":"https://github.com/o/r/releases/download/v1.0.0/nightly-tag.txt"},
                      {"name":"old.zip","size":1,"digest":null,"browser_download_url":"https://x/y"}]}"#;
        let r = parse_release(json.as_bytes()).unwrap().release();
        assert_eq!(r.tag, "v1.0.0");
        assert_eq!(r.asset("nightly-tag.txt").unwrap().sha256.as_deref(), Some("e8352faf"));
        assert_eq!(r.asset("old.zip").unwrap().sha256, None, "an asset without a digest cannot be installed");
        assert!(parse_releases(b"{}").is_err());
    }

    #[test]
    fn the_cache_round_trips() {
        let root = crate::testkit::scratch("remote");
        assert!(load(&root, Backend::LlamaCpp).is_none());
        let r = Remote { fetched: 7, release_build: Some(12359), ..Default::default() };
        save(&root, Backend::LlamaCpp, &r).unwrap();
        assert_eq!(load(&root, Backend::LlamaCpp), Some(r));
        assert!(load(&root, Backend::MsLlamaCpp).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
