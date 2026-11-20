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
//! The streaming, the policy and the digest check are `marlowe_net::download`, shared with the
//! backend installer; the transport is a trait so they run against fixtures.

use std::path::{Path, PathBuf};

use marlowe_memory::cascade::FUSION_GRAPHS;
use marlowe_memory::cue::dense::embedder;
use marlowe_memory::rerank;

use crate::paths::{install_dir, EMBEDDER_DIR, MODELS_ENV, RERANKER_DIR};

/// The public Hugging Face model repo holding the fine-tuned rerankers.
pub const MODELS_REPO: &str = "devvexus/marlowe-memory";
/// Environment override for the base URL the `<dir>/<file>` layout hangs off.
pub const MODELS_URL_ENV: &str = "MARLOWE_MODELS_URL";
/// Whether this build offers the GPU rerank cascade with the models.
pub const WITH_CASCADE: bool = cfg!(feature = "cuda");

const HUB: &str = "https://huggingface.co";
/// `jinaai/jina-embeddings-v2-small-en` at the revision the embedder pins.
const JINA_BASE: &str =
    "https://huggingface.co/jinaai/jina-embeddings-v2-small-en/resolve/44e7d1d6caec8c883c2d4b207588504d519788d0";

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

pub use marlowe_net::download::{DownloadError, Net, Reply, Transport, MAX_HOPS};

/// Where a request may go: the Hugging Face family, and the configured base's host.
#[derive(Debug, Clone)]
pub struct Policy(marlowe_net::download::Policy);

impl Policy {
    pub fn for_base(base: &str) -> Self {
        let host = marlowe_net::Target::parse(base).ok().map(|t| t.host);
        let hf = marlowe_net::download::Policy::new(&["huggingface.co", "hf.co"], &[]);
        let extra = host.filter(|h| !hf.under_root(h));
        Self(hf.with_any_port(extra))
    }

    /// `url`'s host if it is https and allowed.
    #[cfg(test)]
    fn check(&self, url: &str) -> Result<String, DownloadError> {
        self.0.check(url)
    }
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
        marlowe_net::download::fetch_verified(&e.url, &path, e.sha256, &name, t, &policy.0, &mut |n| {
            on(Progress { file: &name, done: done + n, total })
        })?;
        done += e.bytes;
        on(Progress { file: &name, done, total });
    }
    Ok(())
}

/// Megabytes as shown to the user (decimal, rounded).
pub fn megabytes(bytes: u64) -> u64 {
    (bytes + 500_000) / 1_000_000
}

#[cfg(test)]
mod tests;
