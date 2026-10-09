//! The embedding cache — **load-bearing for ADR-004's model choice, not an optimization.**
//!
//! `runs/session-c/PREREGISTRATION-model.json` adopts jina-embeddings-v2-small-en *conditional
//! on this cache existing and being correct*. The arithmetic: 493,500 embeddings per full
//! fit-and-score cycle, 142.5 minutes for two cycles uncached against a pre-registered
//! 90-minute budget, 71.3 minutes with the cache because the second cycle re-embeds nothing.
//! If this file is wrong, the model choice was never licensed.
//!
//! **The failure mode is silence.** A cache that returns the wrong vector does not crash. It
//! returns 512 unit-length floats that score, rank, and produce a number — the project's
//! unobservable-mismatch pattern, in the one component whose whole job is to skip a computation.
//! Two things guard it, and neither is a comment:
//!
//! 1. **Every input that can change the output is in the key.** Model digest, vocabulary digest,
//!    embedder version, `MAX_SEQ_LEN`, and the text. A key of text alone would serve
//!    all-MiniLM-L6-v2 vectors to a jina build after a model swap.
//! 2. **A hit and a miss must be byte-identical**, asserted by test, and that test is a
//!    **standing check** in `STATE.md` — re-run whenever the model, tokenizer, embedder version
//!    or `MAX_SEQ_LEN` changes, not only when this file changes.
//!
//! **Not in the profile root.** `--profile-root` is required to be empty on every spawn and the
//! harness spawns one process per corpus plus four for the clock probe, so a cache there would
//! be cold every time and would prove nothing. It is passed in explicitly and has no default —
//! a re-resolved default path is on CLAUDE.md's list of things that let two sides silently
//! disagree.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::cue::dense::{DIMENSIONS, MAX_SEQ_LEN};

/// Bumped by hand whenever the arithmetic around the graph changes — pooling, normalization,
/// the tokenizer's algorithm — in a way that moves vectors without moving the model digest.
///
/// It is in the cache key, so a bump invalidates every entry. Forgetting to bump it after
/// changing the pooling is the one hazard the digests cannot catch, which is why it sits here
/// next to them rather than in a config file.
pub const EMBEDDER_VERSION: &str = "session-c-1";

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("embedding cache at {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },

    #[error(
        "embedding cache at {path} holds a record of {found} bytes, which is not a whole \
         number of {DIMENSIONS}-dimension f32 vectors ({expected} bytes). Refusing to serve a \
         truncated vector: it would still be unit-ish, still score, and still rank"
    )]
    CorruptRecord { path: PathBuf, found: usize, expected: usize },
}

/// The identity of the embedder a cached vector was produced by.
///
/// Every field is an input the output depends on. Adding a field that affects vectors and
/// forgetting to put it here is how a stale vector survives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheIdentity {
    pub model_sha256: String,
    pub vocab_sha256: String,
    pub embedder_version: &'static str,
    pub max_seq_len: usize,
    pub dimensions: usize,
    /// The ONNX execution provider that computed the vectors.
    ///
    /// **Added 2026-08-17, when the embedder stopped being CPU by omission, and it is exactly the
    /// field this module's header is about.** ADR-015's rule is that a different execution provider
    /// is a different scorer; a CUDA vector and a CPU vector for the same text are not required to
    /// be bit-identical and are not. Without this field they share a key, so a warm cache written
    /// on one provider is served to a run on the other — a vector that is unit-length, scores,
    /// ranks, and is wrong for the configuration claiming it.
    ///
    /// **It invalidates every cache entry written before this date**, which is the same cost
    /// `MAX_SEQ_LEN` imposed and is paid for the same reason. The alternative — exempting CPU so
    /// existing files survive — would make the namespace mean "provider, unless it is the one we
    /// used to assume", and a rule with an exemption for the historical default is how a stale
    /// artifact gets served under a new label.
    pub provider: &'static str,
}

impl CacheIdentity {
    pub fn new(
        model_sha256: impl Into<String>,
        vocab_sha256: impl Into<String>,
        provider: &'static str,
    ) -> Self {
        Self {
            model_sha256: model_sha256.into(),
            vocab_sha256: vocab_sha256.into(),
            embedder_version: EMBEDDER_VERSION,
            max_seq_len: MAX_SEQ_LEN,
            dimensions: DIMENSIONS,
            provider,
        }
    }

    /// The per-identity namespace. Two identities never share a file, so a model swap cannot
    /// read the previous model's vectors even if a key somehow collided.
    fn namespace(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.model_sha256.as_bytes());
        hasher.update(b"\0");
        hasher.update(self.vocab_sha256.as_bytes());
        hasher.update(b"\0");
        hasher.update(self.embedder_version.as_bytes());
        hasher.update(b"\0");
        hasher.update(&(self.max_seq_len as u64).to_le_bytes());
        hasher.update(&(self.dimensions as u64).to_le_bytes());
        hasher.update(b"\0");
        hasher.update(self.provider.as_bytes());
        hasher.finalize().to_hex()[..32].to_string()
    }

    /// The key for one text, under this identity.
    fn key(&self, text: &str) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        // The identity is hashed INTO the key as well as namespacing the file. Belt and braces
        // is cheap here and the failure it prevents is silent.
        hasher.update(self.namespace().as_bytes());
        hasher.update(b"\0");
        hasher.update(text.as_bytes());
        *hasher.finalize().as_bytes()
    }
}

/// A content-addressed store of embeddings, loaded into memory and appended to on disk.
///
/// Deliberately a flat append-only log plus an in-memory index rather than a database: the
/// access pattern is "write once, read many, never update", the file is a derived artifact that
/// can always be deleted, and a corrupt tail costs a re-embed rather than a wrong answer.
pub struct EmbeddingCache {
    path: PathBuf,
    identity: CacheIdentity,
    /// key -> vector. BTreeMap, not HashMap: the determinism guard bans hash-ordered
    /// collections outright and this one is read on the scored path.
    entries: BTreeMap<[u8; 32], Vec<f32>>,
    hits: u64,
    misses: u64,
}

impl EmbeddingCache {
    /// Open (or create) the cache for one embedder identity.
    ///
    /// A record whose length is not a whole vector is a hard error, not a skipped entry:
    /// silently dropping a corrupt tail would make the cache's contents depend on how a previous
    /// process happened to die.
    pub fn open(directory: &Path, identity: CacheIdentity) -> Result<Self, CacheError> {
        fs::create_dir_all(directory).map_err(|source| CacheError::Io {
            path: directory.to_path_buf(),
            source,
        })?;
        let path = directory.join(format!("embeddings-{}.bin", identity.namespace()));

        let mut entries = BTreeMap::new();
        if path.exists() {
            let bytes = fs::read(&path).map_err(|source| CacheError::Io {
                path: path.clone(),
                source,
            })?;
            let record = 32 + DIMENSIONS * 4;
            if !bytes.len().is_multiple_of(record) {
                return Err(CacheError::CorruptRecord {
                    path,
                    found: bytes.len() % record,
                    expected: record,
                });
            }
            for chunk in bytes.chunks_exact(record) {
                let mut key = [0u8; 32];
                key.copy_from_slice(&chunk[..32]);
                let vector = chunk[32..]
                    .chunks_exact(4)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    .collect();
                entries.insert(key, vector);
            }
        }

        Ok(Self { path, identity, entries, hits: 0, misses: 0 })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn hits(&self) -> u64 {
        self.hits
    }

    pub fn misses(&self) -> u64 {
        self.misses
    }

    pub fn get(&mut self, text: &str) -> Option<&[f32]> {
        let key = self.identity.key(text);
        match self.entries.get(&key) {
            Some(vector) => {
                self.hits += 1;
                Some(vector)
            }
            None => {
                self.misses += 1;
                None
            }
        }
    }

    /// Record a freshly computed vector. Appends immediately so a crash mid-run keeps what it
    /// earned — the whole value of the cache is that the *second* cycle is free.
    pub fn put(&mut self, text: &str, vector: &[f32]) -> Result<(), CacheError> {
        debug_assert_eq!(vector.len(), DIMENSIONS);
        let key = self.identity.key(text);
        if self.entries.contains_key(&key) {
            return Ok(());
        }

        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|source| CacheError::Io { path: self.path.clone(), source })?;
        let mut writer = BufWriter::new(file);
        writer
            .write_all(&key)
            .and_then(|_| {
                for value in vector {
                    writer.write_all(&value.to_le_bytes())?;
                }
                writer.flush()
            })
            .map_err(|source| CacheError::Io { path: self.path.clone(), source })?;

        self.entries.insert(key, vector.to_vec());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> CacheIdentity {
        CacheIdentity::new("model-digest", "vocab-digest", "CPUExecutionProvider")
    }

    fn vector(seed: f32) -> Vec<f32> {
        let raw: Vec<f32> = (0..DIMENSIONS).map(|i| (i as f32 * seed).sin()).collect();
        let norm = raw.iter().map(|x| x * x).sum::<f32>().sqrt();
        raw.into_iter().map(|x| x / norm).collect()
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("marlowe-cache-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// **The test the model choice depends on.** See STATE.md's standing checks.
    #[test]
    fn a_hit_and_a_miss_produce_byte_identical_vectors() {
        let dir = temp_dir("byte-identity");
        let computed = vector(0.37);

        let mut cache = EmbeddingCache::open(&dir, identity()).unwrap();
        assert!(cache.get("the ingest job times out").is_none(), "cold: a miss");
        cache.put("the ingest job times out", &computed).unwrap();

        // Same process, now a hit.
        let hit = cache.get("the ingest job times out").unwrap().to_vec();
        assert_eq!(
            hit, computed,
            "a hit must return exactly what was computed, bit for bit -- not approximately"
        );

        // A NEW process would re-read the file. Same thing, through the disk round trip, which
        // is where an f32 serialization bug would live.
        let mut reopened = EmbeddingCache::open(&dir, identity()).unwrap();
        let from_disk = reopened.get("the ingest job times out").unwrap().to_vec();
        assert_eq!(from_disk, computed, "the disk round trip must be lossless");
        for (a, b) in from_disk.iter().zip(computed.iter()) {
            assert_eq!(a.to_bits(), b.to_bits(), "bit patterns, not just equality");
        }
    }

    #[test]
    fn a_different_model_digest_cannot_read_the_previous_models_vectors() {
        // The hazard the key exists for: after a model swap, a cache keyed on text alone would
        // serve the old model's vectors. They would still be unit length and still rank.
        let dir = temp_dir("model-swap");
        let text = "we had pasta for dinner";

        let mut old = EmbeddingCache::open(&dir, CacheIdentity::new("minilm-digest", "v", "CPUExecutionProvider")).unwrap();
        old.put(text, &vector(0.11)).unwrap();

        let mut new = EmbeddingCache::open(&dir, CacheIdentity::new("jina-digest", "v", "CPUExecutionProvider")).unwrap();
        assert!(new.get(text).is_none(), "a different model must miss, not inherit");
    }

    #[test]
    fn every_identity_field_participates_in_the_key() {
        // Each field below can change the vectors. A field that does not change the namespace
        // is a field that can serve a stale vector, so this enumerates them rather than
        // trusting the constructor.
        let base = identity();
        let mut vocab_changed = base.clone();
        vocab_changed.vocab_sha256 = "other".into();
        let mut version_changed = base.clone();
        version_changed.embedder_version = "session-c-2";
        let mut len_changed = base.clone();
        len_changed.max_seq_len = MAX_SEQ_LEN + 1;
        let mut dims_changed = base.clone();
        dims_changed.dimensions = DIMENSIONS + 1;
        let mut model_changed = base.clone();
        model_changed.model_sha256 = "other".into();
        // ADR-015: a different execution provider is a different scorer, so it is a different
        // namespace. Without this row a CUDA-computed vector is served to a CPU run.
        let mut provider_changed = base.clone();
        provider_changed.provider = "CUDAExecutionProvider";

        let namespaces = [
            base.namespace(),
            vocab_changed.namespace(),
            version_changed.namespace(),
            len_changed.namespace(),
            dims_changed.namespace(),
            model_changed.namespace(),
            provider_changed.namespace(),
        ];
        for (i, a) in namespaces.iter().enumerate() {
            for (j, b) in namespaces.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "identity fields {i} and {j} collide");
                }
            }
        }
    }

    #[test]
    fn different_texts_do_not_collide() {
        let dir = temp_dir("distinct");
        let mut cache = EmbeddingCache::open(&dir, identity()).unwrap();
        cache.put("alpha", &vector(0.2)).unwrap();
        cache.put("beta", &vector(0.9)).unwrap();
        let alpha = cache.get("alpha").unwrap().to_vec();
        let beta = cache.get("beta").unwrap().to_vec();
        assert_ne!(alpha, beta);
        assert!(cache.get("gamma").is_none());
    }

    #[test]
    fn hits_and_misses_are_counted_so_a_dead_cache_is_visible() {
        // A cache that never hits is indistinguishable from no cache, and the model decision
        // rests on it hitting. Silence there would look exactly like success.
        let dir = temp_dir("counters");
        let mut cache = EmbeddingCache::open(&dir, identity()).unwrap();
        cache.get("x");
        cache.put("x", &vector(0.5)).unwrap();
        cache.get("x");
        cache.get("x");
        assert_eq!(cache.misses(), 1);
        assert_eq!(cache.hits(), 2);
    }

    #[test]
    fn a_truncated_file_is_refused_rather_than_partially_served() {
        let dir = temp_dir("corrupt");
        let mut cache = EmbeddingCache::open(&dir, identity()).unwrap();
        cache.put("x", &vector(0.5)).unwrap();
        let path = cache.path.clone();

        let mut bytes = fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 7);
        fs::write(&path, bytes).unwrap();

        assert!(matches!(
            EmbeddingCache::open(&dir, identity()),
            Err(CacheError::CorruptRecord { .. })
        ));
    }

    #[test]
    fn putting_the_same_text_twice_does_not_duplicate_the_record() {
        let dir = temp_dir("idempotent");
        let mut cache = EmbeddingCache::open(&dir, identity()).unwrap();
        let v = vector(0.5);
        cache.put("x", &v).unwrap();
        cache.put("x", &v).unwrap();
        assert_eq!(cache.len(), 1);
        assert_eq!(EmbeddingCache::open(&dir, identity()).unwrap().len(), 1);
    }
}
