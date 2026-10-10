//! The embedder: pinned model, pinned vocabulary, pinned runtime, no defaults.
//!
//! Every load-time refusal here follows CLAUDE.md's rule — *prefer a load-time error to a
//! sensible default* — and each one guards a failure that is **silent** rather than loud. A
//! wrong model, a wrong vocabulary, or a wrong sequence length all produce a 512-dimension unit
//! vector that scores, ranks, and yields a number. None of them crashes.
//!
//! **Threads are pinned to 1 inside the graph**, and parallelism is at the *text* level: each
//! forward pass runs single-threaded on one worker and results are reassembled by input index.
//! `docs/design/spike-2026-08-04-embedder.md` measured that this makes worker count irrelevant
//! to output — identical digests at 1, 2 and 8 workers, on both engines and both models. That is
//! what lets throughput be tuned without touching a published number.

use std::path::{Path, PathBuf};

use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Value;
use sha2::{Digest, Sha256};

use crate::cue::dense::cache::{CacheIdentity, EmbeddingCache, EMBEDDER_VERSION};
use crate::cue::dense::tokenizer::{encode, Vocab};
use crate::cue::dense::{mean_pool_and_normalize, DIMENSIONS, MAX_SEQ_LEN};

/// `jinaai/jina-embeddings-v2-small-en` at revision `44e7d1d6…`, `model.onnx`.
///
/// Pinned here as well as in `tools/fetch_model.py` deliberately: the fetcher guards the
/// download, this guards the *load*. A file swapped after download passes the first and is
/// caught by the second.
pub const MODEL_SHA256: &str = "974fdefe71fc9889258f569132b35acae6278874c8d09dbdf7806d23ad0b4497";

/// The same revision's `vocab.txt`.
pub const VOCAB_SHA256: &str = "109753d618dbb576a35112f9c20ef35cf3517d46106175bcf010c986a4bef1df";

pub const MODEL_FILE: &str = "model.onnx";
pub const VOCAB_FILE: &str = "vocab.txt";

/// Which execution provider computed a vector.
///
/// **A different execution provider is a DIFFERENT SCORER — ADR-015.** The embedder is CPU in every
/// build, `cuda` included: belief vectors persist, so a GPU-computed vector would mix scorers in one
/// store. The enum has one variant; it is kept because its [`EmbedProvider::name`] reaches
/// [`crate::cue::dense::cache::CacheIdentity`], and it is reported by [`Embedder::provider`] so a
/// run cannot be labelled with a provider it did not use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedProvider {
    Cpu,
}

impl EmbedProvider {
    /// The ONNX Runtime provider name. This string is in the cache namespace, so it is an
    /// identity, not a label — do not "tidy" it.
    pub fn name(self) -> &'static str {
        match self {
            EmbedProvider::Cpu => "CPUExecutionProvider",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error(
        "{path} does not exist. The embedding model is never vendored ({dir} is gitignored); \
         fetch it with `python tools/fetch_model.py`. There is no fallback model and no default \
         path"
    )]
    Missing { path: PathBuf, dir: String },

    #[error("reading {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },

    #[error(
        "{path} hashes to {found}, the pinned digest is {expected}. This is a different file \
         than the one every number in runs/ was produced with. A different model still embeds, \
         still scores and still ranks -- it only moves the number, which is why this is a \
         refusal rather than a warning. See DECISIONS.md ADR-004"
    )]
    DigestMismatch { path: PathBuf, found: String, expected: &'static str },

    #[error("{path}: {source}")]
    Vocab { path: PathBuf, source: crate::cue::dense::tokenizer::VocabError },

    #[error("loading the ONNX session from {path}: {source}")]
    Session { path: PathBuf, source: Box<ort::Error> },

    #[error("running the ONNX graph: {source}")]
    Run { source: Box<ort::Error> },

    #[error(
        "the graph returned a hidden state of {found} values for {tokens} tokens, which is not \
         a multiple of {DIMENSIONS}. The model's output dimension disagrees with this build"
    )]
    ShapeDisagrees { found: usize, tokens: usize },

    #[error(transparent)]
    Cache(#[from] crate::cue::dense::cache::CacheError),
}

fn sha256_file(path: &Path) -> Result<String, EmbedError> {
    let bytes = std::fs::read(path).map_err(|source| EmbedError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

fn verify(path: &Path, expected: &'static str, dir: &Path) -> Result<String, EmbedError> {
    if !path.exists() {
        return Err(EmbedError::Missing {
            path: path.to_path_buf(),
            dir: dir.display().to_string(),
        });
    }
    let found = sha256_file(path)?;
    if found != expected {
        return Err(EmbedError::DigestMismatch {
            path: path.to_path_buf(),
            found,
            expected,
        });
    }
    Ok(found)
}

/// The dense cue's embedder.
///
/// No `Debug`: an ONNX `Session` has none, and deriving one by hand would print a model digest
/// and a path into whatever log caught it. `identity()` is the deliberate, minimal alternative.
pub struct Embedder {
    /// One session per worker. `Session::run` takes `&mut self`, and sharing one session across
    /// threads would serialize the very thing the workers exist to parallelize.
    ///
    /// **Every session here uses the SAME execution provider (CPU).** [`Embedder::embed_batch`]
    /// splits a batch contiguously across sessions, so a mixed set would make the vector a text
    /// receives depend on which worker it landed on.
    sessions: Vec<Session>,
    vocab: Vocab,
    cache: Option<EmbeddingCache>,
    truncated: u64,
}

impl Embedder {
    /// Load and verify **on CPU, always**. The fixed-provider constructor.
    ///
    /// `workers` is a throughput knob and provably not a quality knob — see the module docs.
    /// `cache_dir` is `Option` because the cache is a *tool-side* accelerator; the shipping
    /// retrieval path does not need one and must not depend on one existing.
    ///
    /// `tests/embedding_reference.rs` labels its result `cpu`, asserts `MAX_ABS_DIFF = 1e-4`
    /// against HuggingFace's own output, and asserts `provider() == EmbedProvider::Cpu`.
    pub fn load(
        model_dir: &Path,
        workers: usize,
        cache_dir: Option<&Path>,
    ) -> Result<Self, EmbedError> {
        let model_path = model_dir.join(MODEL_FILE);
        let vocab_path = model_dir.join(VOCAB_FILE);

        let model_digest = verify(&model_path, MODEL_SHA256, model_dir)?;
        let vocab_digest = verify(&vocab_path, VOCAB_SHA256, model_dir)?;

        let vocab_text = std::fs::read_to_string(&vocab_path).map_err(|source| EmbedError::Io {
            path: vocab_path.clone(),
            source,
        })?;
        let vocab = Vocab::parse(&vocab_text, &vocab_path.display().to_string())
            .map_err(|source| EmbedError::Vocab { path: vocab_path, source })?;

        let requested = workers.max(1);
        let mut sessions = Vec::with_capacity(requested);
        for _ in 0..requested {
            sessions.push(Self::session(&model_path)?);
        }

        let cache = match cache_dir {
            Some(dir) => Some(EmbeddingCache::open(
                dir,
                CacheIdentity::new(
                    model_digest.clone(),
                    vocab_digest.clone(),
                    EmbedProvider::Cpu.name(),
                ),
            )?),
            None => None,
        };

        Ok(Self { sessions, vocab, cache, truncated: 0 })
    }

    /// Which provider served this embedder. Always CPU (see [`EmbedProvider`]).
    pub fn provider(&self) -> EmbedProvider {
        EmbedProvider::Cpu
    }

    /// Build one CPU session. **The provider is REQUESTED, never left to ORT.**
    ///
    /// Until 2026-08-17 this function called no `with_execution_providers` at all, so the shipped
    /// embedder was CPU *by omission* — nothing asked for anything, and ORT quietly served CPU. The
    /// omission was invisible in exactly the way this project keeps paying for: there was no wrong
    /// value to find, only an absent call.
    ///
    /// CPU registers explicitly with `error_on_failure()`. It cannot fail, and that is not the
    /// point: the point is that CPU is a decision with a line of code behind it rather than the
    /// absence of one.
    fn session(model_path: &Path) -> Result<Session, EmbedError> {
        let build = || -> ort::Result<Session> {
            let b = crate::ep::register(crate::ep::session_builder()?, crate::ep::Provider::Cpu)?;
            let b = b.with_intra_threads(1)?.with_inter_threads(1)?;
            // Pinned explicitly rather than left at the runtime's default: the default is a
            // property of the ONNX Runtime version, and a bump could re-fuse the graph and move
            // a published number with nothing in this repo changing.
            let mut b = b.with_optimization_level(GraphOptimizationLevel::Level1)?;
            b.commit_from_file(model_path)
        };
        build().map_err(|source| EmbedError::Session { path: model_path.to_path_buf(), source: Box::new(source) })
    }

    pub fn workers(&self) -> usize {
        self.sessions.len()
    }

    /// How many texts were truncated at [`MAX_SEQ_LEN`]. Reported, never assumed to be zero.
    pub fn truncated(&self) -> u64 {
        self.truncated
    }

    pub fn cache_stats(&self) -> Option<(u64, u64)> {
        self.cache.as_ref().map(|c| (c.hits(), c.misses()))
    }

    /// Embed one text. Cache-aware.
    pub fn embed(&mut self, text: &str) -> Result<Vec<f32>, EmbedError> {
        if let Some(cache) = self.cache.as_mut() {
            if let Some(hit) = cache.get(text) {
                return Ok(hit.to_vec());
            }
        }
        let encoded = encode(&self.vocab, text, MAX_SEQ_LEN);
        if encoded.truncated {
            self.truncated += 1;
        }
        let vector = Self::forward(&mut self.sessions[0], &encoded.input_ids)?;
        if let Some(cache) = self.cache.as_mut() {
            cache.put(text, &vector)?;
        }
        Ok(vector)
    }

    /// Embed many texts, in input order, using every worker.
    ///
    /// Cache lookups happen first and single-threaded, so a fully warm batch does no inference
    /// at all — which is the property ADR-004's model choice depends on
    /// (`runs/session-c/PREREGISTRATION-model.json`).
    pub fn embed_batch(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let mut out: Vec<Option<Vec<f32>>> = vec![None; texts.len()];
        let mut pending: Vec<usize> = Vec::new();

        for (index, text) in texts.iter().enumerate() {
            match self.cache.as_mut().and_then(|c| c.get(text)) {
                Some(hit) => out[index] = Some(hit.to_vec()),
                None => pending.push(index),
            }
        }

        if !pending.is_empty() {
            let encoded: Vec<_> = pending
                .iter()
                .map(|index| encode(&self.vocab, &texts[*index], MAX_SEQ_LEN))
                .collect();
            self.truncated += encoded.iter().filter(|e| e.truncated).count() as u64;

            // Contiguous split across workers, each returning its results in order. Because the
            // splits are contiguous and taken in worker order, concatenating them reconstructs
            // the input order exactly -- no scatter, no shared mutable state, no unsafe.
            //
            // Worker count therefore cannot reorder or alter anything, which is the property
            // docs/design/spike-2026-08-04-embedder.md measured at 1, 2 and 8 workers.
            let workers = self.sessions.len();
            let per_worker = encoded.len().div_ceil(workers);
            let mut computed: Vec<Result<Vec<f32>, EmbedError>> = Vec::with_capacity(encoded.len());

            std::thread::scope(|scope| {
                let mut handles = Vec::new();
                let mut remaining: &[crate::cue::dense::tokenizer::Encoded] = &encoded;
                for session in self.sessions.iter_mut() {
                    let take = per_worker.min(remaining.len());
                    let (mine, rest) = remaining.split_at(take);
                    remaining = rest;
                    handles.push(scope.spawn(move || {
                        mine.iter()
                            .map(|e| Self::forward(session, &e.input_ids))
                            .collect::<Vec<_>>()
                    }));
                }
                for handle in handles {
                    computed.extend(handle.join().expect("embedding worker panicked"));
                }
            });

            for (position, index) in pending.iter().enumerate() {
                let vector = std::mem::replace(
                    &mut computed[position],
                    Err(EmbedError::ShapeDisagrees { found: 0, tokens: 0 }),
                )?;
                if let Some(cache) = self.cache.as_mut() {
                    cache.put(&texts[*index], &vector)?;
                }
                out[*index] = Some(vector);
            }
        }

        Ok(out.into_iter().map(|v| v.expect("every slot filled")).collect())
    }

    fn forward(session: &mut Session, input_ids: &[u32]) -> Result<Vec<f32>, EmbedError> {
        let tokens = input_ids.len();
        let ids: Vec<i64> = input_ids.iter().map(|v| *v as i64).collect();
        let mask: Vec<i64> = vec![1; tokens];
        let zeros: Vec<i64> = vec![0; tokens];

        let names: Vec<String> = session.inputs().iter().map(|i| i.name().to_string()).collect();
        let mut inputs: Vec<(String, Value)> = Vec::with_capacity(names.len());
        for name in names {
            let data = if name.contains("attention") {
                &mask
            } else if name.contains("token_type") {
                &zeros
            } else {
                &ids
            };
            let tensor = Value::from_array(([1usize, tokens], data.clone()))
                .map_err(|source| EmbedError::Run { source: Box::new(source) })?;
            inputs.push((name, tensor.into_dyn()));
        }

        let outputs = session
            .run(inputs)
            .map_err(|source| EmbedError::Run { source: Box::new(source) })?;
        let (_, hidden) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|source| EmbedError::Run { source: Box::new(source) })?;

        if hidden.len() != tokens * DIMENSIONS {
            return Err(EmbedError::ShapeDisagrees { found: hidden.len(), tokens });
        }
        Ok(mean_pool_and_normalize(hidden, tokens))
    }

    /// What the cache keys on, for reporting. Never used to decide anything.
    ///
    /// It does NOT carry the provider; the provider reaches the cache namespace from
    /// [`EmbedProvider::name`] at construction, not from this tuple.
    pub fn identity() -> (&'static str, &'static str, &'static str, usize, usize) {
        (MODEL_SHA256, VOCAB_SHA256, EMBEDDER_VERSION, MAX_SEQ_LEN, DIMENSIONS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .unwrap()
            .join("models")
            .join("jina-embeddings-v2-small-en")
    }

    #[test]
    fn a_missing_model_names_the_fetch_command_rather_than_defaulting() {
        let err = match Embedder::load(Path::new("no-such-directory"), 1, None) {
            Err(e) => e,
            Ok(_) => panic!("a missing model must not load"),
        };
        assert!(matches!(err, EmbedError::Missing { .. }), "{err}");
        assert!(err.to_string().contains("tools/fetch_model.py"), "{err}");
    }

    #[test]
    fn a_swapped_model_file_is_refused_at_load() {
        // The hazard the load-time digest exists for, and the reason it is checked here as well
        // as at download: a file replaced after fetching passes the fetcher and reaches this.
        let dir = std::env::temp_dir().join("marlowe-embedder-swapped");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(MODEL_FILE), b"not an onnx graph").unwrap();
        std::fs::write(dir.join(VOCAB_FILE), b"[PAD]\n").unwrap();

        let err = match Embedder::load(&dir, 1, None) {
            Err(e) => e,
            Ok(_) => panic!("a swapped model file must not load"),
        };
        assert!(matches!(err, EmbedError::DigestMismatch { .. }), "{err}");
        assert!(err.to_string().contains("ADR-004"), "{err}");
    }

    #[test]
    fn the_pinned_digests_match_the_fetched_model() {
        // Skips on a fresh clone, where models/ legitimately does not exist.
        let dir = model_dir();
        if !dir.join(MODEL_FILE).exists() {
            eprintln!("SKIP: run `python tools/fetch_model.py`");
            return;
        }
        assert_eq!(sha256_file(&dir.join(MODEL_FILE)).unwrap(), MODEL_SHA256);
        assert_eq!(sha256_file(&dir.join(VOCAB_FILE)).unwrap(), VOCAB_SHA256);
    }
}
