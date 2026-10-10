//! The six-graph cascade through the real retrieval path, on CPU (the GPU is a speed, not a
//! different ranking: the per-provider check is `tests/gpu_cuda.rs`).
//!
//! Skips when any fusion graph is absent (`scripts/fetch-models.ps1 -Cascade`).

use std::path::{Path, PathBuf};

use marlowe_contract::{Fidelity, PayloadKind, TrustClass};
use marlowe_memory::cascade::{Cascade, CASCADE_NARROW, FUSION_GRAPHS};
use marlowe_memory::cue::dense::vectors::VectorStore;
use marlowe_memory::rerank::{CrossEncoder, RerankProvider};
use marlowe_memory::retrieve::{select_for_injection, Rerank, RetrievalScope, Scoring};
use marlowe_memory::{BeliefStore, Coverage, FrozenGate, MemoryEntry, OperatingPoint, MATURATION_WINDOW_MS};

fn models() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("models");
    let all = FUSION_GRAPHS.iter().all(|(n, _)| dir.join(n).join("model.onnx").is_file());
    if !all {
        eprintln!("SKIP: a fusion graph is missing under {} (fetch-models.ps1 -Cascade)", dir.display());
    }
    all.then_some(dir)
}

fn entry(id: &str, text: &str, created: i64) -> MemoryEntry {
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
        occurred_at_ms: created,
        created_at: created,
        last_accessed: created,
        access_count: 0,
        confidence: 1.0,
        activation: 1.0,
        fidelity: Fidelity::Record,
        silent_until: Some(created + MATURATION_WINDOW_MS),
        supersedes: Vec::new(),
        superseded_by: None,
        tags: Default::default(),
    }
}

const TEXTS: &[&str] = &[
    "The nightly ingest job timed out again because the warehouse was locked.",
    "We moved the analytics warehouse off Postgres in April.",
    "The warehouse migration to ClickHouse finished last week.",
    "I had pasta for dinner and watched a film.",
    "The ingest job retries three times before paging anyone.",
    "Our database backups run at 2am from the warehouse replica.",
    "My sister is visiting in June.",
    "The analytics team asked which database the warehouse uses now.",
    "Postgres kept running out of connections during ingest.",
    "We chose ClickHouse for the warehouse because of columnar scans.",
    "The cat knocked a plant off the windowsill.",
    "Training loss plateaued after the learning-rate warmup.",
    "The warehouse dashboards load in under a second since the move.",
    "I need to renew my passport before the trip.",
];

#[test]
fn under_the_cascade_rerank_score_stays_the_shipped_logit_and_the_ten_are_fused() {
    let Some(dir) = models() else { return };
    let mut primary = CrossEncoder::load(&dir.join(FUSION_GRAPHS[0].0)).expect("shipped graph");
    let mut cascade = Cascade::load(&dir, 1, RerankProvider::Cpu).expect("fusion members");
    assert_eq!(cascade.members_mut().len(), 5);

    let mut beliefs = BeliefStore::default();
    for (i, t) in TEXTS.iter().enumerate() {
        beliefs.insert(entry(&format!("m-{i:02}"), t, 1_000 + i as i64 * 1_000));
    }
    let now = 100_000 + MATURATION_WINDOW_MS;
    let gate = FrozenGate::load().expect("shipped gate");
    let point = OperatingPoint::load().expect("shipped operating point");
    let query = "which database does the analytics warehouse use now";
    let run = |primary: &mut CrossEncoder, cascade: &mut Cascade, batched: bool| {
        let sel = select_for_injection(
            &beliefs,
            "s-1",
            query,
            now,
            7000,
            &Scoring::Gated { gate: &gate, operating_point: &point, coverage: Coverage::Declared },
            &VectorStore::default(),
            None,
            &mut Rerank::Cascade { encoder: primary, fusion: cascade.members_mut(), batched },
            RetrievalScope::ThisSession,
        );
        let rows: Vec<(String, Option<f32>, Option<u32>)> =
            sel.scored.iter().map(|c| (c.entry.id.clone(), c.rerank_score, c.fusion_rank)).collect();
        (sel.reranked, rows)
    };
    let (reranked, rows) = run(&mut primary, &mut cascade, false);
    assert_eq!(reranked as usize, TEXTS.len(), "the whole (sub-30) slate is scored by the shipped graph");

    let mut ranks: Vec<u32> = rows.iter().filter_map(|r| r.2).collect();
    ranks.sort_unstable();
    assert_eq!(ranks, (0..CASCADE_NARROW as u32).collect::<Vec<_>>(), "exactly the narrowed ten carry fusion ranks");

    // `rerank_score` is the shipped graph's own logit, bit for bit, not a fusion score.
    for (id, logit, _) in &rows {
        let text = TEXTS[id[2..].parse::<usize>().unwrap()];
        let direct = primary.score(query, text).unwrap();
        assert_eq!(logit.unwrap().to_bits(), direct.to_bits(), "{id}");
    }

    // The narrowed ten are the shipped graph's top ten.
    let mut by_logit: Vec<&(String, Option<f32>, Option<u32>)> = rows.iter().collect();
    by_logit.sort_by(|a, b| b.1.unwrap().total_cmp(&a.1.unwrap()).then_with(|| a.0.cmp(&b.0)));
    assert!(by_logit[..CASCADE_NARROW].iter().all(|r| r.2.is_some()));
    assert!(by_logit[CASCADE_NARROW..].iter().all(|r| r.2.is_none()));

    // Batching inside the measured envelope changes nothing.
    let (_, batched) = run(&mut primary, &mut cascade, true);
    assert_eq!(rows, batched, "batched and sequential cascades agree bit for bit");
}
