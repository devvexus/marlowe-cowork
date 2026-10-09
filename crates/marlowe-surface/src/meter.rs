//! ADR-021's braille amplitude meter. 6 cells × 2 rows = 12 samples × 8 levels.
//!
//! # The widget animates nothing
//!
//! It renders the frame it is handed. The frame comes from the stub, which writes it only when its
//! sample source reports (`the amplitude source` returns `None` for `waiting`). So the
//! §B5 rule —
//!
//! > Motion means Marlowe is working. Stillness means the ball is in the user's court.
//!
//! — is carried by the *source*, not by a branch in here. There is no `if state == Waiting` in this
//! file, and there must not be: a freeze implemented as a special case is a freeze that comes back
//! the first time someone adds an eighth state.
//!
//! # Braille has no capability probe, by decision
//!
//! There is no way to detect whether a font renders U+2800–U+28FF; a missing glyph surfaces as
//! tofu, a blank, or a double-width box, none of which are distinguishable from a correctly drawn
//! dim frame by anything the program can measure. **No fallback is added.** A silent block-glyph
//! fallback would mean two users see two different indicators with nothing observing the
//! divergence. `marlowe doctor` prints the glyph row and asks the user to confirm it by eye
//! instead — see ADR-021.

use marlowe_view::{Frame, LEVELS};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

/// Cells across. Two braille dot-columns per cell → [`SAMPLES`].
pub const CELLS_WIDE: u16 = 6;

/// Cells tall. Four braille dot-rows per cell → [`LEVELS`].
pub const CELLS_TALL: u16 = 2;

const BRAILLE_BASE: u32 = 0x2800;

/// Dot bit for (column-in-cell 0..2, row-in-cell 0..4).
///
/// Braille's dot numbering is not row-major — dots 7 and 8 were added below the original six, so
/// the fourth row lives in the high bits. Getting this wrong produces a meter that looks plausible
/// and reads wrong at the bottom, which is the half that matters when amplitude is low.
const fn dot_bit(col: usize, row: usize) -> u8 {
    match (col, row) {
        (0, 0) => 0b0000_0001,
        (0, 1) => 0b0000_0010,
        (0, 2) => 0b0000_0100,
        (0, 3) => 0b0100_0000,
        (1, 0) => 0b0000_1000,
        (1, 1) => 0b0001_0000,
        (1, 2) => 0b0010_0000,
        (1, 3) => 0b1000_0000,
        _ => 0,
    }
}

/// The meter as two strings of [`CELLS_WIDE`] braille characters, top row first.
///
/// Levels are drawn **from the bottom up**, which is what makes it read as a level meter rather
/// than a bar chart hanging from the ceiling.
pub fn rows(frame: &Frame) -> [String; CELLS_TALL as usize] {
    let mut out = [String::new(), String::new()];
    for (row_idx, row) in out.iter_mut().enumerate() {
        for cell in 0..CELLS_WIDE as usize {
            let mut bits: u8 = 0;
            for col in 0..2usize {
                let sample = cell * 2 + col;
                let level = frame[sample].min(LEVELS) as usize;
                // Dot-row `r` (0 at the top of the meter) is lit when the column reaches it.
                for r in 0..4usize {
                    let dot_row_from_top = row_idx * 4 + r;
                    let dot_row_from_bottom = LEVELS as usize - dot_row_from_top;
                    if level >= dot_row_from_bottom {
                        bits |= dot_bit(col, r);
                    }
                }
            }
            row.push(char::from_u32(BRAILLE_BASE + bits as u32).expect("braille block"));
        }
    }
    out
}

/// Draw into `area`. Clips rather than wrapping if the caller gave it less than 6×2 — a meter that
/// reflowed onto three rows would stop being one glyph and start being a paragraph.
pub fn render(frame: &Frame, area: Rect, buf: &mut Buffer, style: Style) {
    let rows = rows(frame);
    for (i, row) in rows.iter().enumerate() {
        let y = area.y + i as u16;
        if y >= area.bottom() {
            break;
        }
        buf.set_stringn(area.x, y, row, area.width as usize, style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marlowe_view::SAMPLES;

    #[test]
    fn a_silent_frame_is_all_blank_braille_cells() {
        // U+2800 is a braille cell with no dots raised. It is not U+0020 — a real space would let
        // the terminal's own background through at a different width on some fonts, and the meter
        // would change size between silence and speech.
        let r = rows(&[0; SAMPLES]);
        assert_eq!(r[0], "\u{2800}".repeat(CELLS_WIDE as usize));
        assert_eq!(r[1], "\u{2800}".repeat(CELLS_WIDE as usize));
    }

    #[test]
    fn a_full_frame_raises_every_dot() {
        let r = rows(&[LEVELS; SAMPLES]);
        assert_eq!(r[0], "\u{28FF}".repeat(CELLS_WIDE as usize));
        assert_eq!(r[1], "\u{28FF}".repeat(CELLS_WIDE as usize));
    }

    #[test]
    fn levels_fill_from_the_bottom_up() {
        // Level 4 of 8 is exactly half: the bottom cell row full, the top row empty. If this
        // inverts, the meter reads loudest when it is quietest.
        let r = rows(&[4; SAMPLES]);
        assert_eq!(r[0], "\u{2800}".repeat(CELLS_WIDE as usize), "top row should be empty at half amplitude");
        assert_eq!(r[1], "\u{28FF}".repeat(CELLS_WIDE as usize), "bottom row should be full at half amplitude");
    }

    #[test]
    fn one_level_lights_only_the_lowest_dot_row() {
        let r = rows(&[1; SAMPLES]);
        assert_eq!(r[0], "\u{2800}".repeat(CELLS_WIDE as usize));
        // Dots 7 and 8 only: 0b1100_0000 = 0xC0 -> U+28C0.
        assert_eq!(r[1], "\u{28C0}".repeat(CELLS_WIDE as usize));
    }

    #[test]
    fn every_column_is_independent() {
        let mut f = [0u8; SAMPLES];
        f[0] = LEVELS;
        let r = rows(&f);
        // First cell: left dot-column full, right empty. A full column lights all four dot-rows of
        // BOTH cell rows, and in each the fourth row is dot 7 — bits 1,2,3,7 = 0b0100_0111.
        assert_eq!(r[0].chars().next().unwrap(), '\u{2847}');
        assert_eq!(r[1].chars().next().unwrap(), '\u{2847}');
        assert_eq!(r[0].chars().nth(1).unwrap(), '\u{2800}');
    }

    #[test]
    fn the_meter_is_always_exactly_six_cells_wide() {
        // The status band's layout reserves 6×2. A meter that changed width with amplitude would
        // shift the text beside it on every frame, which is precisely the flicker §B12 forbids.
        for level in 0..=LEVELS {
            for row in rows(&[level; SAMPLES]) {
                assert_eq!(row.chars().count(), CELLS_WIDE as usize);
            }
        }
    }

    #[test]
    fn every_glyph_is_inside_the_braille_block() {
        for level in 0..=LEVELS {
            let mut f = [0u8; SAMPLES];
            for (i, c) in f.iter_mut().enumerate() {
                *c = (level + i as u8) % (LEVELS + 1);
            }
            for row in rows(&f) {
                for ch in row.chars() {
                    let cp = ch as u32;
                    assert!(
                        (0x2800..=0x28FF).contains(&cp),
                        "{ch:?} (U+{cp:04X}) is outside U+2800–U+28FF; ADR-021 pins the meter to \
                         the braille block and a stray glyph would be the fallback the ADR forbids"
                    );
                }
            }
        }
    }
}
