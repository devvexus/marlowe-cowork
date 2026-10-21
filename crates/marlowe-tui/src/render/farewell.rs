//! The shutdown, as the boot in reverse: each step of the daemon's close shows when it begins
//! and flips when it ends, then the goodnight line. Foreground only, in the conversation pane.

use marlowe_surface::chrome::Ink;
use marlowe_surface::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use super::{ink, put};
use crate::state::State;

/// The label column, dots included: wide enough for `unloading dusk-27b`.
const LABEL_W: usize = 26;
const BLOCK_W: u16 = 60;

pub fn draw(r: Rect, state: &State, theme: &Theme, buf: &mut Buffer) {
    let Some(f) = &state.farewell else { return };
    if r.width < 30 || r.height < 6 {
        return;
    }
    let value_w = (BLOCK_W as usize).min(r.width as usize).saturating_sub(LABEL_W + 4);
    let mut lines: Vec<Line> = f.lines().map(|l| super::seq::line(l, LABEL_W, value_w, theme)).collect();
    lines.push(Line::from(""));
    if f.shut() {
        let bye = crate::seq::goodnight(state.user.as_ref().and_then(|u| u.address.as_deref()));
        lines.push(Line::from(Span::styled(bye, ink(Ink::Accent, theme).add_modifier(Modifier::BOLD))));
    } else {
        lines.push(Line::from(Span::styled("any key leaves now", ink(Ink::Dimmer, theme))));
    }
    let x = r.x + r.width.saturating_sub(BLOCK_W) / 2;
    let top = r.y + r.height.saturating_sub(lines.len() as u16) * 2 / 5;
    for (i, l) in lines.iter().enumerate() {
        let y = top + i as u16;
        if y < r.bottom() {
            put(buf, x, y, l, r.right().saturating_sub(x));
        }
    }
}
