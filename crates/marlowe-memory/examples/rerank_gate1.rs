//! **Gate 1: does running the reranker on CUDA change which memory ranks first?**
//!
//! ```text
//! cargo run -p marlowe-memory --release --example rerank_gate1
//! ```
//!
//! # Why this is not a scoring run
//!
//! The obvious way to answer it is to score the whole fit split twice. That is two full passes, and
//! an earlier attempt at exactly that was killed and left a **0-byte** `scored-candidates.ndjson` —
//! worse than no attempt, because a zero-byte file sitting in a results directory reads like a
//! result.
//!
//! The reranker is a **pure function of `(query, document) -> logit`**. Retrieval, pruning and the
//! gate are not under test here; the only question is whether the same pair scores differently on
//! the two providers, and whether any such difference is large enough to reorder a query's
//! candidates. So this scores real pairs on both providers directly and counts **top-1 changes**,
//! which is the quantity the verdict rule is stated in.
//!
//! # The controls, which are what make the number mean anything
//!
//! *"Zero flips"* is exactly what a comparison of one provider **with itself** prints. Three times
//! this session a measurement was nearly published that could not have detected the thing it was
//! looking for. So before reporting anything this checks:
//!
//! 1. the two arms report **different** providers, and
//! 2. the flip detector **does** notice a forced reordering.
//!
//! Either failing prints `VACUOUS` and exits non-zero rather than reporting a clean result.
//!
//! # The verdict rule, fixed before the run
//!
//! **Zero top-1 changes -> gate 1 passes.** Any top-1 change -> do not flip the default.
//!
//! Expect zero: STATE.md records that post-hoc mechanisms on this corpus gain cases almost entirely
//! inside logit gaps below **0.084**, and CUDA's measured deviation on this graph is ~0.001 — two
//! orders of magnitude under. That is a reason to predict zero, never a substitute for counting.

use std::collections::BTreeMap;

use marlowe_memory::rerank::{CrossEncoder, RerankProvider, SHIPPED_THREADS};

/// Pairs come from a **completed** dump — one carrying `core.sha256`, so it is not a truncated run.
const DUMP: &str = "runs/session-e-maxseq/fit-1024/fit/scored-candidates.ndjson";
const CORPUS: &str = "data/longmemeval_s_cleaned.json";
/// Enough queries that a reordering has room to happen, few enough to finish in minutes.
const MAX_QUERIES: usize = 120;
/// **10, not 12, and the bound is not arbitrary.** `score_batch` REFUSES a batch above the sizes
/// invariance was measured at (`runs/session-l/batch-invariance-batch10.json`, 1..10). The first
/// run of this file asked for 12, every scoring call was refused, and it printed
/// `0 flips, max delta 0.000000000` — a clean PASS over ZERO comparisons. The guard was right and
/// the measurement was empty.
const MAX_PER_QUERY: usize = 10;

fn main() {
    let dir = std::path::Path::new("models/ms-marco-MiniLM-L-2-v2-ft-session-j");

    let Some((questions, texts)) = load_corpus() else {
        eprintln!("SKIP: {CORPUS} not found (data/ is gitignored and never vendored)");
        return;
    };
    let pairs = load_pairs(&questions, &texts);
    if pairs.is_empty() {
        eprintln!("SKIP: no pairs recovered from {DUMP}");
        return;
    }
    let total: usize = pairs.values().map(Vec::len).sum();
    println!("{} queries, {total} pairs, from a completed dump", pairs.len());

    let mut cpu = match CrossEncoder::load_with(dir, SHIPPED_THREADS, RerankProvider::Cpu) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("SKIP: the CPU reranker did not load: {e}");
            return;
        }
    };
    let mut cuda = match CrossEncoder::load_with(dir, SHIPPED_THREADS, RerankProvider::Cuda) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("SKIP: CUDA did not construct: {e}");
            eprintln!("      set MARLOWE_CUDA_LIB_DIR — torch's lib dir holds the CUDA 12 runtime");
            return;
        }
    };

    // CONTROL 1: the arms are genuinely different providers.
    if cpu.provider() == cuda.provider() {
        println!("\nVACUOUS: both arms report {:?}", cpu.provider());
        std::process::exit(2);
    }
    println!("arms: {:?} vs {:?}", cpu.provider(), cuda.provider());

    // CONTROL 2: the flip detector notices a forced reordering.
    let probe = [0.10f32, 0.11, 0.09];
    let mut nudged = probe;
    nudged[2] = 0.99;
    if argmax(&probe) == argmax(&nudged) {
        println!("\nVACUOUS: the flip detector did not notice a forced reordering");
        std::process::exit(2);
    }

    // CONTROL 3, added after the first run of this file reported a clean PASS over zero
    // comparisons. Providers differing and the detector working say nothing about whether a single
    // pair was scored, and neither of the first two controls could see that.
    let mut compared = 0usize;
    let mut flips = 0usize;
    let mut reorders = 0usize;
    let mut max_delta = 0.0f32;
    let mut worst = String::new();

    for (qid, items) in &pairs {
        let query = &questions[qid];
        let docs: Vec<&str> = items.iter().map(String::as_str).collect();
        let a = match cpu.score_batch(query, &docs) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("cpu scoring failed on {qid}: {e}");
                continue;
            }
        };
        let b = match cuda.score_batch(query, &docs) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("cuda scoring failed on {qid}: {e}");
                continue;
            }
        };
        compared += a.len();
        for (x, y) in a.iter().zip(b.iter()) {
            let d = (x - y).abs();
            if d > max_delta {
                max_delta = d;
                worst = qid.clone();
            }
        }
        if argmax(&a) != argmax(&b) {
            flips += 1;
            println!("  TOP-1 CHANGED on {qid}");
        }
        if order(&a) != order(&b) {
            reorders += 1;
        }
    }

    // CONTROL 3, and it is the one the first run of this file needed. Providers differing and the
    // flip detector working say nothing about whether a single pair was ever scored — and the first
    // run printed a clean `PASS, 0 flips, max delta 0.000000000` while EVERY scoring call was being
    // refused for asking a batch of 12 against an invariance measured to 10. The guard was right;
    // the measurement was empty; and neither existing control could see it.
    if compared == 0 {
        println!("\nVACUOUS: not one pair was scored. Any verdict below would be about nothing.");
        std::process::exit(2);
    }

    println!("\n=== GATE 1 ===");
    println!("pairs scored on BOTH providers                   {compared}");
    println!("queries compared                                 {}", pairs.len());
    println!("max |delta logit|                                {max_delta:.9}  (worst: {worst})");
    println!("queries with ANY reorder in their candidate list  {reorders}");
    println!("queries whose TOP-1 changed                       {flips}");
    println!(
        "\nVERDICT: {}",
        if flips == 0 {
            "PASS - zero top-1 changes. Under the rule fixed in advance, the flip is licensed."
        } else {
            "FAIL - a top-1 moved. Do NOT flip the default."
        }
    );
    if flips != 0 {
        std::process::exit(1);
    }
}

fn argmax(v: &[f32]) -> usize {
    let mut best = 0usize;
    for (i, x) in v.iter().enumerate() {
        if *x > v[best] {
            best = i;
        }
    }
    best
}

fn order(v: &[f32]) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..v.len()).collect();
    idx.sort_by(|a, b| v[*b].partial_cmp(&v[*a]).unwrap_or(std::cmp::Ordering::Equal));
    idx
}

fn load_corpus() -> Option<(BTreeMap<String, String>, BTreeMap<String, String>)> {
    let raw = std::fs::read_to_string(CORPUS).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let mut questions = BTreeMap::new();
    let mut texts = BTreeMap::new();
    for case in v.as_array()? {
        let qid = case.get("question_id")?.as_str()?.to_string();
        questions.insert(qid.clone(), case.get("question")?.as_str()?.to_string());
        let sessions = case.get("haystack_sessions").and_then(|s| s.as_array());
        let ids = case.get("haystack_session_ids").and_then(|s| s.as_array());
        for (si, sess) in sessions.into_iter().flatten().enumerate() {
            let sid = ids
                .and_then(|i| i.get(si))
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            for (ti, turn) in sess.as_array().into_iter().flatten().enumerate() {
                if let Some(c) = turn.get("content").and_then(|c| c.as_str()) {
                    texts.insert(format!("{qid}|{sid}|{ti}"), c.to_string());
                }
            }
        }
    }
    Some((questions, texts))
}

/// Recover `(query, document)` pairs by joining the dump's `memory_id` back to its turn.
///
/// The id is `m-{qid}-{session}-{turn}-{index}`, and the session name itself contains hyphens, so
/// it is split from the RIGHT — taking the last two fields as turn and index and everything before
/// as the session. Splitting from the left would silently mis-key every ShareGPT session.
fn load_pairs(
    questions: &BTreeMap<String, String>,
    texts: &BTreeMap<String, String>,
) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let Ok(file) = std::fs::read_to_string(DUMP) else { return out };
    for line in file.lines() {
        let Ok(row) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let Some(qid) = row.get("query_id").and_then(|q| q.as_str()) else { continue };
        if !questions.contains_key(qid) {
            continue;
        }
        if !out.contains_key(qid) && out.len() >= MAX_QUERIES {
            continue;
        }
        let Some(mid) = row.get("memory_id").and_then(|m| m.as_str()) else { continue };
        let rest = mid.strip_prefix("m-").unwrap_or(mid);
        let rest = rest.strip_prefix(qid).unwrap_or(rest);
        let rest = rest.strip_prefix('-').unwrap_or(rest);
        let fields: Vec<&str> = rest.rsplitn(3, '-').collect();
        if fields.len() < 3 {
            continue;
        }
        let (session, turn) = (fields[2], fields[1]);
        if let Some(text) = texts.get(&format!("{qid}|{session}|{turn}")) {
            let entry = out.entry(qid.to_string()).or_default();
            if entry.len() < MAX_PER_QUERY {
                entry.push(text.clone());
            }
        }
    }
    out.retain(|_, v| v.len() >= 2);
    out
}
