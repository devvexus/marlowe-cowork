//! **The real `marlowe-typeset.exe`, driven the way the TUI drives it**: started on the first
//! formula, one JSON line each way, restarted once after a death, and the grid after that.
//!
//! This file reads a wall clock (cold and warm timings), the same category as
//! `marlowe-surface/tests/markdown_cost.rs`.

use std::path::PathBuf;
use std::time::Instant;

use marlowe_surface::math;
use marlowe_typeset::helper::{Helper, MAX_STARTS};
use marlowe_typeset::{probe, Caps, Job};

/// This build's helper, or `MARLOWE_TYPESET_HELPER` (a release build, for its timings).
fn exe() -> PathBuf {
    std::env::var_os("MARLOWE_TYPESET_HELPER")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_marlowe-typeset")))
}

fn caps() -> Caps {
    Caps { cell_w: probe::DEFAULT_CELL.0, cell_h: probe::DEFAULT_CELL.1, fg: probe::DEFAULT_FG, bg: probe::DEFAULT_BG }
}

fn job(src: &str) -> Job {
    let f = math::display(src);
    Job { markup: f.typst.clone().expect("a Typst form"), fg: [0xcd, 0xc9, 0xd6], cols: f.width as u16, rows: f.rows.len() as u16 }
}

include!("../../marlowe-surface/tests/data/maths_corpus.rs");
include!("../../marlowe-surface/tests/data/maths_engineering.rs");

#[test]
fn the_helper_renders_restarts_once_and_then_leaves_formulas_on_the_grid() {
    let mut h = Helper::new(exe());
    assert_eq!(h.pid(), None, "nothing runs before the first formula");

    let t0 = Instant::now();
    let img = h.render(&job(r"\frac{a}{b} + \sqrt{x}"), caps()).expect("the first formula renders");
    let cold = t0.elapsed().as_secs_f64() * 1e3;
    assert!(img.strips.iter().any(|s| !s.is_empty()));
    let first = h.pid().expect("started on the first formula");

    // Warm: every display formula in the corpus, through the process.
    let mut displays: Vec<&str> = CORPUS.iter().filter(|(_, d)| *d).map(|(s, _)| *s).collect();
    for (_, items) in ENGINEERING {
        displays.extend(items.iter().filter(|(_, d)| *d).map(|(s, _)| *s));
    }
    let mut samples = Vec::new();
    let mut fitted = 0;
    for src in &displays {
        let t = Instant::now();
        let r = h.render(&job(src), caps());
        samples.push(t.elapsed().as_secs_f64() * 1e3);
        if r.is_ok() {
            fitted += 1;
        }
    }
    assert_eq!(h.pid(), Some(first), "a formula that does not fit is an error reply, not a dead helper");
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at = |q: f64| samples[((samples.len() as f64 - 1.0) * q) as usize];
    println!(
        "helper process: cold first formula {cold:.1} ms (start + Typst + encode); warm p50 {:.2} ms, p95 {:.2} ms, max {:.2} ms over {} formulas ({fitted} fit their box)",
        at(0.5),
        at(0.95),
        samples[samples.len() - 1],
        samples.len()
    );
    assert!(fitted * 10 >= displays.len() * 9, "only {fitted}/{} fit", displays.len());

    // Markup Typst refuses: an error reply, and the helper stays up.
    let bad = Job { markup: "frac(".into(), fg: [1, 2, 3], cols: 10, rows: 2 };
    assert!(h.render(&bad, caps()).is_err());
    assert_eq!(h.pid(), Some(first));

    // A death: the formula in flight would stay on the grid; the next one restarts the helper.
    h.kill();
    assert!(h.usable());
    let again = h.render(&job(r"x^2"), caps());
    assert!(again.is_ok(), "{again:?}");
    assert_eq!(h.starts(), 2);
    assert_ne!(h.pid(), Some(first));

    // A second death: no third start. Every formula stays on the grid from here.
    h.kill();
    assert!(!h.usable());
    assert!(h.render(&job(r"y^2"), caps()).is_err());
    assert_eq!(h.starts(), MAX_STARTS);
}

#[test]
fn the_helper_exits_when_its_client_goes_away() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new(exe()).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "{{\"v\":1,\"id\":1,\"markup\":\"x\",\"fg\":[1,1,1],\"bg\":[0,0,0],\"cell\":[10,20],\"cols\":2,\"rows\":1}}").unwrap();
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let reply: serde_json::Value = serde_json::from_slice(out.stdout.split(|b| *b == b'\n').next().unwrap()).unwrap();
    assert_eq!(reply["id"], 1);
}
