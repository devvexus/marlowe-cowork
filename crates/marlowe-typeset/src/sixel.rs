//! **A Sixel encoder for one-colour images**, with a transparent background.
//!
//! A typeset formula is one colour on nothing, so the image is a coverage plane: how much of each
//! pixel the glyphs cover. Each coverage level becomes one palette register, the glyph colour
//! blended onto the terminal's background by that much. A pixel the glyphs do not touch is not
//! drawn at all: the image is sent with Sixel's transparent-background mode (`P2 = 1`), so the
//! terminal's own background shows through and there is never a filled box. Only the
//! anti-aliased edges depend on knowing the background colour.

/// What fraction of each pixel the glyphs cover, row-major, 0 to 255.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coverage {
    pub w: usize,
    pub h: usize,
    pub alpha: Vec<u8>,
}

impl Coverage {
    pub fn blank(w: usize, h: usize) -> Coverage {
        Coverage { w, h, alpha: vec![0; w * h] }
    }

    fn at(&self, x: usize, y: isize) -> u8 {
        if y < 0 || y as usize >= self.h || x >= self.w {
            return 0;
        }
        self.alpha[y as usize * self.w + x]
    }

    /// The same image with fully transparent rows and columns removed from every edge.
    pub fn trimmed(&self) -> Coverage {
        let row_used = |y: usize| self.alpha[y * self.w..(y + 1) * self.w].iter().any(|a| *a > 0);
        let col_used = |x: usize| (0..self.h).any(|y| self.alpha[y * self.w + x] > 0);
        let Some(top) = (0..self.h).find(|y| row_used(*y)) else {
            return Coverage::blank(0, 0);
        };
        let bottom = (0..self.h).rev().find(|y| row_used(*y)).unwrap_or(top);
        let left = (0..self.w).find(|x| col_used(*x)).unwrap_or(0);
        let right = (0..self.w).rev().find(|x| col_used(*x)).unwrap_or(left);
        let (w, h) = (right - left + 1, bottom - top + 1);
        let mut alpha = Vec::with_capacity(w * h);
        for y in top..=bottom {
            alpha.extend_from_slice(&self.alpha[y * self.w + left..y * self.w + right + 1]);
        }
        Coverage { w, h, alpha }
    }
}

/// Coverage levels, one palette register each. Sixteen is past what the eye separates on a glyph
/// edge at terminal sizes, and keeps every band to a handful of colour passes.
pub const LEVELS: u32 = 16;

fn level(a: u8) -> usize {
    ((u32::from(a) * LEVELS + 127) / 255) as usize
}

/// A colour channel as a Sixel percentage.
fn pct(c: f32) -> u32 {
    (c / 255.0 * 100.0).round().clamp(0.0, 100.0) as u32
}

/// Encodes rows `top .. top + h` of `cov` (rows outside it are transparent) as one Sixel image.
/// Empty when no pixel in those rows is covered, so the caller can skip sending it.
pub fn encode(cov: &Coverage, top: isize, h: usize, fg: [u8; 3], bg: [u8; 3]) -> Vec<u8> {
    let w = cov.w;
    let used: Vec<bool> = {
        let mut u = vec![false; LEVELS as usize + 1];
        for y in 0..h as isize {
            for x in 0..w {
                u[level(cov.at(x, top + y))] = true;
            }
        }
        u
    };
    if !used.iter().skip(1).any(|u| *u) {
        return Vec::new();
    }
    let mut out: Vec<u8> = Vec::with_capacity(w * h / 2 + 64);
    // DCS P1;P2;P3 q: aspect from the raster attributes, P2 = 1 leaves unset pixels transparent.
    out.extend_from_slice(format!("\x1bP0;1;0q\"1;1;{w};{h}").as_bytes());
    for (lvl, u) in used.iter().enumerate().skip(1) {
        if !*u {
            continue;
        }
        let t = lvl as f32 / LEVELS as f32;
        let mix = |i: usize| f32::from(bg[i]) + (f32::from(fg[i]) - f32::from(bg[i])) * t;
        out.extend_from_slice(format!("#{lvl};2;{};{};{}", pct(mix(0)), pct(mix(1)), pct(mix(2))).as_bytes());
    }
    let bands = h.div_ceil(6);
    let mut bits = vec![0u8; w];
    for band in 0..bands {
        let mut first = true;
        for (lvl, _) in used.iter().enumerate().skip(1).filter(|(_, u)| **u) {
            let mut any = false;
            for (x, b) in bits.iter_mut().enumerate() {
                *b = 0;
                for k in 0..6 {
                    let y = band * 6 + k;
                    if y < h && level(cov.at(x, top + y as isize)) == lvl {
                        *b |= 1 << k;
                        any = true;
                    }
                }
            }
            if !any {
                continue;
            }
            if !first {
                out.push(b'$');
            }
            first = false;
            out.extend_from_slice(format!("#{lvl}").as_bytes());
            run_length(&bits, &mut out);
        }
        if band + 1 < bands {
            out.push(b'-');
        }
    }
    out.extend_from_slice(b"\x1b\\");
    out
}

/// One colour pass of one band. Trailing empty columns are not sent.
fn run_length(bits: &[u8], out: &mut Vec<u8>) {
    let end = bits.iter().rposition(|b| *b != 0).map_or(0, |p| p + 1);
    let mut i = 0;
    while i < end {
        let b = bits[i];
        let mut n = 1;
        while i + n < end && bits[i + n] == b {
            n += 1;
        }
        let c = 63 + b;
        if n > 3 {
            out.extend_from_slice(format!("!{n}").as_bytes());
            out.push(c);
        } else {
            for _ in 0..n {
                out.push(c);
            }
        }
        i += n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_pixels(s: &[u8], w: usize, h: usize) -> Vec<Option<usize>> {
        // A minimal decoder for what `encode` emits: registers, `$`, `-`, `!n` and data bytes.
        let mut px = vec![None; w * h];
        let text = std::str::from_utf8(s).unwrap();
        let body = &text[text.find('q').unwrap() + 1..text.rfind('\x1b').unwrap()];
        let body = &body[body.find('#').unwrap()..];
        let (mut x, mut band, mut reg) = (0usize, 0usize, 0usize);
        let b = body.as_bytes();
        let mut i = 0;
        let mut repeat = 1usize;
        while i < b.len() {
            match b[i] {
                b'#' => {
                    let mut j = i + 1;
                    while j < b.len() && b[j].is_ascii_digit() {
                        j += 1;
                    }
                    reg = body[i + 1..j].parse().unwrap();
                    i = j;
                    // A definition, not a selection: skip `;2;r;g;b`.
                    if i < b.len() && b[i] == b';' {
                        while i < b.len() && (b[i] == b';' || b[i].is_ascii_digit()) {
                            i += 1;
                        }
                    }
                    continue;
                }
                b'$' => x = 0,
                b'-' => {
                    x = 0;
                    band += 1;
                }
                b'!' => {
                    let mut j = i + 1;
                    while b[j].is_ascii_digit() {
                        j += 1;
                    }
                    repeat = body[i + 1..j].parse().unwrap();
                    i = j;
                    continue;
                }
                c @ 63..=126 => {
                    for _ in 0..repeat {
                        for k in 0..6 {
                            let y = band * 6 + k;
                            if (c - 63) & (1 << k) != 0 && y < h {
                                px[y * w + x] = Some(reg);
                            }
                        }
                        x += 1;
                    }
                    repeat = 1;
                }
                _ => {}
            }
            i += 1;
        }
        px
    }

    #[test]
    fn uncovered_pixels_are_never_drawn_and_covered_ones_round_trip() {
        let (w, h) = (23, 20);
        let mut cov = Coverage::blank(w, h);
        for y in 0..h {
            for x in 0..w {
                if (x + y) % 3 == 0 {
                    cov.alpha[y * w + x] = ((x * 11 + y * 7) % 256) as u8;
                }
            }
        }
        let s = encode(&cov, 0, h, [0xcd, 0xc9, 0xd6], [0x0f, 0x0e, 0x14]);
        assert!(s.starts_with(b"\x1bP0;1;0q\"1;1;23;20"), "transparent-background mode");
        let px = decode_pixels(&s, w, h);
        for y in 0..h {
            for x in 0..w {
                let want = level(cov.alpha[y * w + x]);
                let got = px[y * w + x].unwrap_or(0);
                assert_eq!(got, want, "pixel ({x},{y})");
            }
        }
    }

    #[test]
    fn a_strip_with_nothing_in_it_is_not_sent_and_rows_outside_are_transparent() {
        let cov = Coverage { w: 4, h: 2, alpha: vec![255; 8] };
        assert!(encode(&cov, 10, 6, [255; 3], [0; 3]).is_empty());
        let s = encode(&cov, -1, 4, [255; 3], [0; 3]);
        let px = decode_pixels(&s, 4, 4);
        assert!(px[0..4].iter().all(Option::is_none), "the row above the image is transparent");
        assert!(px[4..12].iter().all(|p| *p == Some(LEVELS as usize)));
        assert!(px[12..16].iter().all(Option::is_none));
    }

    #[test]
    fn the_palette_blends_the_glyph_colour_onto_the_background() {
        let cov = Coverage { w: 1, h: 1, alpha: vec![255] };
        let s = String::from_utf8(encode(&cov, 0, 1, [0xb3, 0x9a, 0xe8], [0x0f, 0x0e, 0x14])).unwrap();
        // #b39ae8 = (70%, 60%, 91%).
        assert!(s.contains("#16;2;70;60;91"), "{s:?}");
    }

    #[test]
    fn trimming_keeps_only_the_covered_box() {
        let mut cov = Coverage::blank(5, 4);
        cov.alpha[5 + 2] = 9;
        cov.alpha[2 * 5 + 3] = 9;
        let t = cov.trimmed();
        assert_eq!((t.w, t.h), (2, 2));
        assert_eq!(t.alpha, vec![9, 0, 0, 9]);
    }
}
