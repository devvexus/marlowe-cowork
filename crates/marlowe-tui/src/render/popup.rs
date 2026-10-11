//! The completion lists that open above the Message box: `/` commands, `@` files, Ctrl+R history.

use marlowe_surface::chrome::Ink;
use marlowe_surface::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Widget};

use super::{ink, put, put_row, Geometry};
use crate::compose::{Popup, MENTION_ROWS, SEARCH_ROWS};
use crate::state::{State, Target};
use crate::text;

pub fn draw(g: &Geometry, state: &State, theme: &Theme, buf: &mut Buffer) {
    let (Some(composer), Some(p)) = (g.composer, state.popup()) else { return };
    let (title, header, rows) = match &p {
        Popup::Slash { rows } => (" commands ", None, rows.len().min(MENTION_ROWS)),
        Popup::Mention { matches, indexing, .. } => (" files ", indexing.then_some("looking through the workspace…"), matches.len().min(MENTION_ROWS).max(usize::from(*indexing))),
        Popup::Search { .. } => (" history ", Some(""), SEARCH_ROWS),
    };
    let want = rows + usize::from(header.is_some());
    let room = composer.y.saturating_sub(g.conversation_box.y + 1) as usize;
    let inner_h = want.min(room.saturating_sub(2));
    if inner_h == 0 {
        return;
    }
    let w = composer.width.saturating_sub(4).min(86);
    let h = inner_h as u16 + 2;
    let r = Rect { x: composer.x + 2, y: composer.y - h, width: w, height: h };
    Clear.render(r, buf);
    let border = ink(Ink::Structure, theme);
    Block::bordered().border_type(BorderType::Rounded).border_style(border).title(Span::styled(title, border)).render(r, buf);
    let inner = Rect { x: r.x + 2, y: r.y + 1, width: r.width.saturating_sub(4), height: inner_h as u16 };
    let sel = state.code.popup_sel.min(p.len().saturating_sub(1));
    let dim = ink(Ink::Dim, theme);
    let dimmer = ink(Ink::Dimmer, theme);
    match &p {
        Popup::Slash { rows } => {
            let first = sel.saturating_sub(inner_h - 1);
            for (i, row) in rows.iter().enumerate().skip(first).take(inner_h) {
                let on = i == sel;
                let name = format!("/{}", row.name);
                let mut spans = vec![
                    Span::styled(if on { "› " } else { "  " }, ink(Ink::Accent, theme)),
                    Span::styled(text::pad(&name, 16), if on { ink(Ink::Accent, theme).add_modifier(Modifier::BOLD) } else { theme.normal() }),
                ];
                if !row.hint.is_empty() {
                    spans.push(Span::styled(format!("{} ", text::truncate(&text::clean(&row.hint), 26)), dimmer));
                }
                let used = Line::from(spans.clone()).width();
                let room = (inner.width as usize).saturating_sub(used);
                spans.push(Span::styled(text::truncate(&text::clean(&row.help), room), dim));
                put_row(state, buf, (inner.x, inner.y + (i - first) as u16, inner.width), Line::from(spans), Target::Row(i), theme);
            }
        }
        Popup::Mention { matches, indexing, .. } => {
            if let (true, Some(msg)) = (*indexing, header) {
                put(buf, inner.x, inner.y, &Line::from(Span::styled(msg, dim)), inner.width);
            }
            let first = sel.saturating_sub(inner_h.saturating_sub(1));
            for (i, m) in matches.iter().enumerate().skip(first).take(inner_h) {
                let on = i == sel;
                let clean = text::clean(m);
                let (dir, name) = match clean.rsplit_once('/') {
                    Some((d, n)) => (format!("{d}/"), n.to_string()),
                    None => (String::new(), clean.clone()),
                };
                let room = (inner.width as usize).saturating_sub(2);
                let dir = text::truncate_left(&dir, room.saturating_sub(text::width(&name)));
                let name_style = if on { ink(Ink::Accent, theme).add_modifier(Modifier::BOLD) } else { theme.normal() };
                let line = Line::from(vec![
                    Span::styled(if on { "› " } else { "  " }, ink(Ink::Accent, theme)),
                    Span::styled(dir, dimmer),
                    Span::styled(name, name_style),
                ]);
                put_row(state, buf, (inner.x, inner.y + (i - first) as u16, inner.width), line, Target::Row(i), theme);
            }
        }
        Popup::Search { query, matches, .. } => {
            let q = Line::from(vec![
                Span::styled("search › ", dim),
                Span::styled(text::clean(query), theme.bright()),
                Span::styled(if matches.is_empty() { "  nothing matches" } else { "" }, dimmer),
            ]);
            put(buf, inner.x, inner.y, &q, inner.width);
            let list = inner_h.saturating_sub(1);
            let first = sel.saturating_sub(list.saturating_sub(1));
            for (i, m) in matches.iter().enumerate().skip(first).take(list) {
                let on = i == sel;
                let one_line = text::clean(&m.replace('\n', " ⏎ "));
                let line = Line::from(vec![
                    Span::styled(if on { "› " } else { "  " }, ink(Ink::Accent, theme)),
                    Span::styled(text::truncate(&one_line, inner.width as usize - 2), if on { theme.bright() } else { dim }),
                ]);
                put(buf, inner.x, inner.y + 1 + (i - first) as u16, &line, inner.width);
            }
            let hint = Line::from(Span::styled("↵ use · ^r older · esc cancel", dimmer));
            let hw = hint.width() as u16;
            put(buf, r.right().saturating_sub(hw + 3), r.bottom() - 1, &hint, hw);
        }
    }
}
