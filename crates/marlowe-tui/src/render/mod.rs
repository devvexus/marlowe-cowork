//! Drawing. A pure function of `(State, Theme, area)`: no clock reads, no I/O.
//!
//! Every style here has a foreground only. The terminal's own background is the background.

mod boot;
mod conversation;
mod library;
mod memory;
mod overlay;
mod rail;
mod watch;

use marlowe_surface::chrome::Ink;
use marlowe_surface::{Theme, MIN_COLS, MIN_ROWS};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Widget};
use ratatui::Frame;

use crate::composer::MAX_VISIBLE_LINES;
use crate::state::{Activity, Conn, NoteTone, Overlay, State, View};
use crate::text;

/// Width of the right rail, borders excluded.
pub const RAIL_WIDTH: u16 = 34;

/// Where everything is on a frame of a given size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub too_small: bool,
    /// The main rounded frame (title bar on its top border).
    pub main: Rect,
    pub conversation: Rect,
    pub rail: Rect,
    /// The composer box, when the conversation view shows one.
    pub composer: Option<Rect>,
    /// The conductor model name in the title bar.
    pub model: Option<Rect>,
    /// The model picker dropdown, when open.
    pub picker: Option<Rect>,
}

/// Computes the layout. Shared by drawing and mouse hit-testing.
pub fn geometry(area: Rect, state: &State) -> Geometry {
    let too_small = area.width < MIN_COLS || area.height < MIN_ROWS;
    let mut g = Geometry {
        too_small,
        main: area,
        conversation: Rect::default(),
        rail: Rect::default(),
        composer: None,
        model: None,
        picker: None,
    };
    if too_small {
        return g;
    }
    let composer_h = if state.view == View::Conversation {
        let (lines, _) = state.composer.layout(composer_text_width(area.width));
        lines.len().clamp(1, MAX_VISIBLE_LINES) as u16 + 2
    } else {
        0
    };
    g.main = Rect { height: area.height - composer_h, ..area };
    if composer_h > 0 {
        g.composer = Some(Rect { y: g.main.bottom(), height: composer_h, ..area });
    }
    let inner = Rect {
        x: g.main.x + 1,
        y: g.main.y + 1,
        width: g.main.width - 2,
        height: g.main.height.saturating_sub(2),
    };
    let conv_w = inner.width - RAIL_WIDTH - 1;
    g.conversation = Rect { width: conv_w, ..inner };
    g.rail = Rect { x: inner.x + conv_w + 1, width: RAIL_WIDTH, ..inner };

    let (spans, model_at) = title_right(state);
    let w: usize = spans.iter().map(|s| s.width()).sum();
    let start = g.main.right().saturating_sub(2 + w as u16);
    if let Some((off, len)) = model_at {
        g.model = Some(Rect { x: start + off, y: g.main.y, width: len, height: 1 });
    }
    if let (Some(Overlay::ModelPicker { .. }), Some(m)) = (&state.overlay, g.model) {
        let names: Vec<String> = state
            .library
            .models
            .iter()
            .map(|lm| format!("{}  {}", text::short_model(&lm.model, Some(&lm.info)), lm.model.provider))
            .collect();
        let w = names.iter().map(|n| text::width(n)).max().unwrap_or(0).max(24) as u16 + 10;
        let h = state.library.models.len() as u16 + 1 + 2;
        let x = m.x.saturating_sub(2).min(area.right().saturating_sub(w));
        g.picker = Some(Rect { x, y: g.main.y + 1, width: w, height: h.min(area.height - 2) });
    }
    g
}

fn composer_text_width(cols: u16) -> usize {
    cols.saturating_sub(6) as usize
}

/// Draws the whole frame.
pub fn draw(frame: &mut Frame, state: &State, theme: &Theme) {
    let area = frame.area();
    let g = geometry(area, state);
    if g.too_small {
        draw_too_small(area, frame.buffer_mut(), theme);
        return;
    }
    let buf = frame.buffer_mut();
    draw_main_frame(&g, state, theme, buf);
    match state.view {
        View::Conversation => {
            if state.booting() {
                boot::draw(g.conversation, state, theme, buf);
            } else {
                conversation::draw(g.conversation, state, theme, buf);
            }
            rail::draw(g.rail, state, theme, buf);
        }
        View::Library => library::draw(g.main, state, theme, buf),
        View::Memory => memory::draw(g.main, state, theme, buf),
        View::Watch => watch::draw(g.main, state, theme, buf),
    }
    let cursor = g.composer.and_then(|r| draw_composer(r, state, theme, buf));

    match &state.overlay {
        Some(Overlay::Palette { query, sel }) => overlay::palette(area, query, *sel, theme, buf),
        Some(Overlay::ImportOffer { sel }) => overlay::import_offer(area, state, *sel, theme, buf),
        Some(Overlay::Sessions { sel }) => overlay::sessions(area, state, *sel, theme, buf),
        Some(Overlay::ModelPicker { sel }) => {
            if let Some(r) = g.picker {
                overlay::model_picker(r, state, *sel, theme, buf)
            }
        }
        None => {}
    }
    if state.overlay.is_none() && !state.booting() {
        boot::toasts(&g, state, theme, buf);
    }
    if let Some(a) = state.current_approval() {
        overlay::approval(area, state, a, theme, buf);
        return;
    }
    if state.overlay.is_none() && state.view == View::Conversation {
        if let Some(c) = cursor {
            frame.set_cursor_position(c);
        }
    }
    if let View::Library = state.view {
        if let Some(c) = library::cursor(g.main, state) {
            frame.set_cursor_position(c);
        }
    }
    if state.view == View::Memory && state.overlay.is_none() {
        if let Some(c) = memory::cursor(g.main, state) {
            frame.set_cursor_position(c);
        }
    }
    if state.view == View::Watch && state.overlay.is_none() {
        if let Some(c) = watch::cursor(g.main, state) {
            frame.set_cursor_position(c);
        }
    }
}

pub(crate) fn ink(i: Ink, theme: &Theme) -> Style {
    i.style(theme)
}

pub(crate) fn note_style(tone: NoteTone, theme: &Theme) -> Style {
    match tone {
        NoteTone::Dim => ink(Ink::Dim, theme),
        NoteTone::Amber => ink(Ink::Amber, theme),
        NoteTone::Red => ink(Ink::Red, theme),
    }
}

/// Writes a line clipped to `max` columns.
pub(crate) fn put(buf: &mut Buffer, x: u16, y: u16, line: &Line<'_>, max: u16) {
    buf.set_line(x, y, line, max);
}

fn draw_too_small(area: Rect, buf: &mut Buffer, theme: &Theme) {
    let lines = [
        Line::from(Span::styled("MARLOWE", ink(Ink::Accent, theme).add_modifier(Modifier::BOLD))),
        Line::from(""),
        Line::from(Span::styled(
            format!("needs a window of at least {MIN_COLS} × {MIN_ROWS}"),
            theme.normal(),
        )),
        Line::from(Span::styled(
            format!("this one is {} × {} — resize to continue", area.width, area.height),
            ink(Ink::Dim, theme),
        )),
    ];
    let top = area.y + area.height.saturating_sub(lines.len() as u16) / 2;
    for (i, l) in lines.iter().enumerate() {
        let w = l.width() as u16;
        let x = area.x + area.width.saturating_sub(w) / 2;
        let y = top + i as u16;
        if y < area.bottom() {
            put(buf, x, y, l, area.width);
        }
    }
}

/// The braille presence meter: six cells, one row, levels 0..=4 per dot column. It assembles
/// during the boot and follows the voice envelope while Marlowe speaks.
pub(crate) fn meter(state: &State) -> String {
    let act = state.activity();
    let boot = state.wake.boot.filter(|b| b.active(state.now_ms));
    let voice = state.wake.voice_levels(state.now_ms);
    let levels: [u8; 12] = if let Some(b) = boot {
        b.meter_levels(state.now_ms)
    } else if let (Some(v), false) = (voice, act.moving()) {
        v
    } else if act.moving() {
        let t = state.now_ms.max(0) as f64 / 1000.0;
        let speed = match act {
            Activity::Thinking => 4.2,
            Activity::Writing => 6.0,
            _ => 2.4,
        };
        std::array::from_fn(|i| {
            let a = (t * speed - i as f64 * 0.55).sin() * 0.6 + (t * speed * 2.3 + i as f64).sin() * 0.4;
            (1.0 + (a * 0.5 + 0.5) * 3.0).round().clamp(1.0, 4.0) as u8
        })
    } else if act == Activity::Awaiting {
        [1, 2, 3, 4, 3, 2, 2, 3, 4, 3, 2, 1]
    } else {
        [1; 12]
    };
    // Dots in a braille cell, bottom row first: left column (7,3,2,1), right column (8,6,5,4).
    const LEFT: [u32; 4] = [0x40, 0x04, 0x02, 0x01];
    const RIGHT: [u32; 4] = [0x80, 0x20, 0x10, 0x08];
    (0..6)
        .map(|c| {
            let left: u32 = LEFT.iter().take(levels[c * 2] as usize).sum();
            let right: u32 = RIGHT.iter().take(levels[c * 2 + 1] as usize).sum();
            let bits = left | right;
            char::from_u32(0x2800 + bits).unwrap_or(' ')
        })
        .collect()
}

/// The right half of the title bar, and the (offset, width) of the model name within it.
fn title_right(state: &State) -> (Vec<Span<'static>>, Option<(u16, u16)>) {
    // Styles are attached at draw time; here only text and roles matter for geometry.
    let act = state.activity();
    let mut parts: Vec<Span<'static>> = vec![
        Span::raw(" "),
        Span::raw(meter(state)),
        Span::raw(" "),
        Span::raw(if state.booting() { "waking" } else { act.label() }),
        Span::raw(" · "),
    ];
    let model = match state.library.conductor() {
        Some(r) => text::short_model(&r, state.library.info(&r)),
        None if state.library.known => "add a model".to_string(),
        None => "…".to_string(),
    };
    let offset: usize = parts.iter().map(|s| s.width()).sum();
    let label = format!("{model} ▾");
    let len = text::width(&label);
    parts.push(Span::raw(label));
    if let Some(ws) = state.shown_workspace() {
        parts.push(Span::raw(" · "));
        parts.push(Span::raw(text::truncate(&text::short_path(ws), 28)));
    }
    if !state.clock.is_empty() {
        parts.push(Span::raw(" · "));
        parts.push(Span::raw(state.clock.clone()));
    }
    parts.push(Span::raw(" "));
    (parts, Some((offset as u16, len as u16)))
}

fn draw_main_frame(g: &Geometry, state: &State, theme: &Theme, buf: &mut Buffer) {
    let structure = ink(Ink::Structure, theme);
    Block::bordered().border_type(BorderType::Rounded).border_style(structure).render(g.main, buf);

    // Left title: the name, then the session title or the view.
    let mut left = vec![
        Span::styled("─ ", structure),
        Span::styled("MARLOWE", ink(Ink::Accent, theme).add_modifier(Modifier::BOLD)),
        Span::raw(" "),
    ];
    let sub = match state.view {
        View::Library => Some("library".to_string()),
        View::Memory => Some("memory".to_string()),
        View::Watch => Some("watch".to_string()),
        View::Conversation => state.title.as_deref().map(text::clean),
    };
    if let Some(sub) = sub {
        left.push(Span::styled("· ", ink(Ink::Dimmer, theme)));
        left.push(Span::styled(text::truncate(&sub, 48), ink(Ink::Dim, theme)));
        left.push(Span::raw(" "));
    }
    put(buf, g.main.x + 1, g.main.y, &Line::from(left), g.main.width / 2);

    // Right title, styled by role.
    let (spans, _) = title_right(state);
    let act = state.activity();
    let meter_style = match act {
        _ if state.booting() => ink(Ink::Accent, theme),
        Activity::Speaking => ink(Ink::Speech, theme),
        Activity::Offline => ink(Ink::Red, theme),
        Activity::Awaiting => ink(Ink::Amber, theme),
        a if a.moving() => ink(Ink::Accent, theme),
        _ => ink(Ink::Dim, theme),
    };
    let act_style = match act {
        Activity::Offline => ink(Ink::Red, theme),
        Activity::Awaiting => ink(Ink::Amber, theme),
        Activity::Idle | Activity::Connecting => ink(Ink::Dim, theme),
        _ => ink(Ink::Speech, theme),
    };
    let model_style = if state.library.known && state.library.conductor().is_none() {
        ink(Ink::Amber, theme)
    } else {
        theme.normal()
    };
    let sep = ink(Ink::Dimmer, theme);
    let styled: Vec<Span> = spans
        .into_iter()
        .enumerate()
        .map(|(i, s)| {
            let st = match i {
                1 => meter_style,
                3 => act_style,
                5 => model_style,
                _ if s.content.trim() == "·" => sep,
                _ => ink(Ink::Dim, theme),
            };
            Span::styled(s.content, st)
        })
        .collect();
    let line = Line::from(styled);
    let w = line.width() as u16;
    let x = g.main.right().saturating_sub(2 + w);
    put(buf, x, g.main.y, &line, w);

    if state.view == View::Conversation {
        // The divider between conversation and rail, joined into the frame.
        let x = g.rail.x - 1;
        for y in g.conversation.y..g.conversation.bottom() {
            if let Some(c) = buf.cell_mut((x, y)) {
                c.set_symbol("│").set_style(structure);
            }
        }
        for (y, joint) in [(g.main.y, "┬"), (g.main.bottom() - 1, "┴")] {
            if let Some(c) = buf.cell_mut((x, y)) {
                if c.symbol() == "─" {
                    c.set_symbol(joint).set_style(structure);
                }
            }
        }
    }
}

/// Draws the composer box. Returns the cursor position.
fn draw_composer(r: Rect, state: &State, theme: &Theme, buf: &mut Buffer) -> Option<(u16, u16)> {
    let focused = state.overlay.is_none() && state.approvals.is_empty();
    let border = if focused { ink(Ink::Hover, theme) } else { ink(Ink::Structure, theme) };
    Block::bordered().border_type(BorderType::Rounded).border_style(border).render(r, buf);

    let tw = composer_text_width(r.width);
    let x0 = r.x + 4;
    let rows = r.height.saturating_sub(2) as usize;
    put(buf, r.x + 2, r.y + 1, &Line::from(Span::styled("›", ink(Ink::Accent, theme))), 1);

    let (lines, (crow, ccol)) = state.composer.layout(tw);
    let first = (crow + 1).saturating_sub(rows);
    if state.composer.text().is_empty() {
        let hint = if !state.conn.is_online() {
            "offline — reconnecting…"
        } else if state.library.known && !state.library.has_models() {
            "press ↵ to add a model"
        } else if state.is_busy() {
            "steer the running turn…"
        } else {
            "message Marlowe…"
        };
        put(buf, x0, r.y + 1, &Line::from(Span::styled(hint, ink(Ink::Dimmer, theme))), tw as u16);
    } else {
        for (i, l) in lines.iter().skip(first).take(rows).enumerate() {
            let clean = text::clean(l);
            put(buf, x0, r.y + 1 + i as u16, &Line::from(Span::styled(clean, theme.normal())), tw as u16);
        }
    }

    // Bottom border: flash on the left, key hints on the right.
    let key = ink(Ink::Dim, theme);
    let label = ink(Ink::Dimmer, theme);
    let mut hints = vec![Span::raw(" "), Span::styled("↵", key), Span::styled(" send  ", label)];
    hints.extend([Span::styled("⇧↵", key), Span::styled(" newline  ", label)]);
    if state.is_busy() {
        hints.extend([Span::styled("esc", ink(Ink::Amber, theme)), Span::styled(" interrupt  ", label)]);
    }
    hints.extend([Span::styled("^k", key), Span::styled(" palette ", label)]);
    let hl = Line::from(hints);
    let hw = hl.width() as u16;
    let by = r.bottom() - 1;
    put(buf, r.right().saturating_sub(hw + 2), by, &hl, hw);
    if let Some(f) = &state.flash {
        let max = r.width.saturating_sub(hw + 8) as usize;
        let l = Line::from(vec![
            Span::raw(" "),
            Span::styled(text::truncate(&text::clean(&f.text), max), note_style(f.tone, theme)),
            Span::raw(" "),
        ]);
        put(buf, r.x + 2, by, &l, max as u16 + 2);
    } else if let Conn::Offline { reason, attempt } = &state.conn {
        let max = r.width.saturating_sub(hw + 8) as usize;
        let msg = format!(" offline · {} · retry {attempt} ", text::clean(reason));
        let l = Line::from(Span::styled(text::truncate(&msg, max), ink(Ink::Red, theme)));
        put(buf, r.x + 2, by, &l, max as u16);
    } else if let Conn::Refused(msg) = &state.conn {
        let max = r.width.saturating_sub(hw + 8) as usize;
        let msg = format!(" refused · {} ", text::clean(msg));
        let l = Line::from(Span::styled(text::truncate(&msg, max), ink(Ink::Red, theme)));
        put(buf, r.x + 2, by, &l, max as u16);
    }

    let visible_row = crow.checked_sub(first)?;
    if visible_row >= rows {
        return None;
    }
    Some((x0 + ccol as u16, r.y + 1 + visible_row as u16))
}
