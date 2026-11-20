//! Looking upstream: the newest release, and (on request) the newest builds.
//!
//! The daily release check is [`upgrade_offer`]: at most one a day per backend, releases only,
//! never for a build. [`check`] with [`Depth::Build`] is the "Check for updates" button. Results
//! are cached in `<root>\<backend>\remote.json`; a spent GitHub rate limit shows the cache instead.

use std::path::Path;

use marlowe_engines::assets::{plan, Arch, Cuda};
use marlowe_engines::remote::{self, build_number, parse_nightly_tag, BuildRef, Remote, Release};
use marlowe_engines::status::{compare, Update};
use marlowe_engines::{installed, state, Backend};
use marlowe_net::download::DownloadError;

use crate::driver::cuda_driver;
use crate::http::Http;
use crate::EngineError;

/// One day, in seconds.
const DAY: u64 = 24 * 60 * 60;
/// How many builds the list asks for: the newest.
const BUILDS: usize = 30;

/// What to download on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Platform {
    pub arch: Arch,
    pub cuda: Cuda,
}

impl Platform {
    /// This machine: its architecture, and the CUDA build its NVIDIA driver supports.
    pub fn detect() -> Result<Platform, EngineError> {
        let arch = Arch::current().ok_or_else(|| EngineError::Platform("only Windows x64 and ARM64 have prebuilt backends".into()))?;
        Ok(Platform { arch, cuda: marlowe_engines::cuda_for(arch, cuda_driver()) })
    }
}

/// How much to look up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    /// The newest release only.
    Release,
    /// The newest release and the newest builds.
    Build,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    pub remote: Remote,
    /// Why the answer may be old: the rate limit is spent.
    pub note: Option<String>,
}

/// Whether the daily release check for `backend` is due.
pub fn release_due(root: &Path, backend: Backend, now: u64) -> bool {
    state::load(root).of(backend).last_release_check.is_none_or(|t| now.saturating_sub(t) >= DAY)
}

/// Looks `backend` up at `depth` and caches the answer. On a spent rate limit the cached answer
/// comes back with a note; with no cache the error does.
pub fn check(root: &Path, backend: Backend, http: &Http<'_>, platform: Platform, depth: Depth, now: u64) -> Result<Checked, EngineError> {
    let cached = remote::load(root, backend);
    let mut remote = cached.clone().unwrap_or_default();
    match look(root, backend, http, platform, depth, now, &mut remote) {
        Ok(()) => {
            remote.fetched = now;
            if depth == Depth::Build {
                remote.builds_fetched = now;
            }
            remote::save(root, backend, &remote)?;
            state::update(root, |s| s.entry(backend).last_release_check = Some(now))?;
            Ok(Checked { remote, note: None })
        }
        Err(EngineError::Download(DownloadError::RateLimited { url, reset })) => match cached {
            Some(old) => Ok(Checked {
                remote: old,
                note: Some(format!(
                    "GitHub's rate limit is used up{}; showing the last lookup. Set GITHUB_TOKEN to raise it",
                    reset.map(|r| format!(" until {}", clock(r))).unwrap_or_default()
                )),
            }),
            None => Err(DownloadError::RateLimited { url, reset }.into()),
        },
        Err(e) => Err(e),
    }
}

fn look(root: &Path, backend: Backend, http: &Http<'_>, platform: Platform, depth: Depth, now: u64, remote: &mut Remote) -> Result<(), EngineError> {
    let repo = backend.repo();
    match http.api(root, &format!("/repos/{repo}/releases/latest"), now) {
        Ok(body) => {
            let latest = remote::parse_release(&body).map_err(EngineError::Parse)?.release();
            if backend.releases_point_at_builds() {
                let same = remote.latest_release.as_ref().is_some_and(|r| r.tag == latest.tag) && remote.release_build.is_some();
                if !same {
                    remote.release_build = nightly_build(http, &latest)?;
                }
            }
            remote.latest_release = Some(latest);
        }
        // A repo with no release yet.
        Err(EngineError::Download(DownloadError::NotPublished { .. })) => {
            remote.latest_release = None;
            remote.release_build = None;
        }
        Err(e) => return Err(e),
    }
    if depth == Depth::Build {
        let body = http.api(root, &format!("/repos/{repo}/releases?per_page={BUILDS}"), now)?;
        let list = remote::parse_releases(&body).map_err(EngineError::Parse)?;
        remote.builds.clear();
        remote.newest_build = None;
        for g in list.iter().filter(|g| g.prerelease && !g.draft) {
            let rel = g.release();
            remote.builds.push(BuildRef { tag: rel.tag.clone(), published: rel.published.clone(), number: build_number(&rel.tag) });
            if remote.newest_build.is_none() && has_files(backend, &rel, platform) {
                remote.newest_build = Some(rel);
            }
        }
    }
    Ok(())
}

/// The build `nightly-tag.txt` of an upstream release names, as its number.
fn nightly_build(http: &Http<'_>, release: &Release) -> Result<Option<u32>, EngineError> {
    let Some(asset) = release.asset("nightly-tag.txt") else { return Ok(None) };
    let bytes = http.small_file(&asset.url, asset.sha256.as_deref(), &asset.name)?;
    Ok(parse_nightly_tag(&String::from_utf8_lossy(&bytes)).as_deref().and_then(build_number))
}

/// Whether `release` carries every file this platform needs.
pub(crate) fn has_files(backend: Backend, release: &Release, platform: Platform) -> bool {
    plan(backend, &release.tag, platform.arch, platform.cuda).names().iter().all(|n| release.asset(n).is_some())
}

/// The release the daily prompt offers: set only when `backend` is installed, the user has not said
/// to stop asking, a day has passed since the last check, and a newer release exists.
pub fn upgrade_offer(root: &Path, backend: Backend, http: &Http<'_>, platform: Platform, now: u64) -> Result<Option<String>, EngineError> {
    if state::load(root).of(backend).dont_ask || !release_due(root, backend, now) {
        return Ok(None);
    }
    let Some(inst) = installed::current(root, backend) else { return Ok(None) };
    let checked = check(root, backend, http, platform, Depth::Release, now)?;
    let (update, _) = compare(backend, Some(&inst), Some(&checked.remote));
    Ok((update == Update::UpdateAvailable).then(|| checked.remote.latest_release.map(|r| r.tag)).flatten())
}

/// `14:03` UTC for an epoch second: just enough for a rate-limit note.
fn clock(epoch: u64) -> String {
    let day = epoch % DAY;
    format!("{:02}:{:02} UTC", day / 3600, (day % 3600) / 60)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use marlowe_engines::testkit::{lay_out, record};
    use marlowe_engines::Kind;
    use marlowe_net::download::{Reply, Transport};
    use std::collections::HashMap;
    use std::io::Cursor;
    use std::sync::Mutex;

    /// GitHub, from fixed answers keyed by URL; counts the questions.
    #[derive(Default)]
    pub(crate) struct Gh {
        pub routes: Mutex<HashMap<String, (u16, Vec<u8>)>>,
        pub asked: Mutex<Vec<String>>,
    }

    impl Gh {
        pub(crate) fn route(self, url: &str, status: u16, body: impl Into<Vec<u8>>) -> Self {
            self.routes.lock().unwrap().insert(url.to_string(), (status, body.into()));
            self
        }
        pub(crate) fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl Transport for Gh {
        fn get(&self, url: &str) -> Result<Reply, String> {
            self.asked.lock().unwrap().push(url.to_string());
            let (status, body) = self.routes.lock().unwrap().get(url).cloned().ok_or_else(|| format!("no route for {url}"))?;
            let mut r = Reply::new(status, None, Box::new(Cursor::new(body)));
            if status == 403 {
                r.rate_remaining = Some(0);
                r.rate_reset = Some(2_000_000_000);
            }
            Ok(r)
        }
    }

    pub(crate) const PLATFORM: Platform = Platform { arch: Arch::X64, cuda: Cuda::V12_4 };
    pub(crate) const API: &str = "https://api.github.com/repos/ggml-org/llama.cpp";
    pub(crate) const NIGHTLY: &str = "https://github.com/ggml-org/llama.cpp/releases/download/v1.0.0/nightly-tag.txt";

    fn digest(bytes: &[u8]) -> String {
        marlowe_net::download::sha256_bytes(bytes)
    }

    pub(crate) fn asset_json(name: &str, url: &str, sha: &str) -> String {
        format!(r#"{{"name":"{name}","size":10,"digest":"sha256:{sha}","browser_download_url":"{url}"}}"#)
    }

    pub(crate) fn release_json(tag: &str, prerelease: bool, published: &str, assets: &[String]) -> String {
        format!(r#"{{"tag_name":"{tag}","prerelease":{prerelease},"draft":false,"published_at":"{published}","assets":[{}]}}"#, assets.join(","))
    }

    /// An upstream build with this platform's two files (fake digests).
    pub(crate) fn build_json(n: u32, published: &str) -> String {
        let tag = format!("b{n}");
        let a = plan(Backend::LlamaCpp, &tag, PLATFORM.arch, PLATFORM.cuda);
        let assets: Vec<String> = a.names().iter().map(|nm| asset_json(nm, &format!("https://github.com/ggml-org/llama.cpp/releases/download/{tag}/{nm}"), &"a".repeat(64))).collect();
        release_json(&tag, true, published, &assets)
    }

    pub(crate) fn upstream() -> Gh {
        Gh::default()
            .route(
                &format!("{API}/releases/latest"),
                200,
                release_json("v1.0.0", false, "2026-11-13T10:20:19Z", &[asset_json("nightly-tag.txt", NIGHTLY, &digest(b"b12359\n"))]),
            )
            .route(NIGHTLY, 200, b"b12359\n".to_vec())
            .route(
                &format!("{API}/releases?per_page=30"),
                200,
                format!(
                    "[{},{},{}]",
                    // The newest build is still uploading: no files yet.
                    release_json("b12600", true, "2026-11-20T09:00:00Z", &[]),
                    build_json(12560, "2026-11-20T08:00:00Z"),
                    build_json(12559, "2026-11-20T06:00:00Z")
                ),
            )
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("marlowe-discover-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_release_check_maps_the_release_to_its_build_through_the_nightly_file() {
        let root = scratch("release");
        let gh = upstream();
        let c = check(&root, Backend::LlamaCpp, &Http::new(&gh, None), PLATFORM, Depth::Release, 100).unwrap();
        assert_eq!(c.remote.latest_release.as_ref().unwrap().tag, "v1.0.0");
        assert_eq!(c.remote.release_build, Some(12359));
        assert!(c.remote.newest_build.is_none() && c.remote.builds.is_empty(), "a release check does not list builds");
        assert_eq!(gh.asked().len(), 2);
        // The same release again does not fetch the nightly file a second time.
        check(&root, Backend::LlamaCpp, &Http::new(&gh, None), PLATFORM, Depth::Release, 200).unwrap();
        assert_eq!(gh.asked().len(), 3);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_build_check_lists_builds_and_takes_the_newest_one_that_has_its_files() {
        let root = scratch("build");
        let gh = upstream();
        let c = check(&root, Backend::LlamaCpp, &Http::new(&gh, None), PLATFORM, Depth::Build, 100).unwrap();
        assert_eq!(c.remote.newest_build.as_ref().unwrap().tag, "b12560", "b12600 has no files yet");
        assert_eq!(c.remote.builds.iter().filter_map(|b| b.number).collect::<Vec<_>>(), [12600, 12560, 12559]);
        assert!(remote::load(&root, Backend::LlamaCpp).is_some(), "the answer is cached");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_release_prompt_is_once_a_day_releases_only_and_not_after_dont_ask() {
        let root = scratch("offer");
        let mut inst = record(Backend::LlamaCpp, "b12300", Kind::Build, 1);
        inst.build_number = Some(12300);
        inst.release = Some("v0.9.0".into());
        lay_out(&root, &inst, false);
        state::update(&root, |s| s.entry(Backend::LlamaCpp).current = Some("b12300".into())).unwrap();
        let gh = upstream();
        let http = Http::new(&gh, None);
        let day = 86_400;
        assert_eq!(upgrade_offer(&root, Backend::LlamaCpp, &http, PLATFORM, 10 * day).unwrap().as_deref(), Some("v1.0.0"));
        let asked = gh.asked().len();
        assert_eq!(upgrade_offer(&root, Backend::LlamaCpp, &http, PLATFORM, 10 * day + 3600).unwrap(), None, "not again within a day");
        assert_eq!(gh.asked().len(), asked, "and no request is made");
        assert!(upgrade_offer(&root, Backend::LlamaCpp, &http, PLATFORM, 11 * day + 1).unwrap().is_some());
        state::update(&root, |s| s.entry(Backend::LlamaCpp).dont_ask = true).unwrap();
        assert_eq!(upgrade_offer(&root, Backend::LlamaCpp, &http, PLATFORM, 20 * day).unwrap(), None);
        // A backend that is not installed has nothing to upgrade.
        assert_eq!(upgrade_offer(&root, Backend::MsLlamaCpp, &http, PLATFORM, 20 * day).unwrap(), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_build_never_prompts() {
        let root = scratch("nobuild");
        let mut inst = record(Backend::LlamaCpp, "b12400", Kind::Build, 1);
        inst.build_number = Some(12400);
        inst.release = Some("v1.0.0".into());
        lay_out(&root, &inst, false);
        state::update(&root, |s| s.entry(Backend::LlamaCpp).current = Some("b12400".into())).unwrap();
        let gh = upstream();
        assert_eq!(upgrade_offer(&root, Backend::LlamaCpp, &Http::new(&gh, None), PLATFORM, 10 * 86_400).unwrap(), None, "newer builds exist, but only a release is offered");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_spent_rate_limit_shows_the_cache_and_is_remembered() {
        let root = scratch("rate");
        let gh = upstream();
        check(&root, Backend::LlamaCpp, &Http::new(&gh, None), PLATFORM, Depth::Build, 100).unwrap();
        let limited = Gh::default().route(&format!("{API}/releases/latest"), 403, "{\"message\":\"API rate limit exceeded\"}");
        let c = check(&root, Backend::LlamaCpp, &Http::new(&limited, None), PLATFORM, Depth::Build, 200).unwrap();
        assert!(c.note.unwrap().contains("rate limit"), "the cache is shown with a note");
        assert_eq!(c.remote.newest_build.unwrap().tag, "b12560");
        let before = limited.asked().len();
        let again = check(&root, Backend::LlamaCpp, &Http::new(&limited, None), PLATFORM, Depth::Build, 300).unwrap();
        assert!(again.note.is_some());
        assert_eq!(limited.asked().len(), before, "nothing asks before the limit resets");
        // With no cache the failure is the answer.
        let fresh = scratch("rate2");
        assert!(check(&fresh, Backend::LlamaCpp, &Http::new(&limited, None), PLATFORM, Depth::Build, 400).is_err());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&fresh);
    }

    #[test]
    fn a_repo_with_no_release_yet_has_no_latest_release() {
        let root = scratch("norelease");
        let gh = Gh::default().route(&format!("{API}/releases/latest"), 404, "{}");
        let c = check(&root, Backend::LlamaCpp, &Http::new(&gh, None), PLATFORM, Depth::Release, 1).unwrap();
        assert!(c.remote.latest_release.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
