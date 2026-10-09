//! **What sequence length does the corpus actually need?**
//!
//! `MAX_SEQ_LEN` is 8192 because that is the model's ALiBi capacity, and at 8192 this export
//! materialises an `[8, N, N]` int64 relative-position matrix — **4.29 GB**, which is why the two
//! embedder reference tests fail on a machine without that much contiguous RAM free.
//!
//! Lowering it is a one-constant change. What decides the value is not a preference, it is this
//! distribution: the fraction of real inputs that would be truncated at each candidate. So this
//! measures it and prints the truncation cost per candidate, and the choice follows from the table.
//!
//! **It runs the production tokenizer**, not a Python reimplementation, so the counts cannot
//! disagree with what the embedder will actually see.
//!
//! ```text
//! cargo run -p marlowe-memory --release --example token_lengths -- data/longmemeval_s_cleaned.json
//! ```

use std::collections::BTreeMap;

use marlowe_memory::cue::dense::tokenizer::{encode, Vocab};

/// Far above the model's own capacity, so nothing is truncated while measuring the distribution.
/// The point is to see the true tail, including whatever exceeds 8192.
const MEASURE_CEILING: usize = 1_000_000;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let corpus = args.get(1).map(String::as_str).unwrap_or("data/longmemeval_s_cleaned.json");
    let vocab_path = args
        .get(2)
        .map(String::as_str)
        .unwrap_or("models/jina-embeddings-v2-small-en/vocab.txt");

    let vocab_text = std::fs::read_to_string(vocab_path).expect("vocab.txt");
    let vocab = Vocab::parse(&vocab_text, vocab_path).expect("vocab parses");

    let raw = std::fs::read_to_string(corpus).expect("corpus");
    let cases: serde_json::Value = serde_json::from_str(&raw).expect("corpus json");
    let cases = cases.as_array().expect("a list of cases");

    // **The retrieval unit is the TURN**, which is what `ingest` embeds — not the session and not
    // the case. Measuring anything else would answer a question nobody asked.
    let mut lengths: Vec<usize> = Vec::new();
    for case in cases {
        let Some(sessions) = case.get("haystack_sessions").and_then(|v| v.as_array()) else {
            continue;
        };
        for session in sessions {
            let Some(turns) = session.as_array() else { continue };
            for turn in turns {
                let Some(content) = turn.get("content").and_then(|v| v.as_str()) else { continue };
                lengths.push(encode(&vocab, content, MEASURE_CEILING).input_ids.len());
            }
        }
    }
    // Questions are embedded too, on the query side.
    let mut question_lengths: Vec<usize> = Vec::new();
    for case in cases {
        if let Some(q) = case.get("question").and_then(|v| v.as_str()) {
            question_lengths.push(encode(&vocab, q, MEASURE_CEILING).input_ids.len());
        }
    }

    report("TURNS (the retrieval unit)", &mut lengths);
    report("QUESTIONS (the query side)", &mut question_lengths);
}

fn report(what: &str, lengths: &mut Vec<usize>) {
    lengths.sort_unstable();
    let n = lengths.len();
    if n == 0 {
        println!("{what}: nothing measured");
        return;
    }
    let pct = |p: f64| lengths[(((n as f64 - 1.0) * p) as usize).min(n - 1)];
    let total: usize = lengths.iter().sum();

    println!("\n=== {what} ===");
    println!("n = {n}, mean = {:.1}, max = {}", total as f64 / n as f64, lengths[n - 1]);
    println!(
        "p50 {}  p90 {}  p95 {}  p99 {}  p99.9 {}",
        pct(0.50),
        pct(0.90),
        pct(0.95),
        pct(0.99),
        pct(0.999)
    );

    // **The cost of each candidate, in the only terms that matter: what gets truncated, and what
    // the ALiBi matrix costs.** 8 heads, int64, N x N — the tensor that is 4.29 GB at 8192.
    println!("\n  cap  | truncated turns |    % | ALiBi matrix");
    println!("  -----|-----------------|------|-------------");
    let mut counts: BTreeMap<usize, usize> = BTreeMap::new();
    for cap in [128usize, 256, 384, 512, 768, 1024, 2048, 4096, 8192] {
        let over = lengths.iter().filter(|l| **l > cap).count();
        counts.insert(cap, over);
        let bytes = 8u64 * (cap as u64) * (cap as u64) * 8;
        println!(
            "  {cap:>4} | {over:>15} | {:>4.2} | {}",
            100.0 * over as f64 / n as f64,
            human(bytes)
        );
    }

    // How much TEXT is lost, not just how many turns are clipped — a turn truncated by three
    // tokens and one truncated by three thousand are not the same event.
    println!("\n  cap  | tokens lost | % of all tokens");
    println!("  -----|-------------|----------------");
    for cap in [256usize, 512, 1024, 2048] {
        let lost: usize = lengths.iter().map(|l| l.saturating_sub(cap)).sum();
        println!("  {cap:>4} | {lost:>11} | {:>14.3}", 100.0 * lost as f64 / total as f64);
    }
    let _ = counts;
}

fn human(bytes: u64) -> String {
    const K: f64 = 1024.0;
    let b = bytes as f64;
    if b >= K * K * K {
        format!("{:.2} GB", b / (K * K * K))
    } else if b >= K * K {
        format!("{:.1} MB", b / (K * K))
    } else {
        format!("{:.0} KB", b / K)
    }
}
