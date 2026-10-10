//! **Fitting a typeset formula into the box its grid layout reserved**, and cutting it into one
//! Sixel strip per cell row. Runs in the helper (`marlowe-typeset.exe`); tests run it in process
//! with a stand-in typesetter.

use std::sync::Arc;

use crate::service::{Caps, Image, Job};
use crate::sixel::{self, Coverage};

/// Smallest scale at which a formula squeezed into its box is still worth an image.
pub const MIN_SCALE: f32 = 0.6;
/// The em, as a fraction of the cell height: a terminal font's line is about 1.25 em.
pub const EM_PER_CELL: f32 = 0.8;

/// Typesets a job and cuts it into strips, or `None` to leave it on the grid. `typeset` turns
/// (markup, em in pixels, colour) into a coverage plane.
pub fn image(
    job: &Job,
    caps: Caps,
    typeset: &mut dyn FnMut(&str, f32, [u8; 3]) -> Result<Coverage, String>,
) -> Result<Image, String> {
    let (cw, ch) = (usize::from(caps.cell_w.max(1)), usize::from(caps.cell_h.max(1)));
    let (box_w, box_h) = (usize::from(job.cols) * cw, usize::from(job.rows) * ch);
    if box_w == 0 || box_h == 0 {
        return Err("an empty box".into());
    }
    let em = ch as f32 * EM_PER_CELL;
    let mut cov = typeset(&job.markup, em, job.fg)?;
    if cov.w > box_w || cov.h > box_h {
        let s = (box_w as f32 / cov.w as f32).min(box_h as f32 / cov.h as f32);
        if s < MIN_SCALE {
            return Err(format!("needs {s:.2}x to fit its box"));
        }
        cov = typeset(&job.markup, em * s * 0.98, job.fg)?;
        if cov.w > box_w || cov.h > box_h {
            return Err("does not fit its box".into());
        }
    }
    if cov.w == 0 || cov.h == 0 {
        return Err("typeset to nothing".into());
    }
    let top = ((box_h - cov.h) / 2) as isize;
    let strips = (0..usize::from(job.rows))
        .map(|r| {
            let bytes = sixel::encode(&cov, (r * ch) as isize - top, ch, job.fg, caps.bg);
            Arc::<[u8]>::from(bytes)
        })
        .collect();
    Ok(Image { cols: cov.w.div_ceil(cw) as u16, strips })
}
