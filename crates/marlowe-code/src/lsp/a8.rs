//! A8 (design 1): code intelligence numbers on this machine, against a rust-analyzer checkout.
//! Run by hand, in release (the product's build of the tree-sitter C code):
//!
//! ```text
//! $env:MARLOWE_A8_REPO = '<path to rust-lang/rust-analyzer at the pinned sha>'
//! cargo test -p marlowe-code --release --lib -- --ignored a8_ --nocapture --test-threads 1
//! ```
//!
//! Gates: cold tags index <= 20 s per 100k LOC; incremental re-tag <= 50 ms per file; post-edit
//! diagnostics p95 <= 5 s warm. The diagnostics run turns rust-analyzer's `checkOnSave`, build
//! scripts and proc macros off, so the number is the client's path (sync, publish, settle) over
//! rust-analyzer's own diagnostics, not a `cargo check` of the whole repository.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use marlowe_os::workspace::LocalFs;
use serde_json::json;

use super::intel::CodeIntel;
use super::manager::LspManager;
use super::servers::default_specs;
use super::transport::StdioLauncher;
use crate::index::{Index, Lang};

fn repo() -> Option<PathBuf> {
    std::env::var_os("MARLOWE_A8_REPO").map(PathBuf::from).filter(|p| p.join("Cargo.toml").is_file())
}

fn pct(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

/// Up to `n` Rust files spread across the index, largest-first within each crate folder.
fn sample_files(idx: &Index, root: &Path, n: usize) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = idx
        .search("", None, 200_000)
        .into_iter()
        .filter(|s| Lang::of(&s.path) == Some(Lang::Rust))
        .map(|s| s.path)
        .collect();
    files.sort();
    files.dedup();
    let step = (files.len() / n.max(1)).max(1);
    files.into_iter().step_by(step).take(n).filter(|p| p.starts_with(root)).collect()
}

#[test]
#[ignore = "A8: needs MARLOWE_A8_REPO; run by hand in release"]
fn a8_tags_index_cold_and_incremental() {
    let Some(root) = repo() else {
        eprintln!("MARLOWE_A8_REPO is not set to a checkout; skipped");
        return;
    };
    let db = std::env::temp_dir().join(format!("marlowe-a8-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&db);
    let idx = Index::open(&db, &root).unwrap();
    let cold = idx.refresh(&LocalFs).unwrap();
    let per_100k = cold.elapsed.as_secs_f64() * 100_000.0 / cold.lines.max(1) as f64;
    let (files, lines, defs, refs) = idx.counts();
    eprintln!(
        "A8 cold index: {} files, {} lines, {defs} definitions, {refs} references in {:.2} s = {:.2} s per 100k LOC (gate <= 20 s); {} workers",
        files,
        lines,
        cold.elapsed.as_secs_f64(),
        per_100k,
        std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).clamp(1, 8)),
    );
    let warm = idx.refresh(&LocalFs).unwrap();
    eprintln!("A8 no-op refresh (stat pass): {:.0} ms, {} re-tagged", warm.elapsed.as_secs_f64() * 1000.0, warm.tagged);

    let sample = sample_files(&idx, &root, 60);
    let mut times = Vec::new();
    for f in &sample {
        let original = std::fs::read(f).unwrap();
        let mut changed = original.clone();
        changed.extend_from_slice(b"\npub fn a8_incremental_probe() {}\n");
        std::fs::write(f, &changed).unwrap();
        let t = Instant::now();
        assert!(idx.update_file(&LocalFs, f).unwrap());
        times.push(t.elapsed());
        std::fs::write(f, &original).unwrap();
        idx.update_file(&LocalFs, f).unwrap();
    }
    times.sort();
    let mean = times.iter().sum::<Duration>() / times.len().max(1) as u32;
    eprintln!(
        "A8 incremental re-tag over {} files: mean {:.1} ms, p95 {:.1} ms, max {:.1} ms (gate <= 50 ms/file)",
        times.len(),
        mean.as_secs_f64() * 1000.0,
        pct(&times, 0.95).as_secs_f64() * 1000.0,
        pct(&times, 1.0).as_secs_f64() * 1000.0
    );
    drop(idx);
    let _ = std::fs::remove_file(&db);
    assert!(per_100k <= 20.0, "cold index {per_100k:.2} s per 100k LOC");
    assert!(pct(&times, 0.95) <= Duration::from_millis(50));
}

#[test]
#[ignore = "A8: needs MARLOWE_A8_REPO and rust-analyzer; run by hand in release"]
fn a8_post_edit_diagnostics_warm() {
    let Some(root) = repo() else {
        eprintln!("MARLOWE_A8_REPO is not set to a checkout; skipped");
        return;
    };
    let mut spec = default_specs().into_iter().find(|s| s.id == "rust-analyzer").unwrap();
    spec.init_options["checkOnSave"] = json!(false);
    // Build scripts and proc macros would compile every dependency first (minutes of CPU on a
    // shared machine); the probe edit is a plain type error that needs neither.
    spec.init_options["cargo"]["buildScripts"]["enable"] = json!(false);
    spec.init_options["procMacro"]["enable"] = json!(false);
    let lsp = LspManager::with(std::sync::Arc::new(StdioLauncher), vec![spec]);
    let idx = Index::in_memory(&root).unwrap();
    idx.refresh(&LocalFs).unwrap();
    let sample: Vec<PathBuf> = sample_files(&idx, &root, 40)
        .into_iter()
        .filter(|p| !p.to_string_lossy().contains("test_data") && !p.to_string_lossy().contains("fixtures"))
        .take(20)
        .collect();
    let eng = CodeIntel::new(&root, idx, lsp.clone());

    let started = Instant::now();
    let client = lsp.client(&sample[0], &root).unwrap();
    let ready = client.wait_ready(Duration::from_secs(900), &|| false);
    eprintln!("A8 rust-analyzer quiescent after {:.1} s (ready: {ready})", started.elapsed().as_secs_f64());

    let broken = "\nfn a8_probe() -> u32 { \"not a number\" }\n";
    let (mut first, mut repeat, mut misses) = (Vec::new(), Vec::new(), 0);
    for f in &sample {
        let original = std::fs::read_to_string(f).unwrap();
        for round in 0..2 {
            // Round 0: the file was not open (the middleware opens it in pre_mutation, right
            // before the edit). Round 1: it is open and settled.
            let snap = eng.before_edit(&LocalFs, std::slice::from_ref(f));
            std::fs::write(f, format!("{original}{broken}")).unwrap();
            let t = Instant::now();
            let out = eng.after_edit(&LocalFs, std::slice::from_ref(f), &snap, &|| false);
            let took = t.elapsed();
            let hit = out.iter().any(|d| d.new_errors.iter().any(|e| e.message.contains("u32") || e.message.contains("mismatched")));
            // A miss counts as past the gate.
            let took = if hit { took } else { took.max(Duration::from_millis(5001)) };
            if !hit {
                misses += 1;
                eprintln!("A8 miss ({round}) {}: {out:?}", f.display());
            }
            if round == 0 {
                first.push(took);
            } else {
                repeat.push(took);
            }
            std::fs::write(f, &original).unwrap();
            let snap = eng.before_edit(&LocalFs, std::slice::from_ref(f));
            eng.after_edit(&LocalFs, std::slice::from_ref(f), &snap, &|| false);
        }
    }
    first.sort();
    repeat.sort();
    let mut all: Vec<Duration> = first.iter().chain(&repeat).copied().collect();
    all.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    eprintln!(
        "A8 post-edit diagnostics, {} edits ({} misses): p50 {:.0} ms, p95 {:.0} ms, max {:.0} ms; first edit of a file p95 {:.0} ms, repeat edit p95 {:.0} ms (gate p95 <= 5000 ms)",
        all.len(),
        misses,
        ms(pct(&all, 0.5)),
        ms(pct(&all, 0.95)),
        ms(pct(&all, 1.0)),
        ms(pct(&first, 0.95)),
        ms(pct(&repeat, 0.95)),
    );
    lsp.shutdown_all();
    assert!(pct(&all, 0.95) <= Duration::from_secs(5));
    assert!(misses * 20 <= all.len(), "more than 5% of edits got no diagnostics in 5 s");
}
