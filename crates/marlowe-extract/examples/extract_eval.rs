//! Extraction eval harness: runs every main-content candidate over a page manifest and writes one
//! JSON line per (page, candidate). Scoring lives in `tools/extraction_eval.py`.
//!
//! ```text
//! cargo run --release --example extract_eval -p marlowe-extract -- \
//!     --manifest data/runs/extraction-eval/manifest.tsv --out data/runs/extraction-eval/raw.jsonl
//! ```
//!
//! Manifest: one `id \t url \t path` per line; `path` may be `.gz`. Pages are decoded as UTF-8
//! (both benchmarks store UTF-8). Timing is wall-clock per call on one thread by default, so
//! p50/p95 are not inflated by contention; `--threads N` trades that for speed.
//!
//! Records:
//! - `{"kind":"page","id",links:[[url,text],..],"bytes"}` once per page (html.rs link list).
//! - `{"kind":"run","id","candidate","ok","text","chars","ms","page_type","confidence","error"}`
//!   per candidate, plus candidate `"selected"` for `Selector::with_defaults().read` (the
//!   shipped path) with `"chosen"`, `"reason"`, `"markdown"`, `"structured"`, `"coverage"` and
//!   `"client_rendered"`.

use std::fmt::Write as _;
use std::io::{BufRead, BufWriter, Read, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

use marlowe_extract::main_content::{all_candidates, MainContent, MainContentError, PageInput, Selector};

/// Every candidate this build can measure. Uncompiled ones report `Unavailable` per page.
fn candidates() -> Vec<Box<dyn MainContent>> {
    all_candidates()
}

/// Caps the link list written per page; pages with more are link farms anyway.
const MAX_LINKS: usize = 3000;

const CONTENT_TYPE: &str = "text/html; charset=utf-8";

struct Args {
    manifest: PathBuf,
    out: PathBuf,
    threads: usize,
    limit: Option<usize>,
}

fn args() -> Result<Args, String> {
    let mut a = std::env::args().skip(1);
    let (mut manifest, mut out, mut threads, mut limit) = (None, None, 1, None);
    while let Some(flag) = a.next() {
        let mut v = || a.next().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--manifest" => manifest = Some(PathBuf::from(v()?)),
            "--out" => out = Some(PathBuf::from(v()?)),
            "--threads" => threads = v()?.parse().map_err(|e| format!("--threads: {e}"))?,
            "--limit" => limit = Some(v()?.parse().map_err(|e| format!("--limit: {e}"))?),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Args {
        manifest: manifest.ok_or("--manifest is required")?,
        out: out.ok_or("--out is required")?,
        threads: threads.max(1),
        limit,
    })
}

struct Row {
    id: String,
    url: String,
    path: PathBuf,
}

fn read_manifest(path: &PathBuf, limit: Option<usize>) -> Result<Vec<Row>, String> {
    let f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut rows = Vec::new();
    for line in std::io::BufReader::new(f).lines() {
        let line = line.map_err(|e| e.to_string())?;
        let mut parts = line.splitn(3, '\t');
        let (Some(id), Some(url), Some(p)) = (parts.next(), parts.next(), parts.next()) else { continue };
        rows.push(Row { id: id.into(), url: url.into(), path: PathBuf::from(p) });
        if limit.is_some_and(|l| rows.len() >= l) {
            break;
        }
    }
    Ok(rows)
}

fn read_page(path: &PathBuf) -> std::io::Result<Vec<u8>> {
    let raw = std::fs::read(path)?;
    if path.extension().is_some_and(|e| e == "gz") {
        let mut out = Vec::with_capacity(raw.len() * 4);
        flate2::read::GzDecoder::new(&raw[..]).read_to_end(&mut out)?;
        Ok(out)
    } else {
        Ok(raw)
    }
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn opt_str(s: Option<&str>) -> String {
    s.map(esc).unwrap_or_else(|| "null".into())
}

#[allow(clippy::too_many_arguments)]
fn run_line(
    id: &str,
    candidate: &str,
    result: Result<(String, Option<String>, Option<f32>), String>,
    ms: f64,
    chosen: Option<&str>,
    reason: Option<&str>,
) -> String {
    let (ok, text, page_type, conf, err) = match result {
        Ok((t, pt, c)) => (true, t, pt, c, None),
        Err(e) => (false, String::new(), None, None, Some(e)),
    };
    format!(
        "{{\"kind\":\"run\",\"id\":{},\"candidate\":{},\"ok\":{ok},\"text\":{},\"chars\":{},\"ms\":{ms:.4},\
         \"page_type\":{},\"confidence\":{},\"error\":{},\"chosen\":{},\"reason\":{}}}",
        esc(id),
        esc(candidate),
        esc(&text),
        text.trim().chars().count(),
        opt_str(page_type.as_deref()),
        conf.map(|c| format!("{c:.4}")).unwrap_or_else(|| "null".into()),
        opt_str(err.as_deref()),
        opt_str(chosen),
        opt_str(reason),
    )
}

/// All records for one page, as lines.
fn eval_page(row: &Row, cands: &[Box<dyn MainContent>], selector: &Selector) -> Vec<String> {
    let bytes = match read_page(&row.path) {
        Ok(b) => b,
        Err(e) => {
            return vec![run_line(&row.id, "baseline", Err(format!("read: {e}")), 0.0, None, None)];
        }
    };
    let url = (!row.url.is_empty()).then_some(row.url.as_str());
    let mut out = Vec::new();

    let links = marlowe_extract::html::extract(&bytes, Some(CONTENT_TYPE), url).map(|d| d.links).unwrap_or_default();
    let mut page = format!("{{\"kind\":\"page\",\"id\":{},\"bytes\":{},\"links\":[", esc(&row.id), bytes.len());
    for (i, l) in links.iter().take(MAX_LINKS).enumerate() {
        if i > 0 {
            page.push(',');
        }
        let _ = write!(page, "[{},{}]", esc(&l.url), esc(&l.text));
    }
    page.push_str("]}");
    out.push(page);

    let html = marlowe_extract::charset::decode(&bytes, Some(CONTENT_TYPE), true, &mut Vec::new()).text;
    let input = PageInput { html: &html, url };
    for c in cands {
        let t = Instant::now();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| c.extract(&input)))
            .unwrap_or(Err(MainContentError::Panicked(c.candidate())));
        let ms = t.elapsed().as_secs_f64() * 1e3;
        let r = r
            .map(|e| (e.text, Some(e.page_type.as_str().to_string()), e.confidence))
            .map_err(|e| match e {
                MainContentError::Unavailable(_) => "unavailable".to_string(),
                other => other.to_string(),
            });
        out.push(run_line(&row.id, c.candidate().as_str(), r, ms, None, None));
    }

    // The shipped path end to end: html.rs parse + decode + every candidate + choice.
    let t = Instant::now();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| selector.read(&bytes, Some(CONTENT_TYPE), url)));
    let ms = t.elapsed().as_secs_f64() * 1e3;
    let line = match r {
        Ok(Ok(m)) => {
            let mut l = run_line(
                &row.id,
                "selected",
                Ok((m.document.text, Some(m.choice.chosen.page_type.as_str().to_string()), m.choice.chosen.confidence)),
                ms,
                Some(m.choice.chosen.candidate.as_str()),
                Some(&format!("{:?}", m.choice.reason)),
            );
            // The markdown the tools hand on, so link recall can be scored on real URLs.
            l.pop();
            let _ = write!(
                l,
                ",\"markdown\":{},\"structured\":{},\"coverage\":{:.4},\"client_rendered\":{}}}",
                esc(&m.markdown),
                m.structured,
                m.coverage,
                m.client_rendered
            );
            l
        }
        Ok(Err(e)) => run_line(&row.id, "selected", Err(e.to_string()), ms, None, None),
        Err(_) => run_line(&row.id, "selected", Err("panicked".into()), ms, None, None),
    };
    out.push(line);
    out
}

fn main() {
    let a = match args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("extract_eval: {e}");
            std::process::exit(2);
        }
    };
    let rows = match read_manifest(&a.manifest, a.limit) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("extract_eval: {e}");
            std::process::exit(2);
        }
    };
    let file = std::fs::File::create(&a.out).unwrap_or_else(|e| {
        eprintln!("extract_eval: {}: {e}", a.out.display());
        std::process::exit(2);
    });
    let sink = Mutex::new(BufWriter::new(file));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let started = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..a.threads {
            s.spawn(|| {
                let cands = candidates();
                let selector = Selector::with_defaults();
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(row) = rows.get(i) else { break };
                    let lines = eval_page(row, &cands, &selector);
                    let mut w = sink.lock().expect("sink");
                    for l in lines {
                        let _ = writeln!(w, "{l}");
                    }
                }
            });
        }
    });
    let _ = sink.lock().expect("sink").flush();
    eprintln!(
        "extract_eval: {} pages, {} candidate(s) + selected, {} thread(s), {:.1}s",
        rows.len(),
        candidates().len(),
        a.threads,
        started.elapsed().as_secs_f64()
    );
}
