//! The vector store — a **derived view**, never journaled.
//!
//! **Embeddings are re-derived and never written to the journal**, and the reason is ADR-009's,
//! reused rather than re-argued. ADR-009 refused a structural signature on `MemoryEntry` because
//! a derived key stored outside the encrypted record *survives crypto-shredding* (an invariant 5
//! hole) and *does not demote when the entry's fidelity does*, so it keeps matching at full
//! strength on a Gist — §5.4's worst-failure clause. An embedding is the same shape of object
//! and inherits both hazards exactly. So it lives here, derived from whatever text the entry
//! currently carries, and disappears with it.
//!
//! **There is one derivation function, not two.** [`VectorStore::embed_missing`] is called both
//! after an ingest and after a full rebuild; the incremental path and the from-scratch path are
//! *the same code*, so they cannot drift. That is deliberately stronger than having two paths
//! and a test asserting they agree — CLAUDE.md's warning about defaults that make a mismatch
//! unobservable applies to duplicated derivations too.

use std::collections::BTreeMap;

use crate::cue::dense::embedder::{EmbedError, Embedder};
use crate::store::BeliefStore;

/// Vectors for live memories, keyed by memory id.
#[derive(Debug, Default, Clone)]
pub struct VectorStore {
    /// BTreeMap: the determinism guard bans hash-ordered collections, and this is read on the
    /// scored path where iteration order could reach the wire.
    vectors: BTreeMap<String, Vec<f32>>,
}

impl VectorStore {
    pub fn len(&self) -> usize {
        self.vectors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.vectors.is_empty()
    }

    pub fn get(&self, memory_id: &str) -> Option<&[f32]> {
        self.vectors.get(memory_id).map(Vec::as_slice)
    }

    /// Embed every live memory that does not yet have a vector. Returns how many were computed.
    ///
    /// Batched, because that is where the worker parallelism pays: one LongMemEval ingest is
    /// ~493 turns, and embedding them one at a time would serialize the whole run.
    ///
    /// **Entries are visited in `BeliefStore` order and the batch preserves it.** Nothing here
    /// depends on that for correctness — a vector is a pure function of its text — but a fixed
    /// order keeps the cache's append sequence identical between two runs, which keeps the cache
    /// *file* reproducible as well as its contents.
    pub fn embed_missing(
        &mut self,
        beliefs: &BeliefStore,
        embedder: &mut Embedder,
    ) -> Result<usize, EmbedError> {
        let mut ids: Vec<String> = Vec::new();
        let mut texts: Vec<String> = Vec::new();
        for entry in beliefs.recall_candidates() {
            if !self.vectors.contains_key(&entry.id) {
                ids.push(entry.id.clone());
                texts.push(entry.text.clone());
            }
        }
        if ids.is_empty() {
            return Ok(0);
        }

        let vectors = embedder.embed_batch(&texts)?;
        debug_assert_eq!(vectors.len(), ids.len());
        for (id, vector) in ids.iter().zip(vectors) {
            self.vectors.insert(id.clone(), vector);
        }
        Ok(ids.len())
    }

    /// Install a vector directly. **Tests only, and it stays that way.**
    ///
    /// There is deliberately no public setter. This module's whole claim is that
    /// [`VectorStore::embed_missing`] is the *one* derivation function, so a second write path
    /// reachable from production code would be the drift it exists to prevent. `#[cfg(test)]`
    /// makes that structural rather than a naming convention.
    #[cfg(test)]
    pub(crate) fn insert_for_test(&mut self, memory_id: &str, vector: Vec<f32>) {
        self.vectors.insert(memory_id.to_string(), vector);
    }

    /// Drop one vector, so a test can exercise the absent-vector path. Tests only, as above.
    #[cfg(test)]
    pub(crate) fn forget_for_test(&mut self, memory_id: &str) {
        self.vectors.remove(memory_id);
    }

    /// Drop vectors for memories that are no longer live.
    ///
    /// Called after consolidation. Without it a tombstoned memory's vector would outlive the
    /// memory — which is precisely the forgetting leak the module docs refuse to create.
    pub fn retain_live(&mut self, beliefs: &BeliefStore) {
        let live: std::collections::BTreeSet<&str> = beliefs
            .recall_candidates()
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        self.vectors.retain(|id, _| live.contains(id.as_str()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{MemoryEntry, MATURATION_WINDOW_MS};
    use marlowe_contract::{Fidelity, PayloadKind, TrustClass};

    fn entry(id: &str, text: &str) -> MemoryEntry {
        MemoryEntry {
            id: id.into(),
            text: text.into(),
            payload_kind: PayloadKind::Episode,
            embedding_ref: None,
            source_turn_id: format!("t-{id}"),
            source_session_id: "s-1".into(),
            trust_class: TrustClass::UserAsserted,
            effective_trust: TrustClass::UserAsserted,
            derivation: Vec::new(),
            origin_event: 1,
            occurred_at_ms: 1_000,
            created_at: 1_000,
            last_accessed: 1_000,
            access_count: 0,
            confidence: 1.0,
            activation: 1.0,
            fidelity: Fidelity::Record,
            silent_until: Some(1_000 + MATURATION_WINDOW_MS),
            supersedes: Vec::new(),
            superseded_by: None,
        }
    }

    #[test]
    fn an_empty_store_returns_nothing_rather_than_a_zero_vector() {
        // A zero vector would score 0.0 cosine against everything, which is indistinguishable
        // from "genuinely irrelevant" -- a missing vector must be visible as missing.
        let store = VectorStore::default();
        assert!(store.get("m-a").is_none());
        assert!(store.is_empty());
    }

    #[test]
    fn retain_live_drops_vectors_for_memories_that_are_gone() {
        let mut beliefs = BeliefStore::default();
        beliefs.insert(entry("m-a", "alpha"));
        beliefs.insert(entry("m-b", "beta"));

        let mut store = VectorStore::default();
        store.vectors.insert("m-a".into(), vec![0.0; 4]);
        store.vectors.insert("m-b".into(), vec![0.0; 4]);
        store.vectors.insert("m-gone".into(), vec![0.0; 4]);

        store.retain_live(&beliefs);
        assert!(store.get("m-a").is_some());
        assert!(store.get("m-b").is_some());
        assert!(
            store.get("m-gone").is_none(),
            "a vector outliving its memory is the forgetting leak ADR-009 refuses"
        );
    }
}
