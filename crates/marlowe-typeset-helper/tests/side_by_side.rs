//! **Grid beside image**: the same formula drawn both ways, as Windows Terminal would show them.
//!
//! For each formula, its character-grid rows are drawn in the system's Cascadia Mono (Cambria Math
//! and Segoe UI Symbol for what it lacks) at a real cell, and under them the exact Sixel strips the
//! pane would send, decoded and scaled from Windows Terminal's fixed 10×20 Sixel cell to that real
//! cell, both on Marlowe's background. Ignored by default: it reads system fonts and writes PNGs.
//!
//! `scripts/cargo-idle.ps1 test -p marlowe-typeset-helper --test side_by_side -- --ignored --nocapture`
//! writes `target/maths-compare/{bilinear,nearest}.png` (Windows Terminal's scaling filter is not
//! documented, so both bounds are drawn) and `wt-cell.png`, the 10×20 pixels as sent.
//!
//! The grid side is an approximation of DirectWrite (tiny-skia blends in sRGB, no hinting); the
//! image side is byte-exact up to the scaler.

use std::path::PathBuf;

use marlowe_surface::math;
use marlowe_typeset::{fit, probe, Caps, Job};
use ttf_parser::{Face, OutlineBuilder};

/// The grid maths colour in Marlowe's terminal profile (the terminal's foreground).
const FG: [u8; 3] = probe::DEFAULT_FG;
const BG: [u8; 3] = probe::DEFAULT_BG;
/// Windows Terminal's Sixel cell.
const VCELL: (usize, usize) = (10, 20);
/// The real cell measured from Matthew's screenshot.
const REAL_H: usize = 28;
/// A display block's indent in the pane, and the text column it may use.
const INDENT: usize = 2;
const COLS: u16 = 90;

const FORMULAS: &[&str] = &[
    r"\mathrm{KL}(N_1 \| N_2) = \frac{1}{2}\left[\underbrace{\operatorname{tr}(\Sigma_2^{-1}\Sigma_1)}_{\text{trace term}} + (\mu_2-\mu_1)^T\Sigma_2^{-1}(\mu_2-\mu_1) - d + \ln\frac{\det\Sigma_2}{\det\Sigma_1}\right]",
    r"J = \begin{pmatrix} s_1(1-s_1) & -s_1 s_2 \\ -s_2 s_1 & s_2(1-s_2) \end{pmatrix}",
    r"\sum_{i=1}^{n} \frac{e^{z_i}}{\sum_j e^{z_j}} = 1, \quad \sigma(x) = \frac{1}{1+e^{-x}}",
    r"\nabla_\theta \mathcal{L} = \mathbb{E}_{x \sim p}\left[\sqrt{\frac{\partial \ell}{\partial \theta}}\right]",
];

struct Fonts {
    faces: Vec<(Vec<u8>, u32)>,
}

impl Fonts {
    fn load() -> Option<Fonts> {
        let dir = PathBuf::from(std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into())).join("Fonts");
        let mut faces = Vec::new();
        for (file, index) in [("CascadiaMono.ttf", 0), ("cambria.ttc", 1), ("seguisym.ttf", 0)] {
            if let Ok(bytes) = std::fs::read(dir.join(file)) {
                faces.push((bytes, index));
            }
        }
        (!faces.is_empty()).then_some(Fonts { faces })
    }

    fn face(&self, k: usize) -> Face<'_> {
        Face::parse(&self.faces[k].0, self.faces[k].1).expect("a font")
    }
}

struct Path<'a>(&'a mut tiny_skia::PathBuilder, f32, f32, f32);

impl OutlineBuilder for Path<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.0.move_to(self.1 + x * self.3, self.2 - y * self.3);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.0.line_to(self.1 + x * self.3, self.2 - y * self.3);
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.0.quad_to(self.1 + x1 * self.3, self.2 - y1 * self.3, self.1 + x * self.3, self.2 - y * self.3);
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let (ox, oy, s) = (self.1, self.2, self.3);
        self.0.cubic_to(ox + x1 * s, oy - y1 * s, ox + x2 * s, oy - y2 * s, ox + x * s, oy - y * s);
    }
    fn close(&mut self) {
        self.0.close();
    }
}

/// The real cell for Cascadia Mono set so a line is `REAL_H` pixels: (width, height, em px, ascent px).
fn real_cell(f: &Fonts) -> (usize, usize, f32, f32) {
    let face = f.face(0);
    let upem = f32::from(face.units_per_em());
    let line = f32::from(face.ascender()) - f32::from(face.descender()) + f32::from(face.line_gap());
    let px = REAL_H as f32 * upem / line;
    let adv = face.glyph_index('M').and_then(|g| face.glyph_hor_advance(g)).map_or(upem * 0.6, f32::from);
    let xh = face.x_height().map_or(0.0, f32::from) / upem;
    println!("Cascadia Mono: em {px:.1} px, x-height {:.3} em ({:.1} px), line {:.3} em", xh, xh * px, line / upem);
    ((adv * px / upem).round() as usize, REAL_H, px, f32::from(face.ascender()) * px / upem)
}

fn draw_grid(pm: &mut tiny_skia::Pixmap, f: &Fonts, rows: &[String], top: usize, cell: (usize, usize, f32, f32)) {
    let (cw, ch, px, asc) = cell;
    let mut paint = tiny_skia::Paint::default();
    paint.set_color_rgba8(FG[0], FG[1], FG[2], 255);
    paint.anti_alias = true;
    for (r, row) in rows.iter().enumerate() {
        for (c, chr) in row.chars().enumerate() {
            if chr == ' ' {
                continue;
            }
            let Some((face, gid)) = (0..f.faces.len()).find_map(|k| {
                let face = f.face(k);
                face.glyph_index(chr).map(|g| (face, g))
            }) else {
                continue;
            };
            let s = px / f32::from(face.units_per_em());
            let x = ((INDENT + c) * cw) as f32;
            let y = ((top + r) * ch) as f32 + asc;
            let mut pb = tiny_skia::PathBuilder::new();
            if face.outline_glyph(gid, &mut Path(&mut pb, x, y, s)).is_none() {
                continue;
            }
            if let Some(path) = pb.finish() {
                pm.fill_path(&path, &paint, tiny_skia::FillRule::Winding, tiny_skia::Transform::identity(), None);
            }
        }
    }
}

/// A Sixel image's pixels (`None` where nothing was drawn) and its width.
fn decode(s: &[u8]) -> (usize, Vec<Option<[u8; 3]>>) {
    let start = s.iter().position(|b| *b == b'q').expect("a sixel") + 1;
    let (mut w, mut h) = (0usize, 0usize);
    let mut px: Vec<Option<[u8; 3]>> = Vec::new();
    let mut regs = std::collections::HashMap::new();
    let (mut x, mut band, mut colour, mut i) = (0usize, 0usize, [0u8; 3], start);
    let nums = |i: &mut usize| -> Vec<u32> {
        let st = *i;
        while *i < s.len() && (s[*i].is_ascii_digit() || s[*i] == b';') {
            *i += 1;
        }
        std::str::from_utf8(&s[st..*i]).unwrap().split(';').map(|n| n.parse().unwrap_or(0)).collect()
    };
    while i < s.len() {
        let b = s[i];
        i += 1;
        match b {
            b'"' => {
                let n = nums(&mut i);
                (w, h) = (n[2] as usize, n[3] as usize);
                px = vec![None; w * h];
            }
            b'#' => {
                let n = nums(&mut i);
                if n.len() == 5 {
                    let c = |v: u32| ((v * 255 + 50) / 100) as u8;
                    regs.insert(n[0], [c(n[2]), c(n[3]), c(n[4])]);
                }
                colour = regs[&n[0]];
            }
            b'$' => x = 0,
            b'-' => {
                x = 0;
                band += 1;
            }
            b'!' => {
                let n = nums(&mut i)[0] as usize;
                let bits = s[i] - 63;
                i += 1;
                for _ in 0..n {
                    put(&mut px, w, h, x, band, bits, colour);
                    x += 1;
                }
            }
            0x1b => break,
            63..=126 => {
                put(&mut px, w, h, x, band, b - 63, colour);
                x += 1;
            }
            _ => {}
        }
    }
    (w, px)
}

fn put(px: &mut [Option<[u8; 3]>], w: usize, h: usize, x: usize, band: usize, bits: u8, c: [u8; 3]) {
    for k in 0..6 {
        let y = band * 6 + k;
        if bits & (1 << k) != 0 && x < w && y < h {
            px[y * w + x] = Some(c);
        }
    }
}

/// The image's virtual pixels (10×20 per cell) on the background.
fn virtual_plane(strips: &[std::sync::Arc<[u8]>], cols: usize) -> (usize, usize, Vec<[f32; 3]>) {
    let (vw, vh) = (cols * VCELL.0, strips.len() * VCELL.1);
    let mut plane = vec![BG.map(f32::from); vw * vh];
    for (r, s) in strips.iter().enumerate() {
        if s.is_empty() {
            continue;
        }
        let (w, px) = decode(s);
        for (k, p) in px.iter().enumerate() {
            let (x, y) = (k % w, k / w);
            if let Some(c) = p {
                if x < vw {
                    plane[(r * VCELL.1 + y) * vw + x] = c.map(f32::from);
                }
            }
        }
    }
    (vw, vh, plane)
}

fn draw_image(pm: &mut tiny_skia::Pixmap, plane: &(usize, usize, Vec<[f32; 3]>), top: usize, cell: (usize, usize), bilinear: bool) {
    let (vw, vh, p) = plane;
    let (cw, ch) = cell;
    let (sx, sy) = (VCELL.0 as f32 / cw as f32, VCELL.1 as f32 / ch as f32);
    let rw = vw * cw / VCELL.0;
    let rh = vh * ch / VCELL.1;
    let at = |x: isize, y: isize| p[(y.clamp(0, *vh as isize - 1) as usize) * vw + x.clamp(0, *vw as isize - 1) as usize];
    let pw = pm.width() as usize;
    let data = pm.pixels_mut();
    for y in 0..rh {
        for x in 0..rw {
            let (u, v) = ((x as f32 + 0.5) * sx - 0.5, (y as f32 + 0.5) * sy - 0.5);
            let c = if bilinear {
                let (x0, y0) = (u.floor(), v.floor());
                let (fx, fy) = (u - x0, v - y0);
                let (x0, y0) = (x0 as isize, y0 as isize);
                let mut c = [0.0; 3];
                for (i, o) in c.iter_mut().enumerate() {
                    let top = at(x0, y0)[i] * (1.0 - fx) + at(x0 + 1, y0)[i] * fx;
                    let bot = at(x0, y0 + 1)[i] * (1.0 - fx) + at(x0 + 1, y0 + 1)[i] * fx;
                    *o = top * (1.0 - fy) + bot * fy;
                }
                c
            } else {
                at(((x as f32 + 0.5) * sx) as isize, ((y as f32 + 0.5) * sy) as isize)
            };
            let (px_x, px_y) = (INDENT * cw + x, top * ch + y);
            if px_x < pw {
                data[px_y * pw + px_x] = tiny_skia::PremultipliedColorU8::from_rgba(c[0] as u8, c[1] as u8, c[2] as u8, 255).unwrap();
            }
        }
    }
}

#[test]
#[ignore = "reads system fonts and writes PNGs for looking at"]
fn grid_and_image_side_by_side() {
    let Some(fonts) = Fonts::load() else {
        println!("no Cascadia Mono: skipped");
        return;
    };
    let cell = real_cell(&fonts);
    let caps = Caps { cell_w: VCELL.0 as u16, cell_h: VCELL.1 as u16, fg: FG, bg: BG };
    let mut typeset = marlowe_typeset_helper::typesetter();
    let mut blocks = Vec::new();
    for src in FORMULAS {
        let f = math::display(src);
        let job = Job { markup: f.typst.clone().expect("typst"), fg: FG, cols: COLS, rows: f.rows.len() as u16 };
        match fit::image(&job, caps, &mut typeset) {
            Ok(img) => blocks.push((f.rows.clone(), virtual_plane(&img.strips, COLS as usize))),
            Err(e) => println!("{src}: on the grid ({e})"),
        }
    }
    let cols = blocks.iter().map(|(r, _)| r.iter().map(|l| l.chars().count()).max().unwrap_or(0)).max().unwrap_or(0) + INDENT * 2;
    let img_cols = blocks.iter().map(|(_, p)| p.0.div_ceil(VCELL.0)).max().unwrap_or(0) + INDENT * 2;
    let rows: usize = blocks.iter().map(|(r, _)| r.len() * 2 + 2).sum::<usize>() + 1;
    let out = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/maths-compare");
    std::fs::create_dir_all(&out).unwrap();
    for bilinear in [true, false] {
        let mut pm = tiny_skia::Pixmap::new((cols.max(img_cols) * cell.0) as u32, (rows * cell.1) as u32).unwrap();
        pm.fill(tiny_skia::Color::from_rgba8(BG[0], BG[1], BG[2], 255));
        let mut top = 1;
        for (grid, plane) in &blocks {
            draw_grid(&mut pm, &fonts, grid, top, cell);
            top += grid.len() + 1;
            draw_image(&mut pm, plane, top, (cell.0, cell.1), bilinear);
            top += grid.len() + 1;
        }
        let name = if bilinear { "bilinear.png" } else { "nearest.png" };
        pm.save_png(out.join(name)).unwrap();
        println!("wrote {}", out.join(name).display());
    }
    // The first image at its own 10×20 pixels, magnified 4x without smoothing.
    if let Some((_, (vw, vh, p))) = blocks.first() {
        let k = 4;
        let mut pm = tiny_skia::Pixmap::new((vw * k) as u32, (vh * k) as u32).unwrap();
        let w = pm.width() as usize;
        for (i, px) in pm.pixels_mut().iter_mut().enumerate() {
            let (x, y) = ((i % w) / k, (i / w) / k);
            let c = p[y * vw + x];
            *px = tiny_skia::PremultipliedColorU8::from_rgba(c[0] as u8, c[1] as u8, c[2] as u8, 255).unwrap();
        }
        pm.save_png(out.join("wt-cell.png")).unwrap();
    }
}
