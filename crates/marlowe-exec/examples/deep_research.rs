//! A simulated deep-research document pass: **fetch, extract and analyse a real corpus with no
//! model in the loop.**
//!
//! This is the workload the extraction path exists for — heterogeneous sources, mixed formats,
//! real network, real PDFs — measured rather than asserted.
//!
//! The concurrency sweep runs **after a warm-up pass**, deliberately. Without one, level 1 pays
//! every TLS handshake and DNS lookup and every later level inherits a warm pool, so the
//! "speedup" would be measuring cache warmth and reporting it as parallelism. The handshake and
//! reuse counters are printed per level so that claim is checkable rather than trusted.
//!
//! Run: `cargo run --release -p marlowe-exec --example deep_research`

use std::collections::BTreeMap;
use std::time::Instant;

use marlowe_exec::corpus::{fetch_and_extract, Outcome};

/// A research corpus: encyclopaedia articles, standards documents, language references, and
/// three PDFs including a 2.2 MB paper. Spread across hosts on purpose — a single-host corpus
/// would measure one server's keep-alive behaviour and call it throughput.
const CORPUS: &[&str] = &[
    "https://en.wikipedia.org/wiki/Rust_(programming_language)",
    "https://en.wikipedia.org/wiki/Concurrency_(computer_science)",
    "https://en.wikipedia.org/wiki/Information_retrieval",
    "https://en.wikipedia.org/wiki/Transformer_(deep_learning_architecture)",
    "https://en.wikipedia.org/wiki/Memory_hierarchy",
    "https://en.wikipedia.org/wiki/Transport_Layer_Security",
    "https://en.wikipedia.org/wiki/HTML",
    "https://en.wikipedia.org/wiki/Portable_Document_Format",
    "https://datatracker.ietf.org/doc/html/rfc9110",
    "https://datatracker.ietf.org/doc/html/rfc8446",
    "https://datatracker.ietf.org/doc/html/rfc1951",
    "https://datatracker.ietf.org/doc/html/rfc4180",
    "https://docs.python.org/3/library/json.html",
    "https://docs.python.org/3/library/concurrent.futures.html",
    "https://docs.python.org/3/library/gzip.html",
    "https://doc.rust-lang.org/book/ch02-00-guessing-game-tutorial.html",
    "https://doc.rust-lang.org/book/ch16-00-concurrency.html",
    "https://doc.rust-lang.org/std/sync/struct.Mutex.html",
    "https://www.w3.org/TR/webarch/",
    "https://www.w3.org/TR/html52/",
    "https://blog.rust-lang.org/",
    "https://arxiv.org/pdf/1706.03762",
    "https://arxiv.org/pdf/1810.04805",
    "https://arxiv.org/abs/2005.11401",
];

fn run(urls: &[String], concurrency: usize) -> (f64, Vec<Outcome>) {
    let before = marlowe_net::connection_stats();
    let t = Instant::now();
    let out = fetch_and_extract(urls, concurrency);
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    let after = marlowe_net::connection_stats();
    println!(
        "  concurrency {concurrency:>3}   {ms:>8.0} ms   {:>6.1} docs/s   \
         (+{} handshakes, +{} reused)",
        urls.len() as f64 / (ms / 1000.0),
        after.0 - before.0,
        after.1 - before.1
    );
    (ms, out)
}

fn main() {
    let urls: Vec<String> = CORPUS.iter().map(|s| s.to_string()).collect();
    println!("== SIMULATED DEEP RESEARCH PASS ==\n");
    println!("  corpus     {} documents across {} hosts", urls.len(), {
        let mut h: Vec<&str> = CORPUS
            .iter()
            .filter_map(|u| u.split('/').nth(2))
            .collect();
        h.sort_unstable();
        h.dedup();
        h.len()
    });
    // **`available_parallelism` reports LOGICAL threads, not physical cores.** On an 8-core
    // 7800X3D with SMT it returns 16. Labelling that "cores" would overstate the denominator and
    // make a parallel speedup look worse than it is -- the earlier run in this session did exactly
    // that.
    let threads = std::thread::available_parallelism().map_or(0, |n| n.get());
    println!("  logical threads (available_parallelism)  {threads}");
    println!("  derived I/O concurrency                  {}", marlowe_net::io_concurrency());

    println!("\n-- warm-up pass (populates DNS cache, TLS session cache, connection pool) --");
    let (warm_ms, _) = run(&urls, 0);
    let _ = warm_ms;

    // **Three repeats per level, best-of taken.** The first sweep showed 24 workers beating 16 on
    // one run and losing on another, which is the width of the noise on a live network. A single
    // reading per level cannot tell a real ordering from jitter, and picking a default from one
    // reading is how a theory-derived constant survives a measurement that disagreed with it.
    const REPEATS: usize = 3;
    let levels = [1usize, 4, 8, 12, 16, 20, 24];
    println!("\n-- concurrency sweep, all levels equally warm, best of {REPEATS} --");
    let mut best: Vec<(usize, f64)> = Vec::new();
    let mut results = Vec::new();
    for level in levels {
        let mut fastest = f64::MAX;
        for _ in 0..REPEATS {
            let (ms, out) = run(&urls, level);
            if ms < fastest {
                fastest = ms;
            }
            results = out;
        }
        best.push((level, fastest));
        println!("    level {level:>3} best {fastest:>8.0} ms");
    }

    let serial_ms = best[0].1;
    let cauto = best.iter().map(|(_, m)| *m).fold(f64::MAX, f64::min);
    println!("\n-- scaling (best of {REPEATS}) --");
    for (level, ms) in &best {
        println!(
            "  {level:>5} -> {:>5.2}x vs serial   ({ms:.0} ms, {:.1} docs/s)",
            serial_ms / ms,
            urls.len() as f64 / (ms / 1000.0)
        );
    }
    let optimum = best.iter().min_by(|a, b| a.1.total_cmp(&b.1)).map(|(l, _)| *l).unwrap_or(1);
    println!("\n  MEASURED OPTIMUM: concurrency {optimum}");
    println!("  derived default : {}", marlowe_net::io_concurrency());

    // ── what was actually read ───────────────────────────────────────────────────────────
    let mut by_format: BTreeMap<&str, (usize, usize, usize)> = BTreeMap::new();
    let (mut ok, mut redirect, mut failed, mut unreadable) = (0, 0, 0, 0);
    let mut biggest: Vec<(usize, String, usize)> = Vec::new();
    let (mut wire, mut chars, mut links) = (0usize, 0usize, 0usize);
    let mut warned: Vec<(String, String)> = Vec::new();

    for o in &results {
        match o {
            Outcome::Read { document, wire_bytes, url, .. } => {
                ok += 1;
                biggest.push((*wire_bytes, url.clone(), document.text.len()));
                wire += wire_bytes;
                chars += document.text.len();
                links += document.links.len();
                let e = by_format.entry(document.format.as_str()).or_insert((0, 0, 0));
                e.0 += 1;
                e.1 += *wire_bytes;
                e.2 += document.text.len();
                for w in &document.warnings {
                    warned.push((url.clone(), w.to_string()));
                }
            }
            Outcome::Redirect { .. } => redirect += 1,
            Outcome::Unreachable { .. } => failed += 1,
            Outcome::Unreadable { .. } => unreadable += 1,
        }
    }

    println!("\n-- corpus outcome --");
    println!("  read {ok}   redirect {redirect}   unreachable {failed}   unreadable {unreadable}");
    println!(
        "  {:.2} MB on the wire  ->  {:.2} MB of text   ({} links captured)",
        wire as f64 / 1_048_576.0,
        chars as f64 / 1_048_576.0,
        links
    );
    println!("\n  by format:");
    for (fmt, (n, w, c)) in &by_format {
        println!("    {fmt:<9} {n:>3} docs   {:>8.0} KB wire -> {:>8.0} KB text", *w as f64 / 1024.0, *c as f64 / 1024.0);
    }

    // ── where the time actually went ────────────────────────────────────────────────────
    //
    // These are SUMS across documents, so they exceed the wall clock precisely because the work
    // overlapped -- that gap IS the parallelism. Reporting only the wall time would hide which
    // half the corpus is bound by.
    // **The per-document network/extract split is GONE, and that is deliberate.**
    //
    // `Outcome::Read` carried `fetch_ms` and `extract_ms`, measured with `Instant::now()` inside
    // `corpus.rs`. That is library code on a tool path, and §4.5 forbids it: the loop measures
    // tool calls through an INJECTED `ClockSource` so a test can hold time still, and an executor
    // reading the wall clock behind its back makes a decay-dependent result irreproducible from a
    // component nobody would think to look in. `determinism_guard` caught it two sessions late.
    //
    // The fields were read by nothing but this example, so they went rather than being fenced — a
    // forbidden clock read serving two fields the product never looked at.
    //
    // **What was lost, said plainly rather than quietly dropped:** the summed network-vs-extract
    // split that measured 946 ms against 396 ms and 4.5x overlap for ADR-040. Getting it back
    // means the caller supplying a clock — `marlowe-exec` already depends on `marlowe-loop`, so
    // `ClockSource` is in reach — and that is a deliberate change, not an `Instant::now()` put
    // back where this one was.
    //
    // The wall clock this example reads is its own, which is allowed: a benchmark that cannot read
    // a clock cannot benchmark. It is fenced BY NAME in `determinism_guard`, never by directory.
    println!("\n-- throughput --");
    println!(
        "  {:.0} ms wall for {} documents  ({:.0} ms/doc)",
        cauto,
        ok,
        cauto / (ok.max(1)) as f64
    );

    biggest.sort_by(|a, b| b.0.cmp(&a.0));
    println!("\n  largest documents:");
    for (w, url, chars) in biggest.iter().take(6) {
        let short = url.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or(url);
        println!("    {:>8} B wire -> {:>7} chars   {}", w, chars, short);
    }

    println!("\n  warnings raised ({}):", warned.len());
    if warned.is_empty() {
        println!("    (none)");
    }
    for (url, w) in warned.iter().take(12) {
        let short = url.rsplit('/').next().unwrap_or(url);
        println!("    {short:<44} {w}");
    }

    // ── a trivial "analysis" stage, to show the text is usable downstream ────────────────
    let mut freq: BTreeMap<String, usize> = BTreeMap::new();
    for o in &results {
        if let Some(d) = o.document() {
            for word in d.text.split(|c: char| !c.is_alphanumeric()) {
                if word.len() > 6 {
                    *freq.entry(word.to_ascii_lowercase()).or_default() += 1;
                }
            }
        }
    }
    let mut top: Vec<(&String, &usize)> = freq.iter().collect();
    top.sort_by(|a, b| b.1.cmp(a.1));
    println!("\n-- downstream analysis over the extracted text (no model involved) --");
    println!("  distinct terms >6 chars: {}", freq.len());
    print!("  most frequent:");
    for (w, n) in top.iter().take(10) {
        print!(" {w}({n})");
    }
    println!();

    let (h, r) = marlowe_net::connection_stats();
    println!("\n  totals: {h} TLS handshakes, {r} connections reused across the whole session");
}
