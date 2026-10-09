//! Lazily loaded ONNX models (CPU only) and the degraded-mode flag.
//!
//! Each model loads on first need and unloads after `idle` unused. A missing model, or one that
//! fails its pinned digest, puts the service in degraded mode for the rest of the process:
//! lexical recall only, no auto-injection. The reason is kept and reported by `stats`. Other
//! load failures (a file locked by a scanner, an ONNX session error) fail that call only and are
//! retried on the next need. No embedding cache: belief vectors persist in `vecfile`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use marlowe_memory::cue::dense::embedder::{self, EmbedError, Embedder};
use marlowe_memory::rerank::{self, CrossEncoder, RerankError};

use crate::paths::{EMBEDDER_DIR, RERANKER_DIR};

/// One embedder worker: the queue runs at idle priority and RAM matters more than throughput.
const EMBED_WORKERS: usize = 1;

pub(crate) struct Models {
    dir: PathBuf,
    idle: Duration,
    embedder: Option<(Embedder, Instant)>,
    reranker: Option<(CrossEncoder, Instant)>,
    degraded: Option<String>,
}

impl Models {
    /// Checks the files exist; loads nothing.
    pub fn new(dir: PathBuf, idle: Duration) -> Self {
        let required = [
            (EMBEDDER_DIR, embedder::MODEL_FILE),
            (EMBEDDER_DIR, embedder::VOCAB_FILE),
            (RERANKER_DIR, rerank::MODEL_FILE),
            (RERANKER_DIR, rerank::TOKENIZER_FILE),
        ];
        let missing: Vec<String> = required
            .iter()
            .filter(|(sub, file)| !dir.join(sub).join(file).is_file())
            .map(|(sub, file)| format!("{sub}\\{file}"))
            .collect();
        let degraded = (!missing.is_empty()).then(|| {
            format!(
                "models missing from {}: {}. Recall is lexical only and auto-injection is off",
                dir.display(),
                missing.join(", ")
            )
        });
        if let Some(reason) = &degraded {
            eprintln!("marlowe-mind: degraded: {reason}");
        }
        Self { dir, idle, embedder: None, reranker: None, degraded }
    }

    /// Why the service is degraded, if it is.
    pub fn degraded(&self) -> Option<&str> {
        self.degraded.as_deref()
    }

    fn fail(&mut self, what: &str, err: String) -> String {
        let reason = format!("{what} failed to load: {err}. Recall is lexical only and auto-injection is off");
        eprintln!("marlowe-mind: degraded: {reason}");
        self.degraded = Some(reason.clone());
        reason
    }

    /// The embedder, loading it if needed.
    pub fn embedder(&mut self) -> Result<&mut Embedder, String> {
        if let Some(r) = &self.degraded {
            return Err(r.clone());
        }
        if self.embedder.is_none() {
            let dir = self.dir.join(EMBEDDER_DIR);
            match Embedder::load(&dir, EMBED_WORKERS, None) {
                Ok(e) => self.embedder = Some((e, Instant::now())),
                Err(e) if embed_permanent(&e) => return Err(self.fail("the embedder", e.to_string())),
                Err(e) => return Err(format!("the embedder did not load (will retry): {e}")),
            }
        }
        let slot = self.embedder.as_mut().expect("loaded above");
        slot.1 = Instant::now();
        Ok(&mut slot.0)
    }

    /// The cross-encoder, loading it if needed.
    pub fn reranker(&mut self) -> Result<&mut CrossEncoder, String> {
        if let Some(r) = &self.degraded {
            return Err(r.clone());
        }
        if self.reranker.is_none() {
            let dir = self.dir.join(RERANKER_DIR);
            match CrossEncoder::load(&dir) {
                Ok(c) => self.reranker = Some((c, Instant::now())),
                Err(e) if rerank_permanent(&e) => return Err(self.fail("the cross-encoder", e.to_string())),
                Err(e) => return Err(format!("the cross-encoder did not load (will retry): {e}")),
            }
        }
        let slot = self.reranker.as_mut().expect("loaded above");
        slot.1 = Instant::now();
        Ok(&mut slot.0)
    }

    /// Drops models unused for `idle`.
    pub fn unload_idle(&mut self, now: Instant) {
        if self.embedder.as_ref().is_some_and(|(_, t)| now.duration_since(*t) >= self.idle) {
            self.embedder = None;
        }
        if self.reranker.as_ref().is_some_and(|(_, t)| now.duration_since(*t) >= self.idle) {
            self.reranker = None;
        }
    }

    /// When the next model falls idle.
    pub fn next_unload(&self) -> Option<Instant> {
        let e = self.embedder.as_ref().map(|(_, t)| *t + self.idle);
        let r = self.reranker.as_ref().map(|(_, t)| *t + self.idle);
        match (e, r) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Names of the resident models.
    pub fn loaded(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.embedder.is_some() {
            out.push(EMBEDDER_DIR.to_string());
        }
        if self.reranker.is_some() {
            out.push(RERANKER_DIR.to_string());
        }
        out
    }
}

/// Load failures that will not go away by retrying: the files are absent or not the pinned ones.
fn embed_permanent(e: &EmbedError) -> bool {
    matches!(e, EmbedError::Missing { .. } | EmbedError::DigestMismatch { .. } | EmbedError::Vocab { .. })
}

fn rerank_permanent(e: &RerankError) -> bool {
    matches!(
        e,
        RerankError::Missing { .. }
            | RerankError::DigestMismatch { .. }
            | RerankError::SupersededGraph { .. }
            | RerankError::Vocab { .. }
    )
}
