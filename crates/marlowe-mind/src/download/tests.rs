//! The manifest against the pins, and the downloader against fixtures. No network.

use std::collections::HashMap;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Mutex;

use marlowe_memory::cascade::{Cascade, FUSION_GRAPHS};
use sha2::{Digest, Sha256};

use super::*;

const BASE: &str = "https://huggingface.co/devvexus/marlowe-memory/resolve/main";

fn scratch(tag: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("marlowe-dl-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// An entry for `bytes` with its true digest (leaked: pins are `&'static str`).
fn entry(dir: &'static str, file: &'static str, bytes: &[u8], url: &str) -> Entry {
    Entry { dir, file, sha256: Box::leak(hex(bytes).into_boxed_str()), bytes: bytes.len() as u64, url: url.into() }
}

enum Route {
    Body(Vec<u8>),
    /// Status, `Location`.
    Redirect(u16, &'static str),
    Status(u16),
    /// Some bytes, then the connection dies.
    Broken(Vec<u8>),
}

struct Dies(Cursor<Vec<u8>>);

impl Read for Dies {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.0.read(buf)? {
            0 => Err(std::io::Error::other("connection reset")),
            n => Ok(n),
        }
    }
}

#[derive(Default)]
struct Fake {
    routes: Mutex<HashMap<String, Route>>,
    asked: Mutex<Vec<String>>,
}

impl Fake {
    fn with(self, url: &str, r: Route) -> Self {
        self.routes.lock().unwrap().insert(url.into(), r);
        self
    }
    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

impl Transport for Fake {
    fn get(&self, url: &str) -> Result<Reply, String> {
        self.asked.lock().unwrap().push(url.into());
        let routes = self.routes.lock().unwrap();
        let reply = |status, redirect_to: Option<String>, body: Box<dyn Read + Send>| Ok(Reply { status, redirect_to, body });
        match routes.get(url) {
            Some(Route::Body(b)) => reply(200, None, Box::new(Cursor::new(b.clone()))),
            Some(Route::Redirect(s, loc)) => reply(*s, Some((*loc).into()), Box::new(std::io::empty())),
            Some(Route::Status(s)) => reply(*s, None, Box::new(std::io::empty())),
            Some(Route::Broken(b)) => reply(200, None, Box::new(Dies(Cursor::new(b.clone())))),
            None => Err(format!("no route for {url}")),
        }
    }
}

fn run(fake: &Fake, e: &Entry, dest: &std::path::Path) -> Result<(), DownloadError> {
    download(std::slice::from_ref(e), dest, fake, &Policy::for_base(BASE), &mut |_| {})
}

// ---------------------------------------------------------------- the manifest

#[test]
fn the_shipped_manifest_is_the_pinned_pair_and_costs_about_193_megabytes() {
    let m = manifest(false, BASE);
    let got: Vec<(&str, &str, &str)> = m.iter().map(|e| (e.dir, e.file, e.sha256)).collect();
    assert_eq!(
        got,
        [
            (EMBEDDER_DIR, "model.onnx", embedder::MODEL_SHA256),
            (EMBEDDER_DIR, "vocab.txt", embedder::VOCAB_SHA256),
            (RERANKER_DIR, "model.onnx", rerank::MODEL_SHA256),
            (RERANKER_DIR, "tokenizer.json", rerank::TOKENIZER_SHA256),
        ]
    );
    assert!(m.iter().all(|e| e.bytes > 0));
    assert_eq!(megabytes(total_bytes(&m)), 193);
}

#[test]
fn the_embedder_comes_from_upstream_and_the_fine_tunes_from_the_models_repo() {
    let m = manifest(false, &format!("{BASE}/"));
    assert_eq!(
        m[0].url,
        "https://huggingface.co/jinaai/jina-embeddings-v2-small-en/resolve/44e7d1d6caec8c883c2d4b207588504d519788d0/model.onnx"
    );
    assert!(m[1].url.ends_with("/resolve/44e7d1d6caec8c883c2d4b207588504d519788d0/vocab.txt"));
    assert_eq!(m[2].url, format!("{BASE}/{RERANKER_DIR}/model.onnx"));
    assert_eq!(m[3].url, format!("{BASE}/{RERANKER_DIR}/tokenizer.json"), "no doubled slash");
    assert_eq!(MODELS_REPO, "devvexus/marlowe-memory");
}

#[test]
fn the_cascade_adds_the_five_other_graphs_each_with_its_pinned_digest() {
    let m = manifest(true, BASE);
    assert_eq!(m.len(), 4 + 10);
    let extra: Vec<&str> = m[4..].iter().step_by(2).map(|e| e.dir).collect();
    assert_eq!(extra, Cascade::required_dirs().collect::<Vec<_>>());
    for (dir, sha) in &FUSION_GRAPHS[1..] {
        let model = m.iter().find(|e| e.dir == *dir && e.file == "model.onnx").expect("a graph per dir");
        assert_eq!(model.sha256, *sha);
        assert!(model.bytes > 0, "{dir} has a size");
        let tok = m.iter().find(|e| e.dir == *dir && e.file == "tokenizer.json").expect("a tokenizer per dir");
        assert_eq!(tok.sha256, rerank::TOKENIZER_SHA256);
    }
    assert_eq!(megabytes(total_bytes(&m)), 652, "the shipped pair plus the five graphs");
}

/// The sizes are what is in `models/` on a machine that has it (those files are git-ignored).
#[test]
fn the_sizes_are_the_files_in_models() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("models");
    if !dir.join(RERANKER_DIR).is_dir() {
        eprintln!("SKIP: no models/ here");
        return;
    }
    for e in manifest(true, BASE) {
        let on_disk = std::fs::metadata(dir.join(e.dir).join(e.file)).map(|m| m.len());
        if let Ok(len) = on_disk {
            assert_eq!(e.bytes, len, "{}", e.name());
        }
    }
}

#[test]
fn the_base_url_defaults_to_the_repo_and_the_environment_replaces_it() {
    assert_eq!(base_from(None), BASE);
    assert_eq!(base_from(Some("  ".into())), BASE);
    assert_eq!(base_from(Some(" https://mirror.example/m/ ".into())), "https://mirror.example/m");
    assert_eq!(megabytes(193_270_110), 193);
    assert_eq!(megabytes(499_999), 0);
    assert_eq!(megabytes(500_000), 1);
}

// ---------------------------------------------------------------- the downloader

#[test]
fn a_download_streams_to_part_then_renames_and_reports_progress() {
    let dest = scratch("ok");
    let body = vec![7u8; 700_000];
    let e = entry("dir", "model.onnx", &body, "https://huggingface.co/x/model.onnx");
    let fake = Fake::default().with(&e.url, Route::Body(body.clone()));
    let mut seen = Vec::new();
    download(std::slice::from_ref(&e), &dest, &fake, &Policy::for_base(BASE), &mut |p| seen.push((p.done, p.total)))
        .unwrap();
    assert_eq!(std::fs::read(dest.join("dir").join("model.onnx")).unwrap(), body);
    assert!(!dest.join("dir").join("model.onnx.part").exists(), "renamed, not copied");
    assert_eq!(seen.last(), Some(&(700_000, 700_000)));
    assert!(seen.len() >= 3 && seen.windows(2).all(|w| w[0].0 <= w[1].0), "{seen:?}");
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn a_digest_mismatch_deletes_the_part_and_fails_loudly() {
    let dest = scratch("bad");
    let good = b"the real model".to_vec();
    let e = entry("dir", "model.onnx", &good, "https://huggingface.co/x/model.onnx");
    let fake = Fake::default().with(&e.url, Route::Body(b"a different file".to_vec()));
    let err = run(&fake, &e, &dest).unwrap_err();
    assert!(matches!(err, DownloadError::Digest { .. }), "{err}");
    let msg = err.to_string();
    assert!(msg.contains("dir/model.onnx") && msg.contains("does not match the pinned") && msg.contains("deleted"), "{msg}");
    assert!(!dest.join("dir").join("model.onnx.part").exists(), "the .part is gone");
    assert!(!dest.join("dir").join("model.onnx").exists(), "nothing unverified is left in place");
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn a_file_already_there_with_the_right_hash_is_not_fetched() {
    let dest = scratch("skip");
    let body = b"already here".to_vec();
    let e = entry("dir", "model.onnx", &body, "https://huggingface.co/x/model.onnx");
    std::fs::create_dir_all(dest.join("dir")).unwrap();
    std::fs::write(dest.join("dir").join("model.onnx"), &body).unwrap();
    let fake = Fake::default();
    let mut last = None;
    download(std::slice::from_ref(&e), &dest, &fake, &Policy::for_base(BASE), &mut |p| last = Some(p.done)).unwrap();
    assert!(fake.asked().is_empty(), "no request for a good file");
    assert_eq!(last, Some(body.len() as u64), "it still counts toward the progress");

    // The same name with other contents is replaced.
    std::fs::write(dest.join("dir").join("model.onnx"), b"corrupt").unwrap();
    let fake = Fake::default().with(&e.url, Route::Body(body.clone()));
    run(&fake, &e, &dest).unwrap();
    assert_eq!(fake.asked().len(), 1);
    assert_eq!(std::fs::read(dest.join("dir").join("model.onnx")).unwrap(), body);
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn a_missing_file_says_where_it_is_not_published() {
    let dest = scratch("404");
    let e = entry("dir", "model.onnx", b"x", "https://huggingface.co/devvexus/marlowe-memory/resolve/main/dir/model.onnx");
    let fake = Fake::default().with(&e.url, Route::Status(404));
    let err = run(&fake, &e, &dest).unwrap_err();
    assert!(matches!(err, DownloadError::NotPublished { .. }));
    assert_eq!(err.to_string(), format!("not published at {}", e.url));
    assert!(!dest.join("dir").join("model.onnx.part").exists());

    let fake = Fake::default().with(&e.url, Route::Status(503));
    assert!(matches!(run(&fake, &e, &dest).unwrap_err(), DownloadError::Http { status: 503, .. }));

    // Hugging Face answers 401 for a repo that is not there, as for a private one.
    let fake = Fake::default().with(&e.url, Route::Status(401));
    let err = run(&fake, &e, &dest).unwrap_err();
    assert_eq!(err.to_string(), format!("not public at {} (HTTP 401)", e.url));
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn a_connection_that_dies_midway_leaves_no_part_and_no_model() {
    let dest = scratch("reset");
    let e = entry("dir", "model.onnx", b"complete contents", "https://huggingface.co/x/model.onnx");
    let fake = Fake::default().with(&e.url, Route::Broken(b"complete".to_vec()));
    let err = run(&fake, &e, &dest).unwrap_err();
    assert!(matches!(err, DownloadError::Network { .. }), "{err}");
    assert!(std::fs::read_dir(dest.join("dir")).unwrap().next().is_none());
    let _ = std::fs::remove_dir_all(&dest);
}

// ---------------------------------------------------------------- redirects

const START: &str = "https://huggingface.co/devvexus/marlowe-memory/resolve/main/dir/model.onnx";

fn through(fake: Fake, body: &[u8]) -> Result<Vec<String>, DownloadError> {
    let dest = scratch("hops");
    let e = entry("dir", "model.onnx", body, START);
    let r = run(&fake, &e, &dest);
    let _ = std::fs::remove_dir_all(&dest);
    r.map(|()| fake.asked())
}

#[test]
fn a_hugging_face_cdn_redirect_is_followed() {
    let fake = Fake::default()
        .with(START, Route::Redirect(302, "https://cdn-lfs.hf.co/repos/ab/cd/file"))
        .with("https://cdn-lfs.hf.co/repos/ab/cd/file", Route::Redirect(307, "https://cas-bridge.xethub.hf.co/x"))
        .with("https://cas-bridge.xethub.hf.co/x", Route::Redirect(302, "/api/resolve-cache/y"))
        .with("https://cas-bridge.xethub.hf.co/api/resolve-cache/y", Route::Redirect(302, "https://cdn-lfs-us-1.huggingface.co/z"))
        .with("https://cdn-lfs-us-1.huggingface.co/z", Route::Body(b"weights".to_vec()));
    let asked = through(fake, b"weights").unwrap();
    assert_eq!(asked.len(), 5, "{asked:?}");
}

#[test]
fn a_foreign_host_is_refused_before_it_is_asked() {
    for host in ["https://evil.example/model.onnx", "https://evilhf.co/m", "https://huggingface.co.evil.example/m", "https://hf.co.attacker.net/m"] {
        let leak: &'static str = Box::leak(host.to_string().into_boxed_str());
        let fake = Fake::default().with(START, Route::Redirect(302, leak)).with(host, Route::Body(b"weights".to_vec()));
        let err = through(fake, b"weights").unwrap_err();
        assert!(matches!(err, DownloadError::Refused { .. }), "{host}: {err}");
    }
    // The first request is held to the same rule.
    let dest = scratch("first");
    let e = entry("dir", "model.onnx", b"w", "https://evil.example/model.onnx");
    let fake = Fake::default().with(&e.url, Route::Body(b"w".to_vec()));
    assert!(matches!(run(&fake, &e, &dest).unwrap_err(), DownloadError::Refused { .. }));
    assert!(fake.asked().is_empty());
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn plaintext_and_scheme_relative_redirects_are_refused() {
    for loc in ["http://huggingface.co/model.onnx", "//huggingface.co/model.onnx", "model.onnx"] {
        let fake = Fake::default().with(START, Route::Redirect(302, loc));
        let err = through(fake, b"w").unwrap_err();
        assert!(matches!(err, DownloadError::Refused { .. }), "{loc}: {err}");
    }
    let err = Policy::for_base(BASE).check("http://huggingface.co/x").unwrap_err().to_string();
    assert_eq!(err.matches("http://huggingface.co/x").count(), 1, "the URL is said once: {err}");
    let fake = Fake::default().with(START, Route::Status(302));
    assert!(matches!(through(fake, b"w").unwrap_err(), DownloadError::Http { status: 302, .. }));
}

#[test]
fn five_hops_pass_and_a_sixth_is_refused() {
    let hop = |i: usize| -> &'static str { Box::leak(format!("https://cdn-lfs.hf.co/hop{i}").into_boxed_str()) };
    let chain = |n: usize| {
        let mut f = Fake::default().with(START, Route::Redirect(302, hop(1)));
        for i in 1..n {
            f = f.with(hop(i), Route::Redirect(302, hop(i + 1)));
        }
        f.with(hop(n), Route::Body(b"w".to_vec()))
    };
    assert_eq!(through(chain(MAX_HOPS), b"w").unwrap().len(), MAX_HOPS + 1);
    let err = through(chain(MAX_HOPS + 1), b"w").unwrap_err();
    assert!(matches!(err, DownloadError::Refused { .. }) && err.to_string().contains("more than 5 redirects"), "{err}");
}

#[test]
fn the_configured_base_host_is_allowed_besides_the_hugging_face_family() {
    let p = Policy::for_base("https://models.example.org/marlowe");
    assert!(p.check("https://models.example.org/marlowe/dir/model.onnx").is_ok());
    assert!(p.check("https://cdn-lfs.huggingface.co/x").is_ok());
    assert!(p.check("https://other.example.org/x").is_err());
    let hf = Policy::for_base(BASE);
    assert!(hf.check("https://huggingface.co/x").is_ok() && hf.check("https://a.b.hf.co/x").is_ok());
    assert!(hf.check("https://huggingface.co:8443/x").is_err(), "only the https port for the public hosts");
    assert!(hf.check("https://user@huggingface.co/x").is_err());
    assert!(hf.check("http://huggingface.co/x").is_err());
}
