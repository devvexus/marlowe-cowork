//! **The corpus through Typst**: every formula converts, typesets, fits its grid box, and encodes.
//!
//! `cargo test -p marlowe-typeset-helper -- --nocapture` prints the numbers.
//!
//! This file reads a wall clock (the timing test), the same category as
//! `marlowe-surface/tests/markdown_cost.rs`.

use std::time::{Duration, Instant};

use marlowe_surface::math;
use marlowe_typeset::{probe, sixel, Caps, Job, Service, Slot};
use marlowe_typeset_helper::typeset_once;

include!("../../marlowe-surface/tests/data/maths_corpus.rs");
include!("../../marlowe-surface/tests/data/maths_engineering.rs");

/// Marlowe's `bright` weight, the colour display maths is drawn in.
const FG: [u8; 3] = [0xcd, 0xc9, 0xd6];
/// The text column a display block's image may use in a 140-column terminal.
const PANE_COLS: u16 = 90;

fn caps() -> Caps {
    Caps { cell_w: probe::DEFAULT_CELL.0, cell_h: probe::DEFAULT_CELL.1, fg: probe::DEFAULT_FG, bg: probe::DEFAULT_BG }
}

/// Every formula with its category.
fn all_formulas() -> Vec<(&'static str, &'static str, bool)> {
    let mut v: Vec<(&str, &str, bool)> = CORPUS.iter().map(|(s, d)| ("general", *s, *d)).collect();
    for (cat, items) in ENGINEERING {
        v.extend(items.iter().map(|(s, d)| (*cat, *s, *d)));
    }
    v
}

#[test]
fn every_corpus_formula_converts_and_typesets() {
    let all = all_formulas();
    let mut per: Vec<(&str, usize, usize)> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    for (cat, src, _) in &all {
        let ok = match math::typst_markup(src) {
            None => {
                failures.push(format!("no Typst form: {src}"));
                false
            }
            Some(markup) => match typeset_once(&markup, 16.0, FG) {
                Ok(cov) if cov.w > 0 && cov.h > 0 => true,
                Ok(_) => {
                    failures.push(format!("typeset to nothing: {src}\n  {markup}"));
                    false
                }
                Err(e) => {
                    failures.push(format!("Typst refused: {src}\n  {markup}\n  {e}"));
                    false
                }
            },
        };
        match per.iter_mut().find(|(c, _, _)| c == cat) {
            Some(e) => {
                e.1 += 1;
                e.2 += usize::from(ok);
            }
            None => per.push((cat, 1, usize::from(ok))),
        }
    }
    println!("maths corpus, Typst (converts and typesets):");
    for (cat, n, ok) in &per {
        println!("  {cat:<30} {ok:>3}/{n:<3} {:>5.1}%", 100.0 * *ok as f64 / *n as f64);
    }
    let ok = all.len() - failures.len();
    println!("  {:<30} {ok:>3}/{:<3} {:>5.1}%", "all", all.len(), 100.0 * ok as f64 / all.len() as f64);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn every_gallery_formula_typesets() {
    for (title, src) in math::GALLERY {
        let markup = math::typst_markup(src).unwrap_or_else(|| panic!("{title}: no Typst form"));
        let cov = typeset_once(&markup, 16.0, FG).unwrap_or_else(|e| panic!("{title}: {e}\n  {markup}"));
        assert!(cov.w > 0 && cov.h > 0, "{title}: typeset to nothing");
    }
}

#[test]
fn every_display_formula_fits_the_box_its_grid_layout_reserved() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let svc = Service::start(caps(), Box::new(marlowe_typeset_helper::typesetter()), tx);
    let mut ready = 0;
    let mut on_grid: Vec<&str> = Vec::new();
    let displays: Vec<&str> = all_formulas().into_iter().filter(|(_, _, d)| *d).map(|(_, s, _)| s).collect();
    for src in &displays {
        let f = math::display(src);
        let job = Job {
            markup: f.typst.clone().expect("display maths has a Typst form"),
            fg: FG,
            // The pane's box: the grid's rows, and the text column from the block's column (90
            // cells in a 140-column terminal). A display block is alone on its rows.
            cols: PANE_COLS,
            rows: f.rows.len() as u16,
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        let slot = loop {
            match svc.get(&job) {
                Slot::Pending if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(2)),
                other => break other,
            }
        };
        match slot {
            Slot::Ready(img) => {
                assert_eq!(img.strips.len(), f.rows.len(), "{src}: one strip per reserved row");
                assert!(img.cols <= PANE_COLS, "{src}: wider than its box");
                assert!(img.strips.iter().any(|s| !s.is_empty()), "{src}: an empty image");
                for s in img.strips.iter().filter(|s| !s.is_empty()) {
                    assert!(s.starts_with(b"\x1bP0;1;0q"), "{src}: not transparent-background Sixel");
                    assert!(marlowe_typeset::proto::valid_strip(s), "{src}: a strip the client would refuse");
                }
                ready += 1;
            }
            Slot::Failed => on_grid.push(src),
            Slot::Pending => panic!("{src}: still pending after 20 s"),
        }
    }
    println!(
        "typst images: {ready}/{} display formulas fit their box at 0.9x or more; on the grid: {on_grid:#?}",
        displays.len()
    );
    // A formula that would need shrinking below 90% stays on the grid, which is correct, but it
    // should be the exception. Most that do are matrices and `cases`, which the grid sets a row
    // per line, tighter than Typst's leading. (202/236 when the floor went from 0.6x to 0.9x.)
    assert!(ready * 10 >= displays.len() * 8, "only {ready}/{} fit", displays.len());
}

#[test]
fn typesetting_is_well_under_a_frame_once_warm() {
    // Cold: the first render builds the library and parses the fonts.
    let t0 = Instant::now();
    typeset_once("x", 16.0, FG).unwrap();
    let cold = t0.elapsed().as_secs_f64() * 1e3;
    let mut samples: Vec<f64> = Vec::new();
    let all = all_formulas();
    for round in 0..2 {
        for (_, src, _) in &all {
            let markup = math::typst_markup(src).unwrap();
            // A different size each round, so no render is served from Typst's memo whole.
            let em = 16.0 + round as f32;
            let t = Instant::now();
            let cov = typeset_once(&markup, em, FG).unwrap();
            let rows = cov.h.div_ceil(20);
            for r in 0..rows {
                std::hint::black_box(sixel::encode(&cov, (r * 20) as isize, 20, FG, probe::DEFAULT_BG));
            }
            samples.push(t.elapsed().as_secs_f64() * 1e3);
        }
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at = |q: f64| samples[((samples.len() as f64 - 1.0) * q) as usize];
    let (p50, p95, max) = (at(0.5), at(0.95), samples[samples.len() - 1]);
    println!(
        "typst typeset + sixel encode, in process, this build's profile: cold {cold:.1} ms; warm p50 {p50:.2} ms, p95 {p95:.2} ms, max {max:.2} ms over {} renders",
        samples.len()
    );
    // The budget is checked in release builds only: a debug build of Typst is ~10x slower and is
    // not what ships. The worker runs off the UI thread either way.
    if !cfg!(debug_assertions) {
        assert!(p95 < 30.0, "p95 {p95:.2} ms is over the 30 ms budget");
    }
}

/// `MARLOWE_TYPESET_PNG=<dir> cargo test -p marlowe-typeset-helper -- --ignored write_pngs`
/// writes each corpus formula as a PNG, for looking at.
#[test]
#[ignore]
fn write_pngs() {
    let Ok(dir) = std::env::var("MARLOWE_TYPESET_PNG") else { return };
    std::fs::create_dir_all(&dir).unwrap();
    for (k, (cat, src, _)) in all_formulas().iter().enumerate() {
        let markup = math::typst_markup(src).unwrap();
        let png = marlowe_typeset_helper::typeset_png(&markup, 32.0, FG).unwrap();
        let tag: String = cat.chars().filter(|c| c.is_ascii_alphanumeric()).take(12).collect();
        std::fs::write(format!("{dir}/{k:03}_{tag}.png"), png).unwrap();
    }
}
