//! The Situation Board rail: CREW, WATCH, THREADS, TODAY. M1 has no producers for any of them,
//! so each shows an honest empty state.

use marlowe_surface::chrome::Ink;
use marlowe_surface::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::{ink, put};
use crate::state::State;

const SECTIONS: [(&str, &str); 4] = [
    ("CREW", "no runs"),
    ("WATCH", "nothing watched"),
    ("THREADS", "no open threads"),
    ("TODAY", "nothing scheduled"),
];

pub fn draw(r: Rect, state: &State, theme: &Theme, buf: &mut Buffer) {
    let _ = state;
    let x = r.x + 2;
    let w = r.width.saturating_sub(3);
    let mut y = r.y;
    for (title, empty) in SECTIONS {
        if y + 2 > r.bottom() {
            break;
        }
        put(buf, x, y, &Line::from(Span::styled(title, ink(Ink::Accent, theme))), w);
        put(buf, x, y + 1, &Line::from(Span::styled(empty, ink(Ink::Dimmer, theme))), w);
        y += 3;
    }
}
