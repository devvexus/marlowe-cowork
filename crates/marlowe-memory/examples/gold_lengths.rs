//! **Does lowering `MAX_SEQ_LEN` truncate any GOLD turn?**
//!
//! The distribution says 0.18% of all turns exceed 1024. That is the right number for memory and
//! the wrong number for capability: what decides whether retrieval can still find an answer is
//! whether any turn **that contains an answer** loses its tail.
//!
//! A truncated distractor can only change R@1 by moving its own score; a truncated *gold* turn can
//! lose the very span the query is looking for. The two are not the same risk and the pooled
//! figure hides the distinction.

use marlowe_memory::cue::dense::tokenizer::{encode, Vocab};

const CEILING: usize = 1_000_000;

fn main() {
    let vocab_path = "models/jina-embeddings-v2-small-en/vocab.txt";
    let vocab_text = std::fs::read_to_string(vocab_path).expect("vocab");
    let vocab = Vocab::parse(&vocab_text, vocab_path).expect("parses");

    let raw = std::fs::read_to_string("/tmp/gold_turns.json").expect("gold turns");
    let gold: Vec<String> = serde_json::from_str(&raw).expect("json");

    let mut lengths: Vec<usize> = gold
        .iter()
        .map(|t| encode(&vocab, t, CEILING).input_ids.len())
        .collect();
    lengths.sort_unstable();
    let n = lengths.len();
    let pct = |p: f64| lengths[(((n as f64 - 1.0) * p) as usize).min(n - 1)];

    println!("GOLD TURNS (the ones that contain an answer)");
    println!("n = {n}, max = {}", lengths[n - 1]);
    println!("p50 {}  p90 {}  p95 {}  p99 {}", pct(0.50), pct(0.90), pct(0.95), pct(0.99));
    println!("\n  cap  | gold truncated |    % | gold tokens lost");
    println!("  -----|----------------|------|------------------");
    let total: usize = lengths.iter().sum();
    for cap in [256usize, 512, 768, 1024, 2048, 4096, 8192] {
        let over = lengths.iter().filter(|l| **l > cap).count();
        let lost: usize = lengths.iter().map(|l| l.saturating_sub(cap)).sum();
        println!(
            "  {cap:>4} | {over:>14} | {:>4.2} | {lost} ({:.3}%)",
            100.0 * over as f64 / n as f64,
            100.0 * lost as f64 / total as f64
        );
    }
}
