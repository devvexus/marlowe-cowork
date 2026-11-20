//! Installing a backend: the newest release (install or upgrade), or the newest build (sync).
//!
//! The files are downloaded and checked against the sha256 GitHub publishes, unpacked into
//! `<root>\<backend>\.staging-<tag>`, smoke-tested there, and only then moved to
//! `<root>\<backend>\<tag>` and made current. The previous version stays for rollback; anything
//! older is pruned. A failure anywhere leaves the current version as it was.

use std::path::{Path, PathBuf};

use marlowe_engines::assets::plan;
use marlowe_engines::installed::{self, write_record, Device, Installed, Kind};
use marlowe_engines::layout::{backend_dir, staging_dir, tag_dir};
use marlowe_engines::remote::{build_number, Release, Remote};
use marlowe_engines::status::{compare, Update};
use marlowe_engines::{now_secs, state, Backend, SERVER_EXE};
use marlowe_net::download;

use crate::discover::{check, has_files, Depth, Platform};
use crate::http::Http;
use crate::smoke::{smoke, Runner};
use crate::unpack::{flatten, unzip};
use crate::EngineError;

/// What to install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// The newest release: a first install, or the upgrade.
    Release,
    /// The newest build, even inside the installed release.
    Build,
}

/// One step of progress: a line, and bytes of the whole (0 total while unknown).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    pub message: String,
    pub done: u64,
    pub total: u64,
}

/// Everything an install needs from outside.
pub struct Context<'a> {
    pub root: &'a Path,
    /// Where the zips are fetched to (and reused from when their digest matches).
    pub downloads: &'a Path,
    pub http: &'a Http<'a>,
    pub runner: &'a dyn Runner,
    pub platform: Platform,
    /// Keeps the zips after a successful install (they are large).
    pub keep_downloads: bool,
}

/// Installs the newest release or build of `backend` and makes it current. Returns the record;
/// when the newest is already current it is returned without downloading anything.
pub fn install(ctx: &Context<'_>, backend: Backend, target: Target, on: &mut dyn FnMut(Progress)) -> Result<Installed, EngineError> {
    let say = |on: &mut dyn FnMut(Progress), m: &str, done: u64, total: u64| on(Progress { message: m.to_string(), done, total });
    say(on, &format!("looking up {backend} on GitHub"), 0, 0);
    let depth = if target == Target::Build { Depth::Build } else { Depth::Release };
    let remote = check(ctx.root, backend, ctx.http, ctx.platform, depth, now_secs())?.remote;
    let pick = choose(backend, target, &remote)?;
    let working = installed::server(ctx.root, backend).is_ok();
    if let (Some(have), true) = (installed::current(ctx.root, backend), working) {
        // The newest release is already there, or a build at least as new: nothing to fetch (a
        // build ahead of its release is not swapped back for the release).
        let same = have.tag == pick.rec.tag;
        let ahead = target == Target::Release && compare(backend, Some(&have), Some(&remote)).0 == Update::UpToDate;
        if same || ahead {
            say(on, &format!("{backend} {} is already installed", have.label()), 1, 1);
            return Ok(have);
        }
    }
    // The files live on the build's release; an upstream release has none of its own.
    let binaries = match pick.binaries {
        Binaries::Here(r) => r,
        Binaries::Tag(tag) => {
            say(on, &format!("looking up the files of {tag}"), 0, 0);
            fetch_release(ctx, backend, &tag, &remote)?
        }
    };
    let names = plan(backend, &binaries.tag, ctx.platform.arch, ctx.platform.cuda);
    let mut files: Vec<(String, PathBuf, String, String, u64)> = Vec::new();
    for name in names.names() {
        let asset = binaries.asset(name).ok_or_else(|| EngineError::NoAsset { tag: binaries.tag.clone(), name: name.to_string() })?;
        let sha = asset.sha256.clone().ok_or_else(|| EngineError::NoDigest { name: name.to_string() })?;
        files.push((name.to_string(), ctx.downloads.join(name), asset.url.clone(), sha, asset.size));
    }
    let total: u64 = files.iter().map(|f| f.4).sum();
    let mut before = 0u64;
    for (name, path, url, sha, size) in &files {
        say(on, &format!("downloading {name}"), before, total);
        download::fetch_verified(url, path, sha, name, ctx.http.transport, ctx.http.policy(), &mut |n| {
            on(Progress { message: format!("downloading {name}"), done: before + n, total })
        })?;
        before += size;
    }
    let stage = staging_dir(ctx.root, backend, &pick.rec.tag).ok_or_else(|| EngineError::State(format!("{:?} is not a folder name", pick.rec.tag)))?;
    let built = build_in(ctx, backend, &stage, &files, pick.rec, names.runtime.is_some().then(|| ctx.platform.cuda.version().to_string()), on);
    let rec = match built {
        Ok(r) => r,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&stage);
            return Err(e);
        }
    };
    say(on, "making it current", total, total);
    commit(ctx.root, backend, &stage, &rec)?;
    if !ctx.keep_downloads {
        for f in &files {
            let _ = std::fs::remove_file(&f.1);
        }
    }
    say(on, &format!("{backend} {} is installed", rec.label()), total, total);
    Ok(rec)
}

/// Unpacks into the staging folder, smoke-tests it and writes the record.
fn build_in(
    ctx: &Context<'_>,
    backend: Backend,
    stage: &Path,
    files: &[(String, PathBuf, String, String, u64)],
    mut rec: Installed,
    cuda: Option<String>,
    on: &mut dyn FnMut(Progress),
) -> Result<Installed, EngineError> {
    let _ = std::fs::remove_dir_all(stage);
    std::fs::create_dir_all(stage)?;
    for (name, path, ..) in files {
        unzip(path, stage, &mut |done, total| on(Progress { message: format!("unpacking {name}"), done, total }))?;
    }
    flatten(stage)?;
    on(Progress { message: "testing llama-server".into(), done: 0, total: 0 });
    let s = smoke(ctx.runner, &stage.join(SERVER_EXE))?;
    rec.device = s.device;
    rec.note = s.note;
    rec.version = Some(s.version);
    rec.cuda = cuda;
    rec.installed_at = now_secs();
    rec.backend = backend.id().to_string();
    write_record(stage, &rec)?;
    Ok(rec)
}

/// Moves the staging folder into place and makes it current, keeping the previous version.
fn commit(root: &Path, backend: Backend, stage: &Path, rec: &Installed) -> Result<(), EngineError> {
    let dest = tag_dir(root, backend, &rec.tag).ok_or_else(|| EngineError::State(format!("{:?} is not a folder name", rec.tag)))?;
    if dest.exists() {
        std::fs::remove_dir_all(&dest).map_err(|e| EngineError::State(format!("could not replace {}: {e}; stop the engine and try again", dest.display())))?;
    }
    std::fs::rename(stage, &dest)?;
    state::update(root, |s| {
        let own = s.entry(backend);
        if own.current.as_deref() != Some(rec.tag.as_str()) {
            own.previous = own.current.take();
        }
        own.current = Some(rec.tag.clone());
    })?;
    prune(root, backend);
    Ok(())
}

/// Removes every installed version but the current and the previous, and leftover staging folders.
pub fn prune(root: &Path, backend: Backend) {
    let st = state::load(root).of(backend);
    let keep: Vec<String> = st.current.into_iter().chain(st.previous).collect();
    let Ok(dir) = std::fs::read_dir(backend_dir(root, backend)) else { return };
    for e in dir.filter_map(Result::ok) {
        let name = e.file_name().to_string_lossy().into_owned();
        if e.path().is_dir() && !keep.contains(&name) {
            // A folder in use (a running server) refuses to go; the next prune takes it.
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Makes the previous version current again, and the current one the previous. The tag it
/// returns is the one now current.
pub fn rollback(root: &Path, backend: Backend) -> Result<Installed, EngineError> {
    let st = state::load(root).of(backend);
    let prev = st.previous.ok_or_else(|| EngineError::State(format!("{backend} has no previous version to go back to")))?;
    let rec = installed::read(root, backend, &prev).ok_or_else(|| EngineError::State(format!("{backend} {prev} is no longer installed")))?;
    if installed::server(root, backend).is_ok() || tag_dir(root, backend, &prev).is_some_and(|d| d.join(SERVER_EXE).is_file()) {
        state::update(root, |s| {
            let own = s.entry(backend);
            own.previous = own.current.take();
            own.current = Some(prev.clone());
        })?;
        return Ok(rec);
    }
    Err(EngineError::State(format!("{backend} {prev} has no {SERVER_EXE}")))
}

// ------------------------------------------------------------------ choosing

enum Binaries {
    /// The release carries its own files.
    Here(Release),
    /// The files are on this build's release (upstream releases point at a build).
    Tag(String),
}

struct Pick {
    rec: Installed,
    binaries: Binaries,
}

/// What `target` means for `backend` given the last lookup: the folder tag, the record to write
/// and where the files are.
fn choose(backend: Backend, target: Target, remote: &Remote) -> Result<Pick, EngineError> {
    let blank = |tag: &str, kind: Kind, published: &str| Installed {
        backend: backend.id().into(),
        tag: tag.into(),
        kind,
        release: None,
        build: None,
        build_number: None,
        builds_on_release: 0,
        published: Some(published.to_string()).filter(|p| !p.is_empty()),
        installed_at: 0,
        device: Device::Gpu,
        note: None,
        version: None,
        cuda: None,
    };
    match target {
        Target::Release => {
            let latest = remote.latest_release.clone().ok_or_else(|| EngineError::State(format!("{backend} has no release yet; use \"Check for updates\" and \"Sync to upstream\" for a build")))?;
            let mut rec = blank(&latest.tag, Kind::Release, &latest.published);
            rec.release = Some(latest.tag.clone());
            if backend.releases_point_at_builds() {
                let n = remote.release_build.ok_or_else(|| EngineError::Parse(format!("{} has no nightly-tag.txt, so its build is unknown", latest.tag)))?;
                rec.build = Some(format!("b{n}"));
                rec.build_number = Some(n);
                Ok(Pick { rec, binaries: Binaries::Tag(format!("b{n}")) })
            } else {
                Ok(Pick { rec, binaries: Binaries::Here(latest) })
            }
        }
        Target::Build => {
            let newest = remote.newest_build.clone().ok_or_else(|| EngineError::State(format!("{backend} has no build with files for this machine")))?;
            let mut rec = blank(&newest.tag, Kind::Build, &newest.published);
            rec.release = remote.latest_release.as_ref().map(|r| r.tag.clone());
            rec.build = Some(newest.tag.clone());
            rec.build_number = build_number(&newest.tag);
            rec.builds_on_release = builds_on_release(backend, remote, &newest, rec.build_number);
            Ok(Pick { rec, binaries: Binaries::Here(newest) })
        }
    }
}

/// How many builds the build sits ahead of the newest release.
fn builds_on_release(backend: Backend, remote: &Remote, build: &Release, number: Option<u32>) -> u32 {
    if backend.releases_point_at_builds() {
        return match (number, remote.release_build) {
            (Some(n), Some(r)) => n.saturating_sub(r),
            _ => 0,
        };
    }
    let Some(release) = remote.latest_release.as_ref() else { return 0 };
    remote.builds.iter().filter(|b| b.published > release.published && b.published <= build.published).count() as u32
}

/// The build's release by tag, from the lookup when it is the newest build, else from the API.
fn fetch_release(ctx: &Context<'_>, backend: Backend, tag: &str, remote: &Remote) -> Result<Release, EngineError> {
    if let Some(r) = remote.newest_build.as_ref().filter(|r| r.tag == tag && has_files(backend, r, ctx.platform)) {
        return Ok(r.clone());
    }
    let body = ctx.http.api(ctx.root, &format!("/repos/{}/releases/tags/{tag}", backend.repo()), now_secs())?;
    Ok(marlowe_engines::remote::parse_release(&body).map_err(EngineError::Parse)?.release())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover::tests::*;
    use crate::smoke::tests::{ok, Fake};
    use crate::unpack::tests::make_zip;
    use marlowe_engines::Backend;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("marlowe-install-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// GitHub with `v1.0.0` -> `b12359`, whose two real-named zips are fixtures with true digests.
    fn github_with_files() -> (Gh, Vec<u8>) {
        let main = make_zip(&[("llama-server.exe", b"MZ"), ("ggml-cuda.dll", b"MZ")]);
        let rt = make_zip(&[("cudart64_12.dll", b"MZ")]);
        let sha = |b: &[u8]| marlowe_net::download::sha256_bytes(b);
        let base = "https://github.com/ggml-org/llama.cpp/releases/download/b12359";
        let (an, rn) = ("llama-b12359-bin-win-cuda-12.4-x64.zip", "cudart-llama-bin-win-cuda-12.4-x64.zip");
        let rel = release_json("b12359", true, "2026-11-13T10:21:06Z", &[asset_json(an, &format!("{base}/{an}"), &sha(&main)), asset_json(rn, &format!("{base}/{rn}"), &sha(&rt))]);
        let gh = upstream().route(&format!("{API}/releases/tags/b12359"), 200, rel).route(&format!("{base}/{an}"), 200, main.clone()).route(&format!("{base}/{rn}"), 200, rt);
        (gh, main)
    }

    /// Teaches the fixtures a newest build, b12560, with true digests for its two files.
    fn with_build(gh: &Gh, main: &[u8]) {
        let sha = |b: &[u8]| marlowe_net::download::sha256_bytes(b);
        let rt = make_zip(&[("cudart64_12.dll", b"MZ")]);
        let base = "https://github.com/ggml-org/llama.cpp/releases/download/b12560";
        let (an, rn) = ("llama-b12560-bin-win-cuda-12.4-x64.zip", "cudart-llama-bin-win-cuda-12.4-x64.zip");
        let list = format!(
            "[{}]",
            release_json("b12560", true, "2026-11-20T08:00:00Z", &[asset_json(an, &format!("{base}/{an}"), &sha(main)), asset_json(rn, &format!("{base}/{rn}"), &sha(&rt))])
        );
        let mut routes = gh.routes.lock().unwrap();
        routes.insert(format!("{API}/releases?per_page=30"), (200, list.into_bytes()));
        routes.insert(format!("{base}/{an}"), (200, main.to_vec()));
        routes.insert(format!("{base}/{rn}"), (200, rt));
    }

    fn run(gh: &Gh, root: &Path, runner: &dyn Runner, target: Target) -> Result<Installed, EngineError> {
        let dl = root.join("dl");
        let http = Http::new(gh, None);
        let ctx = Context { root, downloads: &dl, http: &http, runner, platform: PLATFORM, keep_downloads: false };
        install(&ctx, Backend::LlamaCpp, target, &mut |_| {})
    }

    #[test]
    fn a_release_install_downloads_checks_unpacks_tests_and_becomes_current() {
        let root = scratch("release");
        let (gh, _) = github_with_files();
        let rec = run(&gh, &root, &Fake::gpu(), Target::Release).unwrap();
        assert_eq!((rec.tag.as_str(), rec.kind, rec.build.as_deref(), rec.build_number), ("v1.0.0", Kind::Release, Some("b12359"), Some(12359)));
        assert_eq!((rec.device, rec.cuda.as_deref()), (Device::Gpu, Some("12.4")));
        let s = installed::server(&root, Backend::LlamaCpp).unwrap();
        assert!(s.binary.ends_with("v1.0.0/llama-server.exe") || s.binary.ends_with("v1.0.0\\llama-server.exe"), "{:?}", s.binary);
        assert!(s.dir.join("cudart64_12.dll").is_file(), "the runtime zip lands beside the server");
        assert!(s.backend_dll.is_some());
        assert!(!root.join("dl").join("llama-b12359-bin-win-cuda-12.4-x64.zip").exists(), "the zips are removed afterwards");
        assert!(!backend_dir(&root, Backend::LlamaCpp).join(".staging-v1.0.0").exists());
        // Installing again is a no-op.
        let asked = gh.asked().len();
        run(&gh, &root, &Fake::gpu(), Target::Release).unwrap();
        assert!(gh.asked().iter().skip(asked).all(|u| u.starts_with("https://api.github.com")), "no download the second time: {:?}", gh.asked());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_download_that_does_not_match_its_published_digest_installs_nothing() {
        let root = scratch("digest");
        let (gh, _) = github_with_files();
        let an = "https://github.com/ggml-org/llama.cpp/releases/download/b12359/llama-b12359-bin-win-cuda-12.4-x64.zip";
        gh.routes.lock().unwrap().insert(an.into(), (200, make_zip(&[("llama-server.exe", b"EVIL")])));
        let e = run(&gh, &root, &Fake::gpu(), Target::Release).unwrap_err();
        assert!(matches!(e, EngineError::Download(marlowe_net::download::DownloadError::Digest { .. })), "{e}");
        assert!(installed::server(&root, Backend::LlamaCpp).is_err());
        assert!(std::fs::read_dir(backend_dir(&root, Backend::LlamaCpp)).map(|d| d.filter_map(Result::ok).all(|e| e.file_name().to_string_lossy().ends_with(".json"))).unwrap_or(true));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_install_that_fails_its_smoke_test_keeps_the_version_it_had() {
        let root = scratch("smoke");
        let (gh, main) = github_with_files();
        run(&gh, &root, &Fake::gpu(), Target::Release).unwrap();
        // A newer build whose binary will not run.
        with_build(&gh, &main);
        let broken = Fake { version: ok(""), ..Fake::gpu() };
        let e = run(&gh, &root, &broken, Target::Build).unwrap_err();
        assert!(matches!(e, EngineError::Smoke(_)), "{e}");
        assert_eq!(installed::current(&root, Backend::LlamaCpp).unwrap().tag, "v1.0.0", "the working version is still current");
        assert!(!tag_dir(&root, Backend::LlamaCpp, "b12560").unwrap().exists());
        assert!(!backend_dir(&root, Backend::LlamaCpp).join(".staging-b12560").exists(), "the staging folder is removed");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_machine_with_no_gpu_installs_as_cpu_with_a_note() {
        let root = scratch("cpu");
        let (gh, _) = github_with_files();
        let cpu = Fake { devices: ok("Available devices:\n"), ..Fake::gpu() };
        let rec = run(&gh, &root, &cpu, Target::Release).unwrap();
        assert_eq!(rec.device, Device::Cpu);
        assert!(rec.note.unwrap().contains("CPU"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_sync_installs_the_newest_build_beside_the_release_then_rollback_and_prune_work() {
        let root = scratch("sync");
        let (gh, main) = github_with_files();
        run(&gh, &root, &Fake::gpu(), Target::Release).unwrap();
        with_build(&gh, &main);
        let rec = run(&gh, &root, &Fake::gpu(), Target::Build).unwrap();
        assert_eq!((rec.tag.as_str(), rec.kind, rec.release.as_deref(), rec.builds_on_release), ("b12560", Kind::Build, Some("v1.0.0"), 201));
        assert_eq!(rec.label(), "v1.0.0 + 201 builds");
        let st = state::load(&root).of(Backend::LlamaCpp);
        assert_eq!((st.current.as_deref(), st.previous.as_deref()), (Some("b12560"), Some("v1.0.0")));
        assert!(tag_dir(&root, Backend::LlamaCpp, "v1.0.0").unwrap().is_dir(), "the release stays for rollback");
        // "Install" with a build already ahead of the release does not swap it back for the release.
        let again = run(&gh, &root, &Fake::gpu(), Target::Release).unwrap();
        assert_eq!(again.tag, "b12560");
        let back = rollback(&root, Backend::LlamaCpp).unwrap();
        assert_eq!(back.tag, "v1.0.0");
        assert_eq!(installed::current(&root, Backend::LlamaCpp).unwrap().tag, "v1.0.0");
        assert_eq!(state::load(&root).of(Backend::LlamaCpp).previous.as_deref(), Some("b12560"));
        // A third install prunes what is neither current nor previous.
        std::fs::create_dir_all(tag_dir(&root, Backend::LlamaCpp, "b1").unwrap()).unwrap();
        std::fs::create_dir_all(backend_dir(&root, Backend::LlamaCpp).join(".staging-old")).unwrap();
        prune(&root, Backend::LlamaCpp);
        assert!(!tag_dir(&root, Backend::LlamaCpp, "b1").unwrap().exists());
        assert!(!backend_dir(&root, Backend::LlamaCpp).join(".staging-old").exists());
        assert!(tag_dir(&root, Backend::LlamaCpp, "b12560").unwrap().is_dir());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_without_a_previous_version_says_so() {
        let root = scratch("noprev");
        let e = rollback(&root, Backend::LlamaCpp).unwrap_err().to_string();
        assert!(e.contains("no previous version"), "{e}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_asset_without_a_digest_is_refused_before_it_is_fetched() {
        let root = scratch("nodigest");
        let (gh, _) = github_with_files();
        let json = String::from_utf8(gh.routes.lock().unwrap().get(&format!("{API}/releases/tags/b12359")).unwrap().1.clone()).unwrap().replace("\"digest\":\"sha256:", "\"digest\":\"md5:");
        gh.routes.lock().unwrap().insert(format!("{API}/releases/tags/b12359"), (200, json.into_bytes()));
        let e = run(&gh, &root, &Fake::gpu(), Target::Release).unwrap_err();
        assert!(matches!(e, EngineError::NoDigest { .. }), "{e}");
        assert!(gh.asked().iter().all(|u| !u.ends_with(".zip")));
        let _ = std::fs::remove_dir_all(&root);
    }
}
