use std::path::{Path, PathBuf};

use super::*;

fn args(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn required_flags_have_no_default() {
    assert!(parse(&args(&["--embedder-model", "m", "--reranking", "off"])).is_err());
    assert!(parse(&args(&["--profile-root", "p", "--reranking", "off"])).is_err());
    let e = parse(&args(&["--profile-root", "p", "--embedder-model", "m"])).unwrap_err();
    assert!(e.contains("--reranking"), "{e}");
}

#[test]
fn shipped_defaults_and_explicit_off() {
    let a = parse(&args(&["--profile-root", "p", "--embedder-model", "m", "--reranking", "off"])).unwrap();
    assert_eq!(a.reranking, None);
    assert_eq!(a.rerank_threads, SHIPPED_THREADS);
    assert_eq!(a.rerank_batch, None);
    assert_eq!(a.coverage, Coverage::Declared);
    let a = parse(&args(&[
        "--profile-root", "p", "--embedder-model", "m", "--reranking", "r",
        "--rerank-batch", "on", "--injection-coverage", "full", "--dump-gate-features", "d.ndjson",
    ]))
    .unwrap();
    assert_eq!(a.reranking, Some(PathBuf::from("r")));
    assert_eq!(a.rerank_batch, Some(true));
    assert_eq!(a.coverage, Coverage::Full);
    assert_eq!(a.dump_gate_features, Some(PathBuf::from("d.ndjson")));
}

#[test]
fn removed_and_malformed_flags_are_refused() {
    let base = ["--profile-root", "p", "--embedder-model", "m", "--reranking", "off"];
    for extra in [
        &["--fit-mode"][..],
        &["--rerank-plan", "cascade"],
        &["--rerank-provider", "cuda"],
        &["--rerank-threads", "0"],
        &["--rerank-batch", "maybe"],
        &["--injection-coverage", "most"],
        &["--dump-gate-features"],
    ] {
        let mut v = base.to_vec();
        v.extend_from_slice(extra);
        assert!(parse(&args(&v)).is_err(), "{extra:?} should be refused");
    }
}

fn models() -> Option<(PathBuf, PathBuf)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("models");
    let e = root.join("jina-embeddings-v2-small-en");
    let r = root.join("ms-marco-MiniLM-L-2-v2-ft-session-j");
    (e.join("model.onnx").exists() && r.join("model.onnx").exists()).then_some((e, r))
}

/// The whole shipped pipeline over the real wire: ingest, retrieve, answer. CPU, tiny input.
/// Skips (passes) when the git-ignored models are absent.
#[test]
fn round_trip_over_the_wire_with_the_shipped_pipeline() {
    let Some((emb, rr)) = models() else {
        eprintln!("models/ absent; skipping the end-to-end adapter test");
        return;
    };
    let tmp = std::env::temp_dir().join(format!("marlowe-eval-adapter-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let root = tmp.join("profile");
    let dump = tmp.join("dump.ndjson");
    std::fs::create_dir_all(&tmp).unwrap();

    let embedder = Embedder::load(&emb, 1, None).unwrap();
    let encoder = CrossEncoder::load(&rr).unwrap();
    let mut adapter = Adapter::start(
        &root,
        embedder,
        RerankSettings { encoder: Some(encoder), batched: false },
        Coverage::Declared,
        Some(&dump),
    )
    .unwrap();

    let t0: i64 = 1_700_000_000_000;
    let turns: Vec<serde_json::Value> = [
        "I adopted a beagle named Biscuit last spring.",
        "My sister lives in Lisbon and teaches piano.",
        "I switched my commute to a folding bike in March.",
        "The quarterly report is due on the fourteenth.",
    ]
    .iter()
    .enumerate()
    .map(|(i, text)| {
        serde_json::json!({
            "turn_id": format!("t{i}"), "speaker": "user", "text": text,
            "occurred_at_ms": t0 + i as i64 * 60_000,
            "origin": {"channel": "terminal", "actor": "user"}
        })
    })
    .collect();
    let ingest = serde_json::json!({"op": "ingest", "body": {
        "contract_version": "1.0", "clock": {"now_ms": t0 + 600_000},
        "session_id": "q1", "turns": turns}});
    let mut out = Vec::new();
    // Seven hours later: past any maturation window.
    let ask = t0 + 7 * 3_600_000;
    let answer = serde_json::json!({"op": "answer", "body": {
        "contract_version": "1.0", "clock": {"now_ms": ask},
        "query_id": "q1", "session_id": "q1", "question": "What is my dog called?"}});
    let input = format!("{ingest}\n{answer}\nnot json\n");
    adapter.run(input.as_bytes(), &mut out).unwrap();

    let lines: Vec<serde_json::Value> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["op"], "ingest");
    assert_eq!(lines[0]["body"]["written"].as_array().unwrap().len(), 4);
    let retrieval = &lines[1]["body"]["retrieval"];
    assert_eq!(lines[1]["body"]["abstention_reason"], "degraded_path");
    assert_eq!(retrieval["gate"]["version"], marlowe_memory::GATE_VERSION);
    assert!(retrieval["cost"]["latency_ms"]["total"].is_i64());
    assert_eq!(lines[2]["error"]["kind"], "malformed_frame", "{}", lines[2]);

    // The dump carries the shipped ranking key for all four candidates, reranked ones non-null.
    let rows: Vec<serde_json::Value> = std::fs::read_to_string(&dump)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(rows.len(), 4);
    for key in ["score", "margin", "calibrated_precision", "survived_pruning", "lexical_bm25", "dense_cosine"] {
        assert!(rows.iter().all(|r| r.get(key).is_some()), "dump row missing {key}");
    }
    assert!(rows.iter().any(|r| r["rerank_score"].is_f64()));
    let _ = std::fs::remove_dir_all(&tmp);
}
