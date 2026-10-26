//! Fetching the memory models: the manifest, the checked downloader, and the redirect policy.
//!
//! The jina embedder is public upstream, at the revision `marlowe-memory` pins. The fine-tuned
//! rerankers exist nowhere else, so they come from [`MODELS_REPO`] on Hugging Face, laid out as
//! `<dir>/<file>` exactly like `models/` (`$MARLOWE_MODELS_URL` replaces the base URL). Every sha256
//! is the one the loading code pins, referenced and never copied; a file that does not match is
//! deleted and the download fails. Each file streams to `<file>.part` with an incremental hash and
//! is renamed into place, so nothing is buffered and a half-written model is never mistaken for one.
//!
//! A redirect is a second destination: at most [`MAX_HOPS`], https only, and only to the Hugging
//! Face family (`huggingface.co`, `*.huggingface.co`, `*.hf.co`) or the configured base's host.
//! The transport is a trait so the policy and the digest handling run against fixtures.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use marlowe_memory::cascade::FUSION_GRAPHS;
use marlowe_memory::cue::dense::embedder;
use marlowe_memory::rerank;
use sha2::{Digest, Sha256};

use crate::paths::{install_dir, EMBEDDER_DIR, MODELS_ENV, RERANKER_DIR};

/// The public Hugging Face model repo holding the fine-tuned rerankers.
pub const MODELS_REPO: &str = "devvexus/marlowe-memory";
/// Environment override for the base URL the `<dir>/<file>` layout hangs off.
pub const MODELS_URL_ENV: &str = "MARLOWE_MODELS_URL";
/// Redirects followed for one file; the next one is refused.
pub const MAX_HOPS: usize = 5;
/// Whether this build offers the GPU rerank cascade with the models.
pub const WITH_CASCADE: bool = cfg!(feature = "cuda");

const HUB: &str = "https://huggingface.co";
/// `jinaai/jina-embeddings-v2-small-en` at the revision the embedder pins.
const JINA_BASE: &str =
    "https://huggingface.co/jinaai/jina-embeddings-v2-small-en/resolve/44e7d1d6caec8c883c2d4b207588504d519788d0";
/// Longest wait for a read before the transfer is called stalled.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// Bytes between progress callbacks.
const PROGRESS_STEP: u64 = 256 * 1024;

/// Sizes of the files in `models/`, for the question and the progress line (the pins guard
/// integrity, not these). The cascade's five graphs each carry the shared tokenizer.
const SIZES: [(&str, &str, u64); 14] = [
    (EMBEDDER_DIR, embedder::MODEL_FILE, 129_809_014),
    (EMBEDDER_DIR, embedder::VOCAB_FILE, 231_589),
    (RERANKER_DIR, rerank::MODEL_FILE, 62_517_846),
    (RERANKER_DIR, rerank::TOKENIZER_FILE, 711_661),
    ("ms-marco-MiniLM-L-6-v2-ft-session-j", rerank::MODEL_FILE, 90_988_005),
    ("ms-marco-MiniLM-L-6-v2-ft-session-j", rerank::TOKENIZER_FILE, 711_661),
    ("ms-marco-MiniLM-L-2-v2-ft-w1", rerank::MODEL_FILE, 62_517_846),
    ("ms-marco-MiniLM-L-2-v2-ft-w1", rerank::TOKENIZER_FILE, 711_661),
    ("ms-marco-MiniLM-L-4-v2-ft-w1", rerank::MODEL_FILE, 76_752_925),
    ("ms-marco-MiniLM-L-4-v2-ft-w1", rerank::TOKENIZER_FILE, 711_661),
    ("ms-marco-MiniLM-L-6-v2-ft-w1", rerank::MODEL_FILE, 90_988_005),
    ("ms-marco-MiniLM-L-6-v2-ft-w1", rerank::TOKENIZER_FILE, 711_661),
    ("ms-marco-MiniLM-L-12-v2-ft-w1", rerank::MODEL_FILE, 133_694_120),
    ("ms-marco-MiniLM-L-12-v2-ft-w1", rerank::TOKENIZER_FILE, 711_661),
];

/// One file to fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub dir: &'static str,
    pub file: &'static str,
    pub sha256: &'static str,
    pub bytes: u64,
    pub url: String,
}

impl Entry {
    /// `<dir>/<file>`, as the layout and the messages spell it.
    pub fn name(&self) -> String {
        format!("{}/{}", self.dir, self.file)
    }
}

/// The base URL for the fine-tuned models: `$MARLOWE_MODELS_URL`, else the Hugging Face repo.
pub fn base_url() -> String {
    base_from(std::env::var(MODELS_URL_ENV).ok())
}

fn base_from(env: Option<String>) -> String {
    let env = env.map(|v| v.trim().trim_end_matches('/').to_string());
    env.filter(|v| !v.is_empty()).unwrap_or_else(|| format!("{HUB}/{MODELS_REPO}/resolve/main"))
}

/// The files to fetch: the shipped embedder and reranker, plus the five cascade graphs when
/// `cascade`. `base` is where the fine-tuned ones live (see [`base_url`]).
pub fn manifest(cascade: bool, base: &str) -> Vec<Entry> {
    let base = base.trim_end_matches('/');
    let size = |dir: &str, file: &str| SIZES.iter().find(|s| s.0 == dir && s.1 == file).map_or(0, |s| s.2);
    let tuned = |dir: &'static str, file: &'static str, sha256: &'static str| Entry {
        dir,
        file,
        sha256,
        bytes: size(dir, file),
        url: format!("{base}/{dir}/{file}"),
    };
    // The embedder keeps its upstream layout: no `<dir>` in the URL.
    let jina = |file: &'static str, sha256: &'static str| Entry {
        dir: EMBEDDER_DIR,
        file,
        sha256,
        bytes: size(EMBEDDER_DIR, file),
        url: format!("{JINA_BASE}/{file}"),
    };
    let mut out = vec![
        jina(embedder::MODEL_FILE, embedder::MODEL_SHA256),
        jina(embedder::VOCAB_FILE, embedder::VOCAB_SHA256),
        tuned(RERANKER_DIR, rerank::MODEL_FILE, rerank::MODEL_SHA256),
        tuned(RERANKER_DIR, rerank::TOKENIZER_FILE, rerank::TOKENIZER_SHA256),
    ];
    if cascade {
        for (dir, sha) in &FUSION_GRAPHS[1..] {
            out.push(tuned(dir, rerank::MODEL_FILE, sha));
            out.push(tuned(dir, rerank::TOKENIZER_FILE, rerank::TOKENIZER_SHA256));
        }
    }
    out
}

/// Bytes of the entries.
pub fn total_bytes(entries: &[Entry]) -> u64 {
    entries.iter().map(|e| e.bytes).sum()
}

/// Where fetched models go: `$MARLOWE_MODELS` when set (it wins at load, too), else
/// `<install dir>\models`, which `resolve_models_dir` finds on every later launch.
pub fn dest_dir() -> Option<PathBuf> {
    match std::env::var_os(MODELS_ENV).filter(|v| !v.is_empty()) {
        Some(env) => Some(PathBuf::from(env)),
        None => install_dir().map(|d| d.join("models")),
    }
}

/// The entries whose file is not at `dest` at all (no hashing: this prices a question).
pub fn absent(entries: &[Entry], dest: &Path) -> Vec<Entry> {
    entries.iter().filter(|e| !dest.join(e.dir).join(e.file).is_file()).cloned().collect()
}

/// Why a download stopped.
#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    /// A 404 or 410: the file is not at its URL (the fine-tunes before they are uploaded).
    #[error("not published at {url}")]
    NotPublished { url: String },
    /// A 401 or 403. Hugging Face answers 401 for a repo that does not exist, as for a private one.
    #[error("not public at {url} (HTTP {status})")]
    NotPublic { url: String, status: u16 },
    #[error("{url} answered HTTP {status}")]
    Http { url: String, status: u16 },
    #[error("could not fetch {url}: {detail}")]
    Network { url: String, detail: String },
    /// A redirect (or the URL itself) the policy does not allow.
    #[error("refused {url}: {detail}")]
    Refused { url: String, detail: String },
    #[error("{name}: sha256 {got} does not match the pinned {want}; the file was deleted")]
    Digest { name: String, got: String, want: &'static str },
    #[error("{path}: {detail}")]
    Io { path: String, detail: String },
}

fn io_err(path: &Path, e: std::io::Error) -> DownloadError {
    DownloadError::Io { path: path.display().to_string(), detail: e.to_string() }
}

/// One response. A redirect carries its `Location`; a body is read only for a 200.
pub struct Reply {
    pub status: u16,
    pub redirect_to: Option<String>,
    pub body: Box<dyn Read + Send>,
}

/// One GET that follows no redirect. The real one is [`Net`]; tests supply fixtures.
pub trait Transport {
    fn get(&self, url: &str) -> Result<Reply, String>;
}

/// The workspace's rustls client.
pub struct Net;

struct Body(marlowe_net::ResponseStream);

impl Read for Body {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.body().read(buf)
    }
}

impl Transport for Net {
    fn get(&self, url: &str) -> Result<Reply, String> {
        let target = marlowe_net::Target::parse(url).map_err(|e| e.to_string())?;
        let headers = marlowe_net::Headers { user_agent: "marlowe", accept: "*/*" };
        let r = marlowe_net::shared().get_streaming(&target, &headers, READ_TIMEOUT).map_err(|e| e.to_string())?;
        Ok(Reply { status: r.status, redirect_to: r.redirect_to.clone(), body: Box::new(Body(r)) })
    }
}

/// Where a request may go.
#[derive(Debug, Clone)]
pub struct Policy {
    /// The configured base URL's host, allowed besides the Hugging Face family.
    extra_host: Option<String>,
}

impl Policy {
    pub fn for_base(base: &str) -> Self {
        let host = marlowe_net::Target::parse(base).ok().map(|t| t.host);
        Self { extra_host: host.filter(|h| !hugging_face(h)) }
    }

    fn host_ok(&self, host: &str, port: u16) -> bool {
        (port == 443 && hugging_face(host)) || self.extra_host.as_deref() == Some(host)
    }

    /// `url`'s host if it is https and allowed.
    fn check(&self, url: &str) -> Result<String, DownloadError> {
        let refused = |detail: String| DownloadError::Refused { url: url.to_string(), detail };
        let t = marlowe_net::Target::parse(url).map_err(|e| match e {
            marlowe_net::FetchError::Unsupported { detail, .. } => refused(detail),
            other => refused(other.to_string()),
        })?;
        if !self.host_ok(&t.host, t.port) {
            return Err(refused(format!("{}:{} is not a Hugging Face host", t.host, t.port)));
        }
        Ok(t.host)
    }
}

/// `huggingface.co`, `hf.co` or a subdomain of either.
fn hugging_face(host: &str) -> bool {
    let under = |root: &str| host == root || host.strip_suffix(root).is_some_and(|p| p.ends_with('.'));
    under("huggingface.co") || under("hf.co")
}

/// An absolute https `Location`, or a path on the current host; anything else is refused.
fn resolve(host: &str, location: &str) -> Result<String, DownloadError> {
    let refused = |detail: &str| DownloadError::Refused { url: location.to_string(), detail: detail.to_string() };
    if location.starts_with("//") {
        return Err(refused("a scheme-relative redirect"));
    }
    if location.starts_with('/') {
        return Ok(format!("https://{host}{location}"));
    }
    if location.starts_with("https://") || location.starts_with("http://") {
        return Ok(location.to_string());
    }
    Err(refused("an unusable redirect"))
}

/// Opens `url`, following at most [`MAX_HOPS`] allowed redirects.
fn open(t: &dyn Transport, policy: &Policy, url: &str) -> Result<Reply, DownloadError> {
    let mut url = url.to_string();
    for hop in 0..=MAX_HOPS {
        let host = policy.check(&url)?;
        let reply = t.get(&url).map_err(|detail| DownloadError::Network { url: url.clone(), detail })?;
        let redirect = matches!(reply.status, 301 | 302 | 303 | 307 | 308);
        match (redirect, reply.redirect_to.as_deref()) {
            (true, Some(loc)) => {
                if hop == MAX_HOPS {
                    return Err(DownloadError::Refused { url, detail: format!("more than {MAX_HOPS} redirects") });
                }
                url = resolve(&host, loc)?;
            }
            (true, None) => return Err(DownloadError::Http { url, status: reply.status }),
            _ => return Ok(reply),
        }
    }
    unreachable!("the last hop returns")
}

/// Progress through a download.
#[derive(Debug, Clone, Copy)]
pub struct Progress<'a> {
    /// The file being fetched, `<dir>/<file>`.
    pub file: &'a str,
    /// Bytes of the whole manifest in place so far, files already present included.
    pub done: u64,
    pub total: u64,
}

fn sha256_of(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(format!("{:x}", h.finalize()));
        }
        h.update(&buf[..n]);
    }
}

/// Fetches every entry into `dest\<dir>\<file>`, skipping files already there with the right
/// hash. Stops at the first failure; whatever finished stays.
pub fn download(
    entries: &[Entry],
    dest: &Path,
    t: &dyn Transport,
    policy: &Policy,
    on: &mut dyn FnMut(Progress<'_>),
) -> Result<(), DownloadError> {
    let total = total_bytes(entries);
    let mut done = 0u64;
    for e in entries {
        let name = e.name();
        let path = dest.join(e.dir).join(e.file);
        if path.is_file() && sha256_of(&path).is_ok_and(|h| h == e.sha256) {
            done += e.bytes;
            on(Progress { file: &name, done, total });
            continue;
        }
        std::fs::create_dir_all(dest.join(e.dir)).map_err(|err| io_err(&dest.join(e.dir), err))?;
        let part = path.with_file_name(format!("{}.part", e.file));
        let got = fetch_one(e, &part, t, policy, &mut |n| on(Progress { file: &name, done: done + n, total }));
        let got = match got {
            Ok(h) => h,
            Err(err) => {
                let _ = std::fs::remove_file(&part);
                return Err(err);
            }
        };
        if got != e.sha256 {
            let _ = std::fs::remove_file(&part);
            return Err(DownloadError::Digest { name, got, want: e.sha256 });
        }
        std::fs::rename(&part, &path).map_err(|err| io_err(&path, err))?;
        done += e.bytes;
        on(Progress { file: &name, done, total });
    }
    Ok(())
}

/// Streams `e` into `part`; returns the sha256 of what arrived. `on` gets the bytes so far.
fn fetch_one(
    e: &Entry,
    part: &Path,
    t: &dyn Transport,
    policy: &Policy,
    on: &mut dyn FnMut(u64),
) -> Result<String, DownloadError> {
    let mut reply = open(t, policy, &e.url)?;
    match reply.status {
        200 => {}
        404 | 410 => return Err(DownloadError::NotPublished { url: e.url.clone() }),
        status @ (401 | 403) => return Err(DownloadError::NotPublic { url: e.url.clone(), status }),
        status => return Err(DownloadError::Http { url: e.url.clone(), status }),
    }
    let mut out = std::fs::File::create(part).map_err(|err| io_err(part, err))?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let (mut n, mut reported) = (0u64, 0u64);
    loop {
        let k = reply
            .body
            .read(&mut buf)
            .map_err(|err| DownloadError::Network { url: e.url.clone(), detail: err.to_string() })?;
        if k == 0 {
            break;
        }
        hash.update(&buf[..k]);
        out.write_all(&buf[..k]).map_err(|err| io_err(part, err))?;
        n += k as u64;
        if n - reported >= PROGRESS_STEP {
            reported = n;
            on(n);
        }
    }
    out.flush().map_err(|err| io_err(part, err))?;
    Ok(format!("{:x}", hash.finalize()))
}

/// Megabytes as shown to the user (decimal, rounded).
pub fn megabytes(bytes: u64) -> u64 {
    (bytes + 500_000) / 1_000_000
}

#[cfg(test)]
mod tests;
