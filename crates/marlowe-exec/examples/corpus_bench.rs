//! Benchmark for the fetch/extract path.
//!
//! Two halves, kept separate on purpose:
//!
//! - **CPU** — synthetic pages, no network, fully deterministic. Measures extraction throughput
//!   and how much of a page is boilerplate.
//! - **End to end** — real URLs on the command line, serial versus concurrent.
//!
//! Run: `cargo run --release -p marlowe-exec --example corpus_bench [-- <url>...]`

use std::time::Instant;

/// A page shaped like a real article: heavy `<head>`, inline CSS, nav, the actual content, a
/// related-links strip, a footer, and analytics script at the end.
fn synthetic_page(article_paras: usize) -> String {
    let mut s = String::with_capacity(256 * 1024);
    s.push_str("<!doctype html><html lang=\"en\"><head><title>A Study of Something</title>");
    s.push_str("<meta charset=\"utf-8\"><meta name=\"description\" content=\"A summary.\">");
    s.push_str("<style>");
    for i in 0..600 {
        s.push_str(&format!(
            ".cls-{i} {{ margin: 0; padding: {i}px; color: #333; font-family: sans-serif; }}\n"
        ));
    }
    s.push_str("</style><script>window.dataLayer=window.dataLayer||[];");
    for i in 0..300 {
        s.push_str(&format!("dataLayer.push({{event:'e{i}',value:{i}}});"));
    }
    s.push_str("</script></head><body><nav>");
    for i in 0..40 {
        s.push_str(&format!("<a href=\"/section/{i}\">Section {i}</a>"));
    }
    s.push_str("</nav><header><a href=\"/\">Home</a></header><main><article><h1>A Study of Something</h1>");
    for p in 0..article_paras {
        s.push_str(&format!(
            "<p>Paragraph {p} of the article body. It contains ordinary prose with \
             <a href=\"/ref/{p}\">a citation</a> and some &amp; entities &mdash; the sort of \
             text a reader actually came for, at a realistic length per paragraph.</p>"
        ));
    }
    s.push_str("</article></main><aside><h2>Related</h2>");
    for i in 0..30 {
        s.push_str(&format!("<a href=\"/related/{i}\">Related article {i}</a>"));
    }
    s.push_str("</aside><footer><p>Copyright 2026. All rights reserved.</p>");
    for i in 0..20 {
        s.push_str(&format!("<a href=\"/legal/{i}\">Legal {i}</a>"));
    }
    s.push_str("</footer><script>");
    for i in 0..400 {
        s.push_str(&format!("function t{i}(a,b){{ return a < b ? a : b; }}\n"));
    }
    s.push_str("</script></body></html>");
    s
}

fn cpu_benchmark() {
    println!("== CPU: extraction throughput (no network) ==\n");

    let page = synthetic_page(60);
    let bytes = page.as_bytes();
    println!("synthetic article page: {} KB", bytes.len() / 1024);

    // What the OLD web executor produced: the whole response as lossy UTF-8.
    let t = Instant::now();
    let mut sink = 0usize;
    for _ in 0..50 {
        sink += String::from_utf8_lossy(bytes).len();
    }
    let old_ms = t.elapsed().as_secs_f64() * 1000.0 / 50.0;
    let old_chars = sink / 50;

    // What it produces now.
    let input = marlowe_extract::Input::new(bytes)
        .content_type(Some("text/html; charset=utf-8"))
        .url(Some("https://example.com/a/study.html"));
    let t = Instant::now();
    let mut doc = None;
    for _ in 0..50 {
        doc = Some(marlowe_extract::extract(&input).expect("extracts"));
    }
    let new_ms = t.elapsed().as_secs_f64() * 1000.0 / 50.0;
    let doc = doc.unwrap();

    println!("\n  old path (from_utf8_lossy of the whole response)");
    println!("    {old_ms:>8.3} ms   {old_chars:>9} chars to the model");
    println!("  new path (extract)");
    println!("    {new_ms:>8.3} ms   {:>9} chars to the model", doc.text.len());
    println!(
        "\n  reduction   {:.1}%   ({} -> {} chars, {:.1}x less)",
        doc.reduction() * 100.0,
        old_chars,
        doc.text.len(),
        old_chars as f64 / doc.text.len().max(1) as f64
    );
    println!(
        "  throughput  {:.0} MB/s",
        (bytes.len() as f64 / (1024.0 * 1024.0)) / (new_ms / 1000.0)
    );
    println!("  links {}  headings {}  title {:?}", doc.links.len(), doc.headings.len(), doc.title);

    // Parallel extraction over a corpus.
    println!("\n== CPU: extract_many over a 300-document corpus ==\n");
    let pages: Vec<String> = (0..300).map(|i| synthetic_page(20 + (i % 40))).collect();
    let total: usize = pages.iter().map(|p| p.len()).sum();
    let inputs: Vec<marlowe_extract::Input<'_>> = pages
        .iter()
        .map(|p| {
            marlowe_extract::Input::new(p.as_bytes())
                .content_type(Some("text/html; charset=utf-8"))
        })
        .collect();

    let t = Instant::now();
    let serial: Vec<_> = inputs.iter().map(marlowe_extract::extract).collect();
    let serial_ms = t.elapsed().as_secs_f64() * 1000.0;

    let t = Instant::now();
    let par = marlowe_extract::extract_many(&inputs);
    let par_ms = t.elapsed().as_secs_f64() * 1000.0;

    // Hold at the high-water mark while `pages`, `inputs`, `serial` and `par` are all still
    // alive, so an external sampler can actually observe the peak. Two samples over a 5 ms run
    // is not a measurement of a maximum.
    if std::env::var("BENCH_HOLD").is_ok() {
        eprintln!("holding at peak for sampling...");
        std::thread::sleep(std::time::Duration::from_millis(2500));
    }

    let ok = par.iter().filter(|r| r.is_ok()).count();
    println!("  corpus {:.1} MB across {} documents", total as f64 / 1_048_576.0, pages.len());
    println!("  serial        {serial_ms:>8.1} ms   {:.0} MB/s", (total as f64 / 1_048_576.0) / (serial_ms / 1000.0));
    println!("  extract_many  {par_ms:>8.1} ms   {:.0} MB/s", (total as f64 / 1_048_576.0) / (par_ms / 1000.0));
    println!("  speedup       {:>8.2}x   on {} cores", serial_ms / par_ms, std::thread::available_parallelism().map_or(0, |n| n.get()));
    println!("  extracted     {ok}/{} ok", par.len());
    assert_eq!(serial.len(), par.len());
}

fn live_benchmark(urls: &[String]) {
    println!("\n== END TO END: {} real URLs ==\n", urls.len());

    // Serial: what the tool does today, one URL per call.
    let t = Instant::now();
    let serial = marlowe_exec::corpus::fetch_and_extract(urls, 1);
    let serial_ms = t.elapsed().as_secs_f64() * 1000.0;

    // Concurrent, warm pool and warm DNS.
    let t = Instant::now();
    let concurrent = marlowe_exec::corpus::fetch_and_extract(urls, 16);
    let concurrent_ms = t.elapsed().as_secs_f64() * 1000.0;

    let read = concurrent.iter().filter(|o| o.document().is_some()).count();
    let wire: usize = concurrent
        .iter()
        .filter_map(|o| match o {
            marlowe_exec::corpus::Outcome::Read { wire_bytes, .. } => Some(*wire_bytes),
            _ => None,
        })
        .sum();
    let chars: usize = concurrent
        .iter()
        .filter_map(|o| o.document().map(|d| d.text.len()))
        .sum();

    println!("  serial (concurrency 1)   {serial_ms:>9.0} ms");
    println!("  concurrent (16)          {concurrent_ms:>9.0} ms");
    println!("  speedup                  {:>9.2}x", serial_ms / concurrent_ms.max(0.001));
    println!("\n  read {read}/{} documents", urls.len());
    println!("  {} KB on the wire -> {} KB of text", wire / 1024, chars / 1024);
    let (handshakes, reuses) = marlowe_net::connection_stats();
    println!("  TLS handshakes {handshakes}, connections reused {reuses}");

    println!("\n  per URL:");
    for o in &concurrent {
        match o {
            marlowe_exec::corpus::Outcome::Read { url, document, wire_bytes, .. } => println!(
                "    ok        {:>7} B wire -> {:>7} chars  {:>5}  {}",
                wire_bytes,
                document.text.len(),
                document.format.as_str(),
                url
            ),
            marlowe_exec::corpus::Outcome::Redirect { url, location, .. } => {
                println!("    redirect  {}  ({url})", location.render())
            }
            marlowe_exec::corpus::Outcome::Unreachable { url, detail } => {
                println!("    FAILED    {url}: {detail}")
            }
            marlowe_exec::corpus::Outcome::Unreadable { url, detail, .. } => {
                println!("    UNREAD    {url}: {detail}")
            }
        }
    }
    let _ = serial;
}

fn main() {
    cpu_benchmark();
    let urls: Vec<String> = std::env::args().skip(1).collect();
    if urls.is_empty() {
        println!("\n(no URLs given; skipping the live half)");
    } else {
        live_benchmark(&urls);
    }
}
