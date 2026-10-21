//! **Typeset maths images on screen**: where they go each frame, and sending them without flicker.
//!
//! Rendering stays a pure function of the state. While a frame is drawn, the conversation pane
//! *places* image strips ([`place`]) in cells it left blank; after ratatui has flushed the frame,
//! [`Emitter::flush`] sends the strips that need sending. The rules:
//!
//! - A strip is sent only into cells that are blank in the frame just drawn, so nothing drawn on
//!   top (an overlay, the scroll tag) is ever painted over.
//! - A strip already on screen at the same place, over cells nothing has touched, is not sent
//!   again. A still screen sends nothing; scrolling re-sends cached bytes and never re-encodes.
//! - A strip that has moved or gone is erased where ratatui will not overwrite it (cells blank
//!   in both frames), before anything is painted.
//!
//! One strip is one cell row, so a block half scrolled out of the pane shows the rows still in it.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use ratatui::buffer::Buffer;

/// One cell row of a typeset image, at a screen position.
#[derive(Debug, Clone)]
pub struct Placed {
    pub x: u16,
    pub y: u16,
    pub cols: u16,
    pub strip: Arc<[u8]>,
}

impl PartialEq for Placed {
    fn eq(&self, o: &Placed) -> bool {
        self.x == o.x && self.y == o.y && self.cols == o.cols && Arc::ptr_eq(&self.strip, &o.strip)
    }
}

static PLAN: Mutex<Vec<Placed>> = Mutex::new(Vec::new());

/// Starts a frame's plan. Called at the top of `render::draw`.
pub(crate) fn begin_frame() {
    if let Ok(mut p) = PLAN.lock() {
        p.clear();
    }
}

/// Places one strip in this frame.
pub(crate) fn place(p: Placed) {
    if let Ok(mut plan) = PLAN.lock() {
        plan.push(p);
    }
}

/// The strips placed in the frame drawn last. For tests.
pub fn planned() -> Vec<Placed> {
    PLAN.lock().map(|p| p.clone()).unwrap_or_default()
}

fn blank(buf: &Buffer, x: u16, y: u16) -> bool {
    buf.cell((x, y)).is_some_and(|c| c.symbol() == " ")
}

fn area_blank(buf: &Buffer, p: &Placed) -> bool {
    (p.x..p.x.saturating_add(p.cols)).all(|x| blank(buf, x, p.y))
}

/// What is on the terminal, as far as images go.
#[derive(Default)]
pub struct Emitter {
    shown: Vec<Placed>,
    prev: Option<Buffer>,
}

impl Emitter {
    /// Sends what the frame just drawn into `buf` needs, as one write.
    pub fn flush(&mut self, buf: &Buffer, out: &mut impl Write) -> io::Result<()> {
        let plan = std::mem::take(&mut *PLAN.lock().map_err(|_| io::Error::other("plan poisoned"))?);
        let bytes = self.frame(buf, plan);
        if !bytes.is_empty() {
            out.write_all(&bytes)?;
            out.flush()?;
        }
        Ok(())
    }

    /// The bytes for one frame. Separate from the I/O so it can be tested.
    pub fn frame(&mut self, buf: &Buffer, plan: Vec<Placed>) -> Vec<u8> {
        // No image on screen and none to place (almost every frame): nothing to remember, and
        // the whole buffer is not copied.
        if plan.is_empty() && self.shown.is_empty() {
            self.prev = None;
            return Vec::new();
        }
        // A resize clears the terminal: nothing is on screen any more.
        if self.prev.as_ref().is_some_and(|p| p.area != buf.area) {
            self.shown.clear();
            self.prev = None;
        }
        let mut bytes: Vec<u8> = Vec::new();
        let untouched = |prev: &Option<Buffer>, x: u16, y: u16| prev.as_ref().is_none_or(|p| blank(p, x, y));
        for old in &self.shown {
            if plan.contains(old) {
                continue;
            }
            // Cells ratatui left alone still hold the old pixels; a space erases them.
            let mut run: Option<u16> = None;
            for x in old.x..=old.x.saturating_add(old.cols) {
                let stale = x < old.x.saturating_add(old.cols) && blank(buf, x, old.y) && untouched(&self.prev, x, old.y);
                match (stale, run) {
                    (true, None) => run = Some(x),
                    (false, Some(from)) => {
                        bytes.extend_from_slice(format!("\x1b[0m\x1b[{};{}H", old.y + 1, from + 1).as_bytes());
                        bytes.extend(std::iter::repeat_n(b' ', usize::from(x - from)));
                        run = None;
                    }
                    _ => {}
                }
            }
        }
        let mut shown = Vec::with_capacity(plan.len());
        for p in plan {
            if !area_blank(buf, &p) {
                continue;
            }
            let still = self.shown.contains(&p) && self.prev.as_ref().is_some_and(|pb| area_blank(pb, &p));
            if !still {
                // Save the cursor, paint at the strip's cell, restore: the composer's cursor and
                // ratatui's idea of where it is both survive.
                bytes.extend_from_slice(format!("\x1b7\x1b[{};{}H", p.y + 1, p.x + 1).as_bytes());
                bytes.extend_from_slice(&p.strip);
                bytes.extend_from_slice(b"\x1b8");
            }
            shown.push(p);
        }
        self.shown = shown;
        self.prev = Some(buf.clone());
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    fn strip(tag: u8) -> Arc<[u8]> {
        Arc::from(vec![b'\x1b', b'P', tag, b'\x1b', b'\\'])
    }

    fn count(bytes: &[u8], needle: &[u8]) -> usize {
        bytes.windows(needle.len()).filter(|w| *w == needle).count()
    }

    #[test]
    fn a_still_screen_sends_nothing_and_a_move_erases_then_repaints() {
        let buf = Buffer::empty(Rect::new(0, 0, 20, 5));
        let s = strip(b'a');
        let mut e = Emitter::default();
        let at = |y| Placed { x: 2, y, cols: 4, strip: Arc::clone(&s) };
        let first = e.frame(&buf, vec![at(1)]);
        assert_eq!(count(&first, b"\x1bPa"), 1, "the strip is sent once");
        assert!(e.frame(&buf, vec![at(1)]).is_empty(), "an unchanged frame sends nothing");
        // Scrolled one row up: the old row is erased, the strip painted at the new row.
        let moved = e.frame(&buf, vec![at(0)]);
        let text = String::from_utf8_lossy(&moved);
        assert!(text.contains("\x1b[2;3H    "), "the old cells are erased: {text:?}");
        assert!(text.contains("\x1b[1;3H\x1bPa"), "painted at its new row: {text:?}");
        assert!(text.find("\x1b[2;3H").unwrap() < text.find("\x1bPa").unwrap(), "erase comes first");
    }

    #[test]
    fn a_strip_is_never_painted_over_something_drawn_on_top() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 5));
        let mut e = Emitter::default();
        let p = Placed { x: 2, y: 1, cols: 4, strip: strip(b'b') };
        assert!(!e.frame(&buf, vec![p.clone()]).is_empty());
        // An overlay covers one of its cells: not repainted, and not erased either.
        buf.cell_mut((3, 1)).unwrap().set_symbol("x");
        assert!(e.frame(&buf, vec![p.clone()]).is_empty());
        // The overlay closes: ratatui blanks the cell, which took the pixels with it. Repaint.
        let buf2 = Buffer::empty(Rect::new(0, 0, 20, 5));
        assert_eq!(count(&e.frame(&buf2, vec![p]), b"\x1bPb"), 1);
    }

    #[test]
    fn a_live_row_changing_elsewhere_never_resends_a_strip() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 6));
        let s = strip(b'd');
        let mut e = Emitter::default();
        let at = Placed { x: 2, y: 1, cols: 4, strip: Arc::clone(&s) };
        assert_eq!(count(&e.frame(&buf, vec![at.clone()]), b"\x1bPd"), 1);
        // A spinner and a timer on the live row, every frame: the strip stays where it is.
        for i in 0..50 {
            buf.set_string(2, 5, format!("working {i}"), ratatui::style::Style::default());
            assert!(e.frame(&buf, vec![at.clone()]).is_empty(), "frame {i} resent the image");
        }
    }

    #[test]
    fn frames_without_images_keep_no_copy_of_the_screen() {
        let buf = Buffer::empty(Rect::new(0, 0, 20, 5));
        let mut e = Emitter::default();
        assert!(e.frame(&buf, vec![]).is_empty());
        assert!(e.prev.is_none());
        // The first strip after that is still sent.
        let p = Placed { x: 0, y: 0, cols: 2, strip: strip(b'e') };
        assert_eq!(count(&e.frame(&buf, vec![p]), b"\x1bPe"), 1);
        assert!(e.prev.is_some());
    }
    #[test]
    fn a_resize_forgets_what_was_on_screen() {
        let mut e = Emitter::default();
        let p = Placed { x: 0, y: 0, cols: 2, strip: strip(b'c') };
        e.frame(&Buffer::empty(Rect::new(0, 0, 10, 3)), vec![p.clone()]);
        let after = e.frame(&Buffer::empty(Rect::new(0, 0, 12, 3)), vec![p]);
        assert_eq!(count(&after, b"\x1bPc"), 1, "the cleared screen needs the strip again");
    }
}
