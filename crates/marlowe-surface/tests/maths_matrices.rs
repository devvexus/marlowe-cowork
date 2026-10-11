//! **Matrices on the character grid**, snapshotted: column alignment from the array spec, brackets
//! and bars built to the full height, ellipses where they belong, the augmented matrix's rule, row
//! spacing around fractions, tags, and a matrix wider than the pane clipped with a marker.
//!
//! `MARLOWE_BLESS=1` rewrites the snapshots in `tests/snapshots/maths/`; review the diff.
//! `cargo test -p marlowe-surface --test maths_matrices -- --ignored --nocapture` prints every
//! display formula in the corpus as the pane draws it, for looking at.

use std::path::PathBuf;

use marlowe_surface::chrome::Ink;
use marlowe_surface::{markdown, math, Theme};
use ratatui::text::Line;

include!("data/maths_corpus.rs");
include!("data/maths_engineering.rs");

fn drawn(src: &str, width: usize) -> String {
    let t = Theme::default_truecolor();
    let lines: Vec<Line<'static>> =
        markdown::render_prose(&format!("$$\n{src}\n$$"), width, &t, Ink::Speech.style(&t));
    lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>().trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn snapshot(name: &str, src: &str, width: usize) {
    let text = format!("{src}\n{}\n{}\n", "-".repeat(width.min(80)), drawn(src, width));
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots/maths").join(format!("{name}.txt"));
    if std::env::var("MARLOWE_BLESS").is_ok_and(|v| v == "1") {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &text).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing snapshot {}; run with MARLOWE_BLESS=1 and review it\n{text}", path.display()))
        .replace("\r\n", "\n");
    assert!(want == text, "snapshot {name} differs.\n--- want\n{want}\n--- got\n{text}");
}

#[test]
fn matrices_snapshot() {
    let cases: &[(&str, &str, usize)] = &[
        ("pmatrix_fractions", r"R_{60^\circ} = \begin{pmatrix} \frac{1}{2} & -\frac{\sqrt{3}}{2} \\ \frac{\sqrt{3}}{2} & \frac{1}{2} \end{pmatrix}", 100),
        ("array_lcr", r"\begin{array}{lcr} \text{left} & \text{centre} & \text{right} \\ a & b & c \\ aaaa & bbbb & cccc \end{array}", 100),
        ("pmatrix_star_r", r"\begin{pmatrix*}[r] -1 & 3 \\ 2 & -4 \end{pmatrix*}", 100),
        ("ellipses", r"A = \begin{pmatrix} a_{11} & a_{12} & \cdots & a_{1n} \\ a_{21} & a_{22} & \cdots & a_{2n} \\ \vdots & \vdots & \ddots & \vdots \\ a_{m1} & a_{m2} & \cdots & a_{mn} \end{pmatrix}", 100),
        ("augmented", r"\left[\begin{array}{ccc|c} 2 & 1 & -1 & 8 \\ -3 & -1 & 2 & -11 \\ -2 & 1 & 2 & -3 \end{array}\right]", 100),
        ("block_hline", r"\left[\begin{array}{c|c} A & B \\ \hline C & D \end{array}\right]", 100),
        ("vmatrix_bars", r"\det A = \begin{vmatrix} a & b \\ c & d \end{vmatrix} = ad - bc", 100),
        ("Vmatrix_Bmatrix", r"\begin{Vmatrix} x_1 \\ x_2 \\ x_3 \end{Vmatrix} + \begin{Bmatrix} 1 & 2 \\ 3 & 4 \\ 5 & 6 \end{Bmatrix}", 100),
        ("jacobian", r"J = \begin{bmatrix} \frac{\partial f_1}{\partial x_1} & \cdots & \frac{\partial f_1}{\partial x_n} \\ \vdots & \ddots & \vdots \\ \frac{\partial f_m}{\partial x_1} & \cdots & \frac{\partial f_m}{\partial x_n} \end{bmatrix}", 100),
        ("tridiagonal", r"\begin{pmatrix} b_1 & c_1 & & & \\ a_2 & b_2 & c_2 & & \\ & \ddots & \ddots & \ddots & \\ & & a_{n-1} & b_{n-1} & c_{n-1} \\ & & & a_n & b_n \end{pmatrix}", 100),
        ("align_tags", r"\begin{align} \nabla \cdot \mathbf{E} &= \frac{\rho}{\varepsilon_0} \tag{1} \\ \nabla \cdot \mathbf{B} &= 0 \tag{2} \\ \nabla \times \mathbf{E} &= -\frac{\partial \mathbf{B}}{\partial t} \tag{3} \end{align}", 100),
        ("lorenz_brace", r"\left\{ \begin{aligned} \dot{x} &= \sigma (y - x) \\ \dot{y} &= x(\rho - z) - y \\ \dot{z} &= xy - \beta z \end{aligned} \right.", 100),
        ("wide_clipped", r"F_8 = \frac{1}{\sqrt{8}} \begin{bmatrix} 1 & 1 & 1 & 1 & 1 & 1 & 1 & 1 \\ 1 & \omega & \omega^2 & \omega^3 & \omega^4 & \omega^5 & \omega^6 & \omega^7 \\ 1 & \omega^2 & \omega^4 & \omega^6 & 1 & \omega^2 & \omega^4 & \omega^6 \end{bmatrix}", 40),
        ("wide_block_inverse", r"\begin{bmatrix} A & B \\ C & D \end{bmatrix}^{-1} = \begin{bmatrix} (A - BD^{-1}C)^{-1} & -(A - BD^{-1}C)^{-1}BD^{-1} \\ -D^{-1}C(A - BD^{-1}C)^{-1} & D^{-1} + D^{-1}C(A - BD^{-1}C)^{-1}BD^{-1} \end{bmatrix}", 50),
    ];
    for (name, src, w) in cases {
        snapshot(name, src, *w);
    }
}

#[test]
fn a_matrix_too_wide_for_the_pane_is_clipped_with_a_marker_not_wrapped() {
    let src = ENGINEERING.iter().find(|(c, _)| *c == "ellipses and large matrices").unwrap().1[2].0;
    let f = math::display(src);
    assert!(f.width > 36 && f.clip_row.is_some(), "the DFT matrix is a wide matrix");
    let shown = drawn(src, 36);
    let rows: Vec<&str> = shown.lines().collect();
    assert_eq!(rows.len(), f.rows.len(), "clipped, not set on one line and wrapped");
    let marks: Vec<usize> = rows.iter().enumerate().filter(|(_, r)| r.contains(math::CLIP_MARKER)).map(|(k, _)| k).collect();
    assert_eq!(marks.len(), 1, "one marker:\n{shown}");
    let marked = rows[marks[0]];
    assert!(marked.ends_with(math::CLIP_MARKER), "the marker is at the edge: {marked:?}");
    use unicode_width::UnicodeWidthStr;
    assert_eq!(marked.width(), 36, "the marker sits in the pane's last column");
    for r in &rows {
        assert!(r.width() <= 36, "a clipped row overflows: {r:?}");
    }
    // The rows reserved while it streams are the rows it lands in.
    assert_eq!(math::reserve_rows(src, 34), f.rows.len());
    // A formula without a matrix still goes to its one-line form.
    let wide = r"\mathcal{L} = \sum_{i=1}^{N} \left( y_i \log \hat{y}_i + (1 - y_i) \log (1 - \hat{y}_i) \right) + \lambda \sum_{j} \|w_j\|^2 + \mu \sum_k \|b_k\|^2";
    assert!(math::display(wide).clip_row.is_none());
    assert!(!drawn(wide, 40).contains(math::CLIP_MARKER));
}

#[test]
fn augmented_rules_and_bars_run_the_full_height() {
    let shown = drawn(r"\left[\begin{array}{cc|c} \frac{1}{2} & 2 & 3 \\ 4 & 5 & 6 \end{array}\right]", 80);
    let rows: Vec<Vec<char>> = shown.lines().map(|l| l.chars().collect()).collect();
    let col = rows[0].iter().position(|c| *c == '⎪').expect("a rule");
    for (k, r) in rows.iter().enumerate() {
        assert_eq!(r.get(col), Some(&'⎪'), "row {k} breaks the rule:\n{shown}");
    }
    assert!(rows[0][2] == '⎡' && rows[rows.len() - 1][2] == '⎣', "the bracket spans the block:\n{shown}");
}

#[test]
fn every_gallery_formula_parses_and_has_an_image_form() {
    assert!(math::GALLERY.len() >= 30);
    for (title, src) in math::GALLERY {
        assert!(math::parses(src), "{title} does not parse strictly");
        assert!(math::typst_markup(src).is_some(), "{title} has no Typst form");
        let shown = drawn(src, 100);
        assert!(!shown.contains(['\\', '$', '&']), "{title} put source on screen:\n{shown}");
    }
}

/// Prints every display formula in the corpus as the pane draws it at 100 columns.
#[test]
#[ignore]
fn dump() {
    let mut all: Vec<(&str, &str)> = CORPUS.iter().filter(|(_, d)| *d).map(|(s, _)| ("general", *s)).collect();
    for (cat, items) in ENGINEERING {
        all.extend(items.iter().filter(|(_, d)| *d).map(|(s, _)| (*cat, *s)));
    }
    let w = std::env::var("MARLOWE_DUMP_WIDTH").ok().and_then(|v| v.parse().ok()).unwrap_or(100);
    for (cat, src) in all {
        println!("── {cat}: {src}\n{}\n", drawn(src, w));
    }
}
