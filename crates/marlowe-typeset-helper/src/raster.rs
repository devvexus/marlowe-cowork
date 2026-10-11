//! **Typst, as a maths rasteriser.** One equation in, a coverage plane out.
//!
//! The world is as small as Typst allows: one in-memory source, embedded fonts (`fonts/`), no
//! files, no packages, no clock. The markup comes from `marlowe_surface::math`, which escapes
//! everything a model wrote, so nothing here evaluates model text as code.
//!
//! **Drawn to sit with the grid maths beside it.** An image must not read as a render pasted in:
//! - **Font.** Fira Math (SIL Open Font License), a sans with an OpenType MATH table, close in
//!   build to a terminal's sans monospace. New Computer Modern and its maths companion (GUST Font
//!   License) are the fallback for the symbols it lacks.
//! - **Pixel-snapped.** The em is nudged so the x-height is a whole number of pixels, and the
//!   page is shifted so the equation's baseline falls on a pixel boundary: lowercase tops and
//!   bottoms land on whole pixels instead of smearing over two grey rows. Every other glyph run
//!   (numerators, denominators, scripts, braces) moves under half a pixel onto a whole-pixel
//!   baseline of its own, and every rule (fraction bars, radical vinculums, over- and underlines)
//!   is a whole number of pixels thick, at least one, with both edges on pixel boundaries
//!   ([`snap`]). Windows Terminal lays Sixel out on a fixed 10×20 cell and scales it to the real
//!   one, so a crisp edge at 10×20 is what survives the scale-up. Stems are not hinted sideways:
//!   Typst draws outlines unhinted.
//! - **Weight.** Every stroke is widened by [`WEIGHT`] supersampled pixels on each side,
//!   horizontally only (so the snapped baseline and x-height stay crisp): at a 17-pixel em Fira
//!   Math's stems are thinner than a terminal font's, and read as grey hairlines.
//! - **Supersampled.** Rendered at [`SUPERSAMPLE`] times the size and averaged down, each output
//!   pixel the mean coverage of its block: an area filter, exact for a coverage plane and free of
//!   ringing. The colour, blended in linear light over 64 levels, is `marlowe_typeset::sixel`'s.

use std::sync::OnceLock;

use typst::diag::{FileError, FileResult};
use typst::foundations::{Bytes, Datetime};
use typst::layout::{Abs, Frame, FrameItem, Page, PagedDocument, Point, Size};
use typst::syntax::{FileId, Source, VirtualPath};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::visualize::{Geometry, Shape};
use typst::{Library, LibraryExt, World};

use marlowe_typeset::sixel::Coverage;

static SANS_MATH_FONT: &[u8] = include_bytes!("../fonts/FiraMath-Regular.otf");
static MATH_FONT: &[u8] = include_bytes!("../fonts/NewCMMath-Regular.otf");
static TEXT_FONT: &[u8] = include_bytes!("../fonts/NewCM10-Regular.otf");

/// Rendered at this many times the asked size in each direction, then averaged down.
pub const SUPERSAMPLE: u32 = 4;
/// Supersampled pixels each stroke is widened by on its left and right: a quarter of an output
/// pixel each side at 4x, half a pixel in all.
pub const WEIGHT: usize = 1;

/// What every render shares: built once, on the worker thread.
pub struct Engine {
    library: LazyHash<Library>,
    book: LazyHash<FontBook>,
    fonts: Vec<Font>,
    main: FileId,
    /// Fira Math's x-height, in ems.
    x_height: f32,
}

impl Engine {
    pub fn new() -> Engine {
        let fonts: Vec<Font> = [SANS_MATH_FONT, MATH_FONT, TEXT_FONT]
            .into_iter()
            .flat_map(|data| Font::iter(Bytes::new(data)))
            .collect();
        let x_height = fonts.first().map_or(0.5, |f| f.metrics().x_height.get() as f32);
        Engine {
            library: LazyHash::new(Library::default()),
            book: LazyHash::new(FontBook::from_fonts(&fonts)),
            fonts,
            main: main_id(),
            x_height,
        }
    }

    /// Typesets `markup` (an equation body) with an em of about `em_px` pixels (snapped so the
    /// x-height is whole pixels), in `fg`, on nothing.
    pub fn render(&self, markup: &str, em_px: f32, fg: [u8; 3]) -> Result<Coverage, String> {
        let page = self.page(markup, self.snapped_em(em_px), fg)?;
        // `size` is in points and one point is one output pixel, so `SUPERSAMPLE` pixels here.
        let pixmap = typst_render::render(&page, SUPERSAMPLE as f32);
        let (w, h) = (pixmap.width() as usize, pixmap.height() as usize);
        let alpha: Vec<u8> = pixmap.data().chunks_exact(4).map(|p| p[3]).collect();
        let fine = embolden(&Coverage { w, h, alpha }, WEIGHT);
        Ok(downsample(&fine, SUPERSAMPLE as usize).trimmed())
    }

    /// The em nearest `em_px` at which the x-height is a whole number of pixels.
    fn snapped_em(&self, em_px: f32) -> f32 {
        let xh = self.x_height.max(0.1);
        (em_px * xh).round().max(1.0) / xh
    }

    /// What the terminal is sent, as a PNG for looking at: the supersampled coverage blended onto
    /// Marlowe's background exactly as the Sixel palette blends it.
    pub fn png(&self, markup: &str, em_px: f32, fg: [u8; 3]) -> Result<Vec<u8>, String> {
        let cov = self.render(markup, em_px, fg)?;
        let bg = marlowe_typeset::probe::DEFAULT_BG;
        let mut pm = tiny_skia::Pixmap::new(cov.w.max(1) as u32, cov.h.max(1) as u32).ok_or("an empty render")?;
        for (px, a) in pm.pixels_mut().iter_mut().zip(&cov.alpha) {
            let [r, g, b] = marlowe_typeset::sixel::blend(fg, bg, f32::from(*a) / 255.0);
            *px = tiny_skia::PremultipliedColorU8::from_rgba(r.round() as u8, g.round() as u8, b.round() as u8, 255)
                .ok_or("a colour out of range")?;
        }
        pm.encode_png().map_err(|e| e.to_string())
    }

    /// The one page of the formula, shifted down (by under a pixel) so the equation's baseline is
    /// on a pixel boundary, then every other run and rule [`snap`]ped.
    fn page(&self, markup: &str, em_px: f32, fg: [u8; 3]) -> Result<Page, String> {
        let doc = self.compile(markup, em_px, fg)?;
        if doc.pages.len() > 1 {
            return Err("the formula broke across pages".into());
        }
        let mut page = doc.pages.into_iter().next().ok_or("no page")?;
        if let Some(b) = baseline(&page.frame) {
            let b = b.to_pt();
            let dy = b.ceil() - b;
            if dy > 1e-4 {
                let mut size = page.frame.size();
                size.y += Abs::pt(1.0);
                page.frame.set_size(size);
                page.frame.translate(Point::with_y(Abs::pt(dy)));
            }
        }
        page.frame = snap(&page.frame, Point::zero());
        Ok(page)
    }

    fn compile(&self, markup: &str, em_px: f32, fg: [u8; 3]) -> Result<PagedDocument, String> {
        let [r, g, b] = fg;
        let text = format!(
            "#set page(width: auto, height: auto, margin: (x: 0.2em, y: 0.15em), fill: none)\n\
             #set text(font: (\"Fira Math\", \"New Computer Modern\"), size: {em_px:.2}pt, fill: rgb({r}, {g}, {b}))\n\
             #show math.equation: set text(font: (\"Fira Math\", \"New Computer Modern Math\"))\n\
             #let fg = rgb({r}, {g}, {b})\n\
             $ {markup} $\n"
        );
        let world = OneShot { engine: self, source: Source::new(self.main, text) };
        let doc: PagedDocument = typst::compile(&world)
            .output
            .map_err(|errs| errs.iter().map(|e| e.message.to_string()).collect::<Vec<_>>().join("; "))?;
        // Bound comemo's memo: formulas are many and each is rendered once.
        typst::comemo::evict(30);
        Ok(doc)
    }
}

/// Where the equation's main baseline is in `frame`: the baseline the most glyphs sit on (Typst
/// places a text run at its baseline). Numerators, scripts and big operators sit elsewhere, but
/// the line they hang off carries the most.
fn baseline(frame: &Frame) -> Option<Abs> {
    fn runs(frame: &Frame, at: Point, out: &mut Vec<(Abs, usize)>) {
        for (pos, item) in frame.items() {
            match item {
                FrameItem::Group(g) => runs(&g.frame, at + *pos, out),
                FrameItem::Text(t) => out.push((at.y + pos.y, t.glyphs.len())),
                _ => {}
            }
        }
    }
    let mut all = Vec::new();
    runs(frame, Point::zero(), &mut all);
    let mut lines: Vec<(Abs, usize)> = Vec::new();
    for (y, n) in all {
        match lines.iter_mut().find(|(ly, _)| (*ly - y).abs() < Abs::pt(0.01)) {
            Some(l) => l.1 += n,
            None => lines.push((y, n)),
        }
    }
    lines.into_iter().max_by_key(|(_, n)| *n).map(|(y, _)| y)
}

/// `frame`, which sits at `at` on the page, with every glyph run's baseline and every rule's edges
/// on whole pixels (one point is one output pixel).
///
/// A run moves by at most half a pixel, a rule by at most a pixel. A vertical glyph assembly (a
/// tall bracket) is one run, so its parts move together. A group with a transform is left as is.
fn snap(frame: &Frame, at: Point) -> Frame {
    let mut out = Frame::new(frame.size(), frame.kind());
    if frame.has_baseline() {
        out.set_baseline(frame.baseline());
    }
    for (pos, item) in frame.items() {
        let here = at + *pos;
        let (shift, item) = match item {
            FrameItem::Group(g) if g.transform.is_identity() => {
                let mut g = g.clone();
                g.frame = snap(&g.frame, here);
                (Point::zero(), FrameItem::Group(g))
            }
            FrameItem::Text(_) => {
                let y = here.y.to_pt();
                (Point::with_y(Abs::pt(y.round() - y)), item.clone())
            }
            FrameItem::Shape(shape, span) => {
                let (shift, shape) = snap_rule(shape, here);
                (shift, FrameItem::Shape(shape, *span))
            }
            other => (Point::zero(), other.clone()),
        };
        out.push(*pos + shift, item);
    }
    out
}

/// A horizontal or vertical stroked line made a whole number of pixels thick, at least one, with
/// both edges on pixel boundaries; a filled rectangle with its top and bottom on them. Anything
/// else as it is. Returns the shift and the shape.
fn snap_rule(shape: &Shape, at: Point) -> (Point, Shape) {
    let flat = |v: Abs| v.to_pt().abs() < 1e-6;
    // How far to move the centre `c` of a stroke `t` thick so its edge `c - t/2` is whole.
    let centre = |c: f64, t: f64| (c - t / 2.0).round() + t / 2.0 - c;
    let mut s = shape.clone();
    match (&shape.geometry, s.stroke.as_mut()) {
        (Geometry::Line(d), Some(stroke)) if flat(d.y) || flat(d.x) => {
            let t = stroke.thickness.to_pt().round().max(1.0);
            stroke.thickness = Abs::pt(t);
            let shift = if flat(d.y) {
                Point::with_y(Abs::pt(centre(at.y.to_pt(), t)))
            } else {
                Point::with_x(Abs::pt(centre(at.x.to_pt(), t)))
            };
            (shift, s)
        }
        (Geometry::Rect(size), None) if shape.fill.is_some() && size.y.to_pt() > 0.0 => {
            let y = at.y.to_pt();
            let h = size.y.to_pt().round().max(1.0);
            s.geometry = Geometry::Rect(Size::new(size.x, Abs::pt(h)));
            (Point::with_y(Abs::pt(y.round() - y)), s)
        }
        _ => (Point::zero(), s),
    }
}

/// `cov` with every covered run widened by `r` pixels to the left and right: each pixel the
/// largest coverage within `r` of it on its row. Rows are untouched, so horizontal edges stay
/// where they were snapped.
fn embolden(cov: &Coverage, r: usize) -> Coverage {
    if r == 0 || cov.w == 0 {
        return cov.clone();
    }
    let mut alpha = vec![0u8; cov.alpha.len()];
    for (src, dst) in cov.alpha.chunks_exact(cov.w).zip(alpha.chunks_exact_mut(cov.w)) {
        for (x, d) in dst.iter_mut().enumerate() {
            let lo = x.saturating_sub(r);
            let hi = (x + r + 1).min(cov.w);
            *d = src[lo..hi].iter().copied().max().unwrap_or(0);
        }
    }
    Coverage { w: cov.w, h: cov.h, alpha }
}

/// `cov` reduced `k` times in each direction, each pixel the mean of its `k` by `k` block (an
/// area filter). A partial block at the right or bottom edge counts its missing pixels as empty.
fn downsample(cov: &Coverage, k: usize) -> Coverage {
    if k <= 1 || cov.w == 0 || cov.h == 0 {
        return cov.clone();
    }
    let (w, h) = (cov.w.div_ceil(k), cov.h.div_ceil(k));
    let n = (k * k) as u32;
    let mut alpha = vec![0u8; w * h];
    for (y, row) in alpha.chunks_exact_mut(w).enumerate() {
        for (x, a) in row.iter_mut().enumerate() {
            let mut sum = 0u32;
            for sy in y * k..((y + 1) * k).min(cov.h) {
                let line = &cov.alpha[sy * cov.w..(sy + 1) * cov.w];
                sum += line[x * k..((x + 1) * k).min(cov.w)].iter().map(|v| u32::from(*v)).sum::<u32>();
            }
            *a = ((sum + n / 2) / n) as u8;
        }
    }
    Coverage { w, h, alpha }
}

impl Default for Engine {
    fn default() -> Self {
        Engine::new()
    }
}

/// One interned id for every render: `FileId::new_fake` would intern a new one each time, and
/// the interner is bounded.
fn main_id() -> FileId {
    static ID: OnceLock<FileId> = OnceLock::new();
    *ID.get_or_init(|| FileId::new(None, VirtualPath::new("/maths.typ")))
}

struct OneShot<'a> {
    engine: &'a Engine,
    source: Source,
}

impl World for OneShot<'_> {
    fn library(&self) -> &LazyHash<Library> {
        &self.engine.library
    }

    fn book(&self) -> &LazyHash<FontBook> {
        &self.engine.book
    }

    fn main(&self) -> FileId {
        self.engine.main
    }

    fn source(&self, id: FileId) -> FileResult<Source> {
        if id == self.engine.main {
            Ok(self.source.clone())
        } else {
            Err(FileError::AccessDenied)
        }
    }

    fn file(&self, _id: FileId) -> FileResult<Bytes> {
        Err(FileError::AccessDenied)
    }

    fn font(&self, index: usize) -> Option<Font> {
        self.engine.fonts.get(index).cloned()
    }

    fn today(&self, _offset: Option<i64>) -> Option<Datetime> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downsampling_averages_each_block_and_keeps_partial_edges() {
        // A 5 by 2 plane at k = 2: blocks of four, the right column a partial block.
        let cov = Coverage { w: 5, h: 2, alpha: vec![255, 255, 0, 255, 255, 255, 255, 0, 0, 255] };
        let d = downsample(&cov, 2);
        assert_eq!((d.w, d.h), (3, 1));
        assert_eq!(d.alpha, vec![255, 64, 128]);
    }

    #[test]
    fn emboldening_widens_runs_sideways_only() {
        let cov = Coverage { w: 5, h: 2, alpha: vec![0, 0, 200, 0, 0, 0, 0, 0, 0, 0] };
        let b = embolden(&cov, 1);
        assert_eq!(b.alpha, vec![0, 200, 200, 200, 0, 0, 0, 0, 0, 0]);
    }

    /// Coverage of `page` rendered and averaged down with no snapping or weight.
    fn plain(page: &Page) -> Coverage {
        let pm = typst_render::render(page, SUPERSAMPLE as f32);
        let alpha = pm.data().chunks_exact(4).map(|p| p[3]).collect();
        downsample(&Coverage { w: pm.width() as usize, h: pm.height() as usize, alpha }, SUPERSAMPLE as usize).trimmed()
    }

    #[test]
    fn lowercase_is_pixel_snapped_at_windows_terminals_cell() {
        // A 20-pixel cell's em. `z` is flat at the baseline and at the x-height, so with both on
        // pixel boundaries its first and last rows are solid, not grey half rows, and it is
        // exactly the x-height tall.
        let e = Engine::new();
        let em = e.snapped_em(0.86 * 20.0);
        let xh = (em * e.x_height).round() as usize;
        assert!((8..=10).contains(&xh), "x-height {xh} px at a {em} px em");
        let fg = [0xcd, 0xc9, 0xd6];
        let cov = e.render("z", 0.86 * 20.0, fg).unwrap();
        let row_max = |c: &Coverage, y: usize| c.alpha[y * c.w..(y + 1) * c.w].iter().copied().max().unwrap();
        let rows: Vec<u8> = (0..cov.h).map(|y| row_max(&cov, y)).collect();
        assert_eq!(cov.h, xh, "{} rows for a {xh} px x-height: {rows:?}", cov.h);
        assert!(row_max(&cov, 0) >= 240 && row_max(&cov, cov.h - 1) >= 240, "soft edges: {} / {}", row_max(&cov, 0), row_max(&cov, cov.h - 1));
        // Unsnapped, the same letter straddles a row boundary.
        let doc = e.compile("z", em + 0.37, fg).unwrap();
        let soft = plain(&doc.pages[0]);
        assert!(soft.h > xh, "unsnapped, `z` was {} rows: it fit by luck; pick another offset", soft.h);
    }

    /// Every glyph run's baseline and every horizontal rule's (top, bottom) in `frame`, on the page.
    fn edges(frame: &Frame, at: Point, runs: &mut Vec<f64>, rules: &mut Vec<(f64, f64)>) {
        for (pos, item) in frame.items() {
            let here = at + *pos;
            match item {
                FrameItem::Group(g) => edges(&g.frame, here, runs, rules),
                FrameItem::Text(_) => runs.push(here.y.to_pt()),
                FrameItem::Shape(s, _) => {
                    if let (Geometry::Line(d), Some(st)) = (&s.geometry, &s.stroke) {
                        if d.y.to_pt().abs() < 1e-6 {
                            let (y, t) = (here.y.to_pt(), st.thickness.to_pt());
                            rules.push((y - t / 2.0, y + t / 2.0));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    #[test]
    fn every_run_and_rule_lands_on_whole_pixels() {
        // The KL formula from Matthew's first hands-on: two fraction bars with a numerator and a
        // denominator each, scripts, an underbrace and a tall bracket.
        let e = Engine::new();
        let kl = r#"D_(K L)(p || q) = 1/2 [ underbrace("tr"(Sigma_2^(-1) Sigma_1), "trace term") + (mu_2 - mu_1)^T Sigma_2^(-1) (mu_2 - mu_1) - d + ln frac(det Sigma_2, det Sigma_1) ]"#;
        let page = e.page(kl, e.snapped_em(0.86 * 20.0), [0xcd, 0xc9, 0xd6]).unwrap();
        let (mut runs, mut rules) = (Vec::new(), Vec::new());
        edges(&page.frame, Point::zero(), &mut runs, &mut rules);
        let whole = |v: f64| (v - v.round()).abs() < 1e-3;
        assert!(rules.len() >= 2, "two fraction bars at least: {rules:?}");
        assert!(
            rules.iter().all(|(a, b)| whole(*a) && whole(*b) && b - a >= 1.0 - 1e-3),
            "rules off the pixel grid: {rules:?}"
        );
        assert!(runs.iter().all(|y| whole(*y)), "baselines off the pixel grid: {runs:?}");
    }

    #[test]
    fn a_fraction_bar_is_one_solid_row() {
        // At Windows Terminal's 20-pixel Sixel cell the bar of 1/2 is one row at full coverage,
        // not two grey half rows.
        let e = Engine::new();
        let cov = e.render("frac(1, 2)", 0.86 * 20.0, [0xcd, 0xc9, 0xd6]).unwrap();
        let row = |y: usize| &cov.alpha[y * cov.w..(y + 1) * cov.w];
        // A bar row: inked across most of the formula's width, which no digit row is.
        let bar: Vec<usize> =
            (0..cov.h).filter(|&y| row(y).iter().filter(|a| **a > 32).count() * 10 >= cov.w * 8).collect();
        let dump: Vec<Vec<u8>> = (0..cov.h).map(|y| row(y).to_vec()).collect();
        assert_eq!(bar.len(), 1, "bar rows {bar:?} in {dump:?}");
        // Its middle, clear of the anti-aliased ends, is solid.
        let mid = &row(bar[0])[cov.w / 4..cov.w - cov.w / 4];
        assert!(mid.iter().all(|a| *a >= 240), "the bar is grey: {mid:?}");
    }

    /// The font family of every glyph run in `frame`, groups included.
    fn families(frame: &typst::layout::Frame, out: &mut Vec<String>) {
        use typst::layout::FrameItem;
        for (_, item) in frame.items() {
            match item {
                FrameItem::Group(g) => families(&g.frame, out),
                FrameItem::Text(t) => out.push(t.font.info().family.clone()),
                _ => {}
            }
        }
    }

    #[test]
    fn maths_is_set_in_the_sans_maths_font_not_the_serif() {
        // A fraction, a sum, Greek and a letter: all of it from Fira Math, so an image sits with
        // the terminal's sans text instead of reading as a serif render pasted in.
        let e = Engine::new();
        let doc = e.compile("frac(sum_(i=1)^n alpha_i, x)", 17.2, [0xcd, 0xc9, 0xd6]).unwrap();
        let mut fams = Vec::new();
        families(&doc.pages[0].frame, &mut fams);
        assert!(!fams.is_empty(), "no glyphs");
        assert!(fams.iter().all(|f| f == "Fira Math"), "{fams:?}");
    }
}
