//! The lexical cue — BM25 over the live candidate set.
//!
//! **Pure Rust, not SQLite FTS5, and the reason is determinism.** `STATE.md` suggested FTS5
//! (the 2026-08-01 spike used it from Python), but it was never a pinned decision and
//! `DECISIONS.md` does not name it. Three reasons it is the wrong instrument here:
//!
//! 1. FTS5's `bm25()` ranking is a property of the *bundled SQLite version*. `marlowe-eval
//!    repro` compares two runs byte for byte and the injected set is in that hash, so a
//!    dependency bump could change a published number with nothing in this repo changing.
//! 2. A second physical index has to be kept in sync with §4.3's live-only partition. A drift
//!    there is unobservable — retrieval quietly reads a stale or over-broad set and every test
//!    stays green. That is the failure pattern CLAUDE.md says has already cost this project
//!    four bugs.
//! 3. The tokenizer is the part most likely to be wrong, and here it is fifteen lines that can
//!    be asserted against hand-computed values.
//!
//! ADR-003's hot index is about *physical* partitioning at scale. Session A recorded that the
//! candidate filter **is** the partition for now; that is still true and this cue reads it.
//!
//! Every constant below is a **frozen parameter** under HP1: the cue is on the measured path,
//! so its weights are frozen in M0 exactly as the gate's are.

use std::collections::BTreeMap;

use crate::entry::{MemoryEntry, MemoryId};

/// BM25 term-frequency saturation. The standard value.
pub const BM25_K1: f64 = 1.2;

/// BM25 length normalization. The standard value.
pub const BM25_B: f64 = 0.75;

/// Maps an unbounded BM25 score into `[0, 1]` as `s / (s + K)`.
///
/// The mapping is half-open in real arithmetic and closed after the `f32` cast: scores large
/// enough round to exactly 1.0. Stated as `[0, 1]` because that is what the function returns.
/// Real BM25 scores over a session's candidate set land in single or low double digits, so the
/// rounding regime is not reachable in practice — but the range is documented as measured, not
/// as derived.
///
/// **Absolute, deliberately — not min-max over the query's candidate set.** Min-max looks
/// like the obvious normalization and would quietly destroy the gate: it forces the best
/// candidate of *every* query to 1.0, including queries where nothing matches at all. A gate
/// whose top feature is 1.0 by construction cannot abstain, and abstention is a first-class
/// outcome under §4.2 — so the normalization has to preserve "everything here is bad".
///
/// The constant only sets where the fitted logistic's operating range sits; the fitted weight
/// and bias absorb the scale. It is not a quality knob and must not be tuned as one.
pub const BM25_SATURATION: f64 = 10.0;

/// Split text into terms: ASCII-lowercased, non-alphanumeric treated as a boundary.
///
/// No stemming and no stop list. Both are defensible and both are *tuning knobs*, which under
/// HP1's freeze rule would have to arrive as frozen artifacts with a recorded decision behind
/// them. Digits are kept — LongMemEval's temporal-reasoning category turns on dates and
/// version numbers, and dropping them would hobble the cue on the category most likely to
/// need it.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buffer = String::new();
    // **Defined in terms of `for_each_token`, not beside it.** Two token splitters with the same
    // rules written twice is the two-implementations-one-checked pattern applied to the thing every
    // BM25 number is computed over; a divergence would move the lexical cue and nothing would say
    // so. There is one splitter and this is a collecting wrapper around it.
    for_each_token(text, &mut buffer, |token| out.push(token.to_string()));
    out
}

/// [`tokenize`] without the allocations: calls `emit` once per token, reusing one buffer.
///
/// **This is where the lexical stage's cost actually was.** M0c Session L measured `score_all` at
/// 14.7 ms p50 — the second-largest stage in retrieval, 6.83% of P95 — for a set of ~487 candidates
/// re-tokenized on *every* query. The dominant term was not the character scan but roughly 97,000
/// `String` allocations per query, plus a `BTreeMap` built per document.
///
/// The token sequence is identical to [`tokenize`]'s by construction, because that function is
/// implemented with this one.
fn for_each_token(text: &str, buffer: &mut String, mut emit: impl FnMut(&str)) {
    buffer.clear();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            buffer.push(ch.to_ascii_lowercase());
        } else if ch.is_alphanumeric() {
            // Non-ASCII alphanumerics are kept as-is rather than dropped. `to_ascii_lowercase`
            // is a no-op on them, so this is a faithful passthrough, not a casefold claim.
            buffer.push(ch);
        } else if !buffer.is_empty() {
            emit(buffer);
            buffer.clear();
        }
    }
    if !buffer.is_empty() {
        emit(buffer);
        buffer.clear();
    }
}

/// Raw BM25 for every document, in the order the documents were given.
///
/// The corpus statistics (`N`, document frequency, average length) are taken over **the
/// candidate set passed in** — the set retrieval is actually choosing from. Computing IDF over
/// a wider set than the one being ranked would score documents against a corpus they are not
/// competing in.
///
/// Determinism: query terms are deduplicated in order of first appearance and documents are
/// visited in the given order, so the floating-point accumulation order is fixed. `repro`
/// compares two runs byte for byte and the injected set is in that hash.
pub fn score_all(docs: &[&MemoryEntry], query_text: &str) -> Vec<f32> {
    // **The projection, and nothing else, lives here.** See [`score_texts`]: this function reads
    // `doc.text` and delegates, so there is exactly one BM25 in the workspace and the scored path
    // is bit-identical to what it was before the split.
    let texts: Vec<&str> = docs.iter().map(|d| d.text.as_str()).collect();
    score_texts(&texts, query_text)
}

/// [`score_all`] over bare text, for a candidate set that is not a belief.
///
/// # Why this exists — ADR-051
///
/// `use(query = ...)` ranks installed skills, and a skill is not a `MemoryEntry`. The options were
/// a second BM25 beside this one, or lifting the one line of this function that knew what a
/// document was. **A second scorer is the wrong answer** on a project that has logged what happens
/// when two implementations of one thing drift: the tokenizer is the part most likely to be wrong,
/// and two of them would disagree silently while both looked right.
///
/// The arithmetic below is untouched by the split. `score_all` is now a projection plus a call, so
/// the accumulation order — which the header above establishes is part of the *result*, not a
/// presentation choice — is the same sequence of `f64` additions it always was.
/// `the_belief_path_and_the_text_path_are_the_same_arithmetic` pins that instead of asserting it
/// in prose.
pub fn score_texts(docs: &[&str], query_text: &str) -> Vec<f32> {
    let n = docs.len();
    if n == 0 {
        return Vec::new();
    }

    let query_terms = query_terms(query_text);
    let t = query_terms.len();
    // Term -> its column. BTreeMap because the determinism guard bans hash-ordered collections on
    // any path whose order can reach the wire.
    let column: BTreeMap<&str, usize> =
        query_terms.iter().enumerate().map(|(j, q)| (q.as_str(), j)).collect();

    // **Only the query's terms are counted.** The previous implementation built a full
    // `BTreeMap<&str, u32>` of every term in every document and a document-frequency map over the
    // whole vocabulary, then read ~5-10 entries out of them. Everything else was computed and
    // discarded. `counts` is row-major, document `i` occupying `[i * t .. (i + 1) * t]`.
    let mut counts = vec![0u32; n * t];
    let mut lengths = vec![0usize; n];
    let mut buffer = String::new();
    for (i, doc) in docs.iter().enumerate() {
        let mut length = 0usize;
        for_each_token(doc, &mut buffer, |token| {
            // The length is every token, not just the matched ones: it is the BM25 length
            // normalizer's input, so counting only query terms here would silently rescale every
            // score while every test that checks a ranking still passed.
            length += 1;
            if let Some(j) = column.get(token) {
                counts[i * t + j] += 1;
            }
        });
        lengths[i] = length;
    }

    bm25(n, t, &counts, &lengths)
}

/// The query's terms, deduplicated in order of first appearance. **This order is the
/// accumulation order in [`bm25`] and therefore part of the arithmetic**, not a presentation
/// choice.
fn query_terms(query_text: &str) -> Vec<String> {
    let mut query_terms: Vec<String> = Vec::new();
    for term in tokenize(query_text) {
        if !query_terms.contains(&term) {
            query_terms.push(term);
        }
    }
    query_terms
}

/// BM25 from per-document query-term counts (row-major, `n * t`) and token lengths.
///
/// The one place the scoring arithmetic lives. [`score_texts`] and [`score_indexed`] differ only
/// in how they fill `counts` and `lengths`, which are integers, so their scores are bit-identical.
fn bm25(n: usize, t: usize, counts: &[u32], lengths: &[usize]) -> Vec<f32> {
    // Summed in document order, exactly as `lengths.iter().sum()` did.
    let total_len: usize = lengths.iter().sum();
    // A candidate set of empty documents has no average length to speak of. Guarding with 1.0
    // keeps the length-normalization term finite; every tf is zero in that case anyway, so the
    // guard cannot manufacture a score.
    let avgdl = if total_len == 0 {
        1.0
    } else {
        total_len as f64 / n as f64
    };

    // Document frequency, over the candidate set, for the query's terms only.
    let mut document_frequency = vec![0u32; t];
    for i in 0..n {
        for j in 0..t {
            if counts[i * t + j] > 0 {
                document_frequency[j] += 1;
            }
        }
    }

    // The BM25+ / Lucene form: always positive, so a term present in every document
    // contributes a small amount rather than a negative one. The classic Robertson IDF
    // goes negative above df > N/2, which would let a common term *penalise* the
    // documents that contain it.
    let idf: Vec<Option<f64>> = (0..t)
        .map(|j| {
            let df_t = document_frequency[j] as f64;
            if df_t == 0.0 {
                None
            } else {
                Some((1.0 + (n as f64 - df_t + 0.5) / (df_t + 0.5)).ln())
            }
        })
        .collect();

    // **Document-major, and the accumulation order is unchanged.** The previous loop was
    // term-major, but for any fixed document the terms were still visited in `query_terms` order —
    // so each `scores[i]` is the sum of exactly the same f64 sequence, in exactly the same order.
    // That is what makes this bit-identical rather than merely equivalent, and byte-identity of
    // `scored-candidates.ndjson` is the gate this change is held to.
    let mut scores = vec![0.0f64; n];
    for i in 0..n {
        let norm = 1.0 - BM25_B + BM25_B * (lengths[i] as f64 / avgdl);
        for j in 0..t {
            let Some(idf) = idf[j] else { continue };
            let f = counts[i * t + j] as f64;
            if f == 0.0 {
                continue;
            }
            scores[i] += idf * (f * (BM25_K1 + 1.0)) / (f + BM25_K1 * norm);
        }
    }

    scores.into_iter().map(|s| s as f32).collect()
}

/// Per-document term statistics, cached so a query never re-tokenizes the store.
///
/// Holds exactly the integers [`score_texts`] would compute from each document's text: its
/// token count and its term frequencies. Corpus statistics (`N`, df, avgdl) are **not** cached:
/// they are taken over the candidate set each query actually ranks, which moves with maturation
/// and scope, and caching them over the whole store would change the arithmetic.
///
/// Kept current by [`crate::store::BeliefStore`], the only writer of entry text.
#[derive(Debug, Default, Clone)]
pub struct LexicalIndex {
    /// Interned terms. BTreeMap: hash-ordered collections are banned on the scored path.
    vocab: BTreeMap<String, u32>,
    docs: BTreeMap<MemoryId, DocTerms>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct DocTerms {
    length: usize,
    /// `(term id, count)`, sorted by term id.
    terms: Vec<(u32, u32)>,
}

impl LexicalIndex {
    /// Index (or re-index) one document's current text.
    pub fn upsert(&mut self, id: &str, text: &str) {
        let mut counts: BTreeMap<u32, u32> = BTreeMap::new();
        let mut length = 0usize;
        let mut buffer = String::new();
        let vocab = &mut self.vocab;
        for_each_token(text, &mut buffer, |token| {
            length += 1;
            let term = match vocab.get(token) {
                Some(term) => *term,
                None => {
                    let next = vocab.len() as u32;
                    vocab.insert(token.to_string(), next);
                    next
                }
            };
            *counts.entry(term).or_insert(0) += 1;
        });
        self.docs.insert(id.to_string(), DocTerms { length, terms: counts.into_iter().collect() });
    }

    pub fn remove(&mut self, id: &str) {
        self.docs.remove(id);
    }

    /// Documents indexed.
    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// Distinct terms ever seen. Never shrinks; ids are stable for the index's life.
    pub fn vocabulary(&self) -> usize {
        self.vocab.len()
    }

    /// How many indexed documents contain each of `terms` (already tokenized), in order. Read-only
    /// statistics for extractive summaries; not on the scored path.
    pub fn document_frequencies(&self, terms: &[&str]) -> Vec<usize> {
        let ids: Vec<Option<u32>> = terms.iter().map(|t| self.vocab.get(*t).copied()).collect();
        let mut wanted: Vec<(u32, usize)> = ids.iter().enumerate().filter_map(|(i, id)| id.map(|id| (id, i))).collect();
        wanted.sort_unstable();
        let mut out = vec![0usize; terms.len()];
        if wanted.is_empty() {
            return out;
        }
        for doc in self.docs.values() {
            for (term, _) in &doc.terms {
                if let Ok(k) = wanted.binary_search_by_key(term, |(id, _)| *id) {
                    // Equal ids sit together; count every position that asked for this term.
                    let mut j = k;
                    while j > 0 && wanted[j - 1].0 == *term {
                        j -= 1;
                    }
                    while j < wanted.len() && wanted[j].0 == *term {
                        out[wanted[j].1] += 1;
                        j += 1;
                    }
                }
            }
        }
        out
    }
}

/// [`score_all`] over cached statistics. **Bit-identical to `score_all`** for any documents whose
/// cached text is current; a document missing from the index is tokenized on the spot, so the
/// result never depends on whether the cache was warm.
pub fn score_indexed(index: &LexicalIndex, docs: &[&MemoryEntry], query_text: &str) -> Vec<f32> {
    let n = docs.len();
    if n == 0 {
        return Vec::new();
    }
    let query_terms = query_terms(query_text);
    let t = query_terms.len();
    let ids: Vec<Option<u32>> = query_terms.iter().map(|q| index.vocab.get(q).copied()).collect();

    let mut counts = vec![0u32; n * t];
    let mut lengths = vec![0usize; n];
    let mut buffer = String::new();
    for (i, doc) in docs.iter().enumerate() {
        match index.docs.get(&doc.id) {
            Some(cached) => {
                lengths[i] = cached.length;
                for (j, id) in ids.iter().enumerate() {
                    let Some(id) = id else { continue };
                    if let Ok(k) = cached.terms.binary_search_by_key(id, |(term, _)| *term) {
                        counts[i * t + j] = cached.terms[k].1;
                    }
                }
            }
            None => {
                let mut length = 0usize;
                for_each_token(&doc.text, &mut buffer, |token| {
                    length += 1;
                    if let Some(j) = query_terms.iter().position(|q| q == token) {
                        counts[i * t + j] += 1;
                    }
                });
                lengths[i] = length;
            }
        }
    }
    bm25(n, t, &counts, &lengths)
}

/// Squash a raw BM25 score into `[0, 1)`. See [`BM25_SATURATION`].
pub fn saturate(raw: f32) -> f32 {
    let s = raw as f64;
    if s <= 0.0 {
        return 0.0;
    }
    (s / (s + BM25_SATURATION)) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::MATURATION_WINDOW_MS;
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
            tags: Default::default(),
        }
    }

    /// **ADR-051's split, pinned bit for bit.** `score_all` became a projection over
    /// `score_texts`; if the two ever stop being the same arithmetic, the retrieval path has
    /// silently acquired a second BM25 — which is the specific thing the split exists to prevent.
    ///
    /// `assert_eq` on `f32`, not an epsilon: the claim is *identical*, not *close*. `repro`
    /// compares two runs byte for byte, so "close" is not the property the scored path needs.
    #[test]
    fn document_frequencies_count_documents_not_occurrences() {
        let mut ix = LexicalIndex::default();
        ix.upsert("a", "gate gate ablation");
        ix.upsert("b", "the gate features");
        ix.upsert("c", "unrelated");
        assert_eq!(ix.document_frequencies(&["gate", "ablation", "missing", "gate"]), vec![2, 1, 0, 2]);
        ix.remove("b");
        assert_eq!(ix.document_frequencies(&["gate"]), vec![1]);
    }

    #[test]
    fn the_belief_path_and_the_text_path_are_the_same_arithmetic() {
        let entries = [
            entry("a", "moved off Postgres in April 2023"),
            entry("b", "the database migration finished"),
            entry("c", "nothing to do with any of it"),
            entry("d", ""),
        ];
        let refs: Vec<&MemoryEntry> = entries.iter().collect();
        let texts: Vec<&str> = entries.iter().map(|e| e.text.as_str()).collect();

        for query in ["postgres migration", "april 2023", "", "unrelated words entirely"] {
            assert_eq!(
                score_all(&refs, query),
                score_texts(&texts, query),
                "the belief path and the text path diverged on {query:?}"
            );
        }

        // The control: without it, two functions that both returned all-zeros would satisfy
        // every assertion above.
        let scored = score_all(&refs, "postgres migration");
        assert!(
            scored.iter().any(|s| *s > 0.0),
            "no candidate scored above zero, so the equality above compared nothing"
        );
    }

    #[test]
    fn the_tokenizer_does_what_it_says() {
        assert_eq!(tokenize("Hello, World!"), vec!["hello", "world"]);
        assert_eq!(
            tokenize("moved off Postgres in April 2023"),
            vec!["moved", "off", "postgres", "in", "april", "2023"],
            "digits are kept: temporal-reasoning turns on them"
        );
        assert_eq!(tokenize("v1.2.3"), vec!["v1", "2", "3"]);
        assert_eq!(tokenize(""), Vec::<String>::new());
        assert_eq!(tokenize("   ,,,   "), Vec::<String>::new());
    }

    #[test]
    fn no_stemming_and_no_stop_list() {
        // Both are frozen-artifact decisions, not defaults. Asserted so adding either is a
        // deliberate act that breaks a test rather than a quiet quality tweak.
        assert_eq!(tokenize("running runs run"), vec!["running", "runs", "run"]);
        assert_eq!(tokenize("the a of"), vec!["the", "a", "of"]);
    }

    #[test]
    fn a_matching_document_outscores_a_non_matching_one() {
        let a = entry("m-a", "the ingest job times out on the nightly run");
        let b = entry("m-b", "we had pasta for dinner");
        let docs = vec![&a, &b];
        let scores = score_all(&docs, "why is the ingest job timing out");
        assert!(scores[0] > scores[1], "{scores:?}");
        assert!(scores[0] > 0.0);
    }

    #[test]
    fn a_query_sharing_no_vocabulary_scores_zero_everywhere() {
        // This is the single-cue failure mode brief §5 names, asserted rather than assumed:
        // "fails when relevance is not similarity". A dense cue is what closes it.
        let a = entry("m-a", "the ingest job times out on the nightly run");
        let b = entry("m-b", "we had pasta for dinner");
        let docs = vec![&a, &b];
        let scores = score_all(&docs, "quarterly headcount forecast");
        assert_eq!(scores, vec![0.0, 0.0]);
    }

    #[test]
    fn idf_is_never_negative() {
        // A term in every document must contribute a small positive amount, never a penalty.
        let a = entry("m-a", "postgres postgres postgres");
        let b = entry("m-b", "postgres");
        let docs = vec![&a, &b];
        let scores = score_all(&docs, "postgres");
        assert!(scores.iter().all(|s| *s >= 0.0), "{scores:?}");
        assert!(scores[0] > scores[1], "higher tf still ranks higher");
    }

    #[test]
    fn scoring_is_deterministic_across_calls() {
        let a = entry("m-a", "the ingest job times out on the nightly run");
        let b = entry("m-b", "the nightly run was rescheduled");
        let c = entry("m-c", "we had pasta for dinner");
        let docs = vec![&a, &b, &c];
        let first = score_all(&docs, "nightly ingest run");
        let second = score_all(&docs, "nightly ingest run");
        assert_eq!(first, second);
    }

    #[test]
    fn an_empty_candidate_set_scores_nothing_rather_than_dividing_by_zero() {
        assert_eq!(score_all(&[], "anything"), Vec::<f32>::new());
        let empty = entry("m-a", "");
        assert_eq!(score_all(&[&empty], "anything"), vec![0.0]);
    }

    #[test]
    fn saturation_is_absolute_not_relative_to_the_candidate_set() {
        // The property the gate depends on: a query where nothing matches well must produce a
        // LOW feature value, not a 1.0 for whichever candidate matched least badly. Min-max
        // normalization would return 1.0 for the top document in both cases below.
        let good = saturate(30.0);
        let poor = saturate(0.4);
        assert!(good > 0.7, "{good}");
        assert!(poor < 0.1, "{poor}");
        assert_eq!(saturate(0.0), 0.0);
        assert_eq!(saturate(-1.0), 0.0, "a negative score is no evidence, not anti-evidence");
        // Half-open in real arithmetic, closed after the f32 cast. Asserted as it behaves.
        assert!(saturate(100.0) < 1.0, "a realistic score stays inside the range");
        assert_eq!(saturate(f32::MAX), 1.0, "and an unreachable one saturates exactly");
    }

    #[test]
    fn saturation_is_monotone() {
        let mut previous = -1.0f32;
        for raw in [0.0f32, 0.1, 1.0, 5.0, 10.0, 50.0, 500.0] {
            let s = saturate(raw);
            assert!(s > previous || (raw == 0.0 && s == 0.0), "{raw} -> {s}");
            previous = s;
        }
    }

    /// **The cache changes no arithmetic.** `assert_eq` on `f32`, not an epsilon, across
    /// queries with repeated, absent, digit and non-ASCII terms, a doc set that is a subset of
    /// the index, documents re-indexed after an edit, and one missing from the index entirely.
    #[test]
    fn the_indexed_path_is_bit_identical_to_tokenizing_every_query() {
        let mut entries = vec![
            entry("a", "moved off Postgres in April 2023, Postgres was slow"),
            entry("b", "the database migration finished; the migration took 3 days"),
            entry("c", "nothing to do with any of it"),
            entry("d", ""),
            entry("e", "Café crème at the café: v1.2.3 shipped"),
            entry("f", "postgres postgres postgres migration"),
            entry("g", "the ingest job times out on the nightly run"),
        ];
        let mut index = LexicalIndex::default();
        for e in &entries {
            index.upsert(&e.id, &e.text);
        }
        // An edit after indexing, re-indexed the way the store does it.
        entries[2].text = "a late note about the postgres migration".into();
        index.upsert("c", &entries[2].text);
        // And a tombstone: text cleared, re-indexed as empty.
        entries[6].text.clear();
        index.upsert("g", "");
        // One entry the index has never seen.
        let stranger = entry("z", "postgres in april, and café");

        let queries = [
            "postgres migration",
            "april 2023 postgres postgres",
            "",
            "unrelated words entirely",
            "café v1 3",
            "THE the The",
            "ingest nightly",
        ];
        let all: Vec<&MemoryEntry> = entries.iter().chain(std::iter::once(&stranger)).collect();
        let subsets: [Vec<&MemoryEntry>; 3] = [
            all.clone(),
            vec![all[0], all[4], all[7]],
            vec![all[5]],
        ];
        let mut nonzero = false;
        for docs in &subsets {
            for q in queries {
                let fresh = score_all(docs, q);
                let cached = score_indexed(&index, docs, q);
                assert_eq!(
                    fresh.iter().map(|s| s.to_bits()).collect::<Vec<_>>(),
                    cached.iter().map(|s| s.to_bits()).collect::<Vec<_>>(),
                    "diverged on {q:?}"
                );
                nonzero |= fresh.iter().any(|s| *s > 0.0);
            }
        }
        assert!(nonzero, "control: something must have scored above zero");
        assert_eq!(score_indexed(&index, &[], "anything"), Vec::<f32>::new());
    }

    #[test]
    fn reindexing_replaces_rather_than_accumulates() {
        let mut index = LexicalIndex::default();
        index.upsert("a", "one two two");
        index.upsert("a", "two");
        let e = entry("a", "two");
        assert_eq!(score_indexed(&index, &[&e], "two"), score_all(&[&e], "two"));
        assert_eq!(index.len(), 1);
        index.remove("a");
        assert!(index.is_empty());
        assert_eq!(index.vocabulary(), 2, "term ids are never reused");
    }
}
