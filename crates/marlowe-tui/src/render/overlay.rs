//! Floating panels: the approval modal, the ^k palette, the sessions list, the model picker.
//! `Clear` resets cells to the terminal default; nothing here sets a background.

use marlowe_surface::chrome::Ink;
use marlowe_surface::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Widget};

use super::{ink, put};
use crate::input::palette_matches;
use crate::state::{Approval, State, APPROVAL_CHOICES, APPROVAL_DETAIL_ROWS};
use crate::text;

fn centred(area: Rect, w: u16, h: u16, top_bias: bool) -> Rect {
    let w = w.min(area.width.saturating_sub(4));
    let h = h.min(area.height.saturating_sub(2));
    let y = if top_bias {
        area.y + area.height.saturating_sub(h) / 4
    } else {
        area.y + area.height.saturating_sub(h) / 2
    };
    Rect { x: area.x + area.width.saturating_sub(w) / 2, y, width: w, height: h }
}

fn panel(r: Rect, title: &str, border: ratatui::style::Style, double: bool, buf: &mut Buffer) -> Rect {
    Clear.render(r, buf);
    let bt = if double { BorderType::Double } else { BorderType::Rounded };
    Block::bordered()
        .border_type(bt)
        .border_style(border)
        .title(Span::styled(format!(" {title} "), border))
        .render(r, buf);
    Rect { x: r.x + 2, y: r.y + 1, width: r.width.saturating_sub(4), height: r.height.saturating_sub(2) }
}

/// Recedes everything already drawn: a foreground rewrite to the dimmest weight, never a fill.
fn scrim(area: Rect, theme: &Theme, buf: &mut Buffer) {
    let fg = Ink::Dimmer.color(theme);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(c) = buf.cell_mut((x, y)) {
                let s = c.style().remove_modifier(Modifier::BOLD).fg(fg);
                c.set_style(s);
            }
        }
    }
}

pub fn approval(area: Rect, state: &State, a: &Approval, theme: &Theme, buf: &mut Buffer) {
    scrim(area, theme, buf);
    let amber = ink(Ink::Amber, theme);
    let dim = ink(Ink::Dim, theme);
    let dimmer = ink(Ink::Dimmer, theme);
    let w: u16 = 78;
    let tw = (w - 4) as usize;

    let detail = a.detail_lines();
    let top = state.approval_scroll.min(a.detail_scroll_max());
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(text::clean(&a.tool), amber.add_modifier(Modifier::BOLD)),
        Span::styled("  wants to act", dim),
    ]));
    lines.push(Line::from(""));
    for l in text::wrap(&text::clean(&a.summary), tw) {
        lines.push(Line::from(Span::styled(l, theme.bright())));
    }
    if !detail.is_empty() {
        lines.push(Line::from(""));
        if top > 0 {
            lines.push(Line::from(Span::styled(format!("  ↑ {top} lines above"), dimmer)));
        }
        for l in detail.iter().skip(top).take(APPROVAL_DETAIL_ROWS) {
            lines.push(Line::from(vec![Span::styled("▏ ", dimmer), Span::styled(l.clone(), theme.normal())]));
        }
        let below = detail.len().saturating_sub(top + APPROVAL_DETAIL_ROWS);
        if below > 0 {
            lines.push(Line::from(Span::styled(format!("  ↓ {below} more lines"), dimmer)));
        }
    }
    if state.session.as_ref() != Some(&a.session) {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("from another session", dimmer)));
    }
    lines.push(Line::from(""));
    let labels = ["allow", "allow for session", "deny"];
    let keys = ["y", "a", "n"];
    let mut choice = Vec::new();
    for (i, (label, key)) in labels.iter().zip(keys).enumerate() {
        let sel = i == state.approval_sel;
        let allowed = APPROVAL_CHOICES[i].is_allowed();
        let marker = if sel { "› " } else { "  " };
        let style = match (sel, allowed) {
            (_, true) if !state.approval_read => dimmer,
            (true, true) => ink(Ink::Accent, theme).add_modifier(Modifier::BOLD),
            (true, false) => ink(Ink::Red, theme).add_modifier(Modifier::BOLD),
            (false, _) => theme.normal(),
        };
        choice.push(Span::styled(marker, ink(Ink::Accent, theme)));
        choice.push(Span::styled(format!("{key} "), dimmer));
        choice.push(Span::styled(*label, style));
        choice.push(Span::raw("      "));
    }
    lines.push(Line::from(choice));
    if !state.approval_read {
        lines.push(Line::from(Span::styled("read the whole request (↓ / PgDn) to enable allow", amber)));
    }
    let more = state.approvals.len().saturating_sub(1);
    if more > 0 {
        lines.push(Line::from(Span::styled(format!("{more} more waiting"), dimmer)));
    }

    let r = centred(area, w, lines.len() as u16 + 2, false);
    let inner = panel(r, "approval", amber, true, buf);
    for (i, l) in lines.iter().enumerate().take(inner.height as usize) {
        put(buf, inner.x, inner.y + i as u16, l, inner.width);
    }
}

pub fn palette(area: Rect, query: &str, sel: usize, theme: &Theme, buf: &mut Buffer) {
    let matches = palette_matches(query);
    let h = matches.len().max(1) as u16 + 4;
    let r = centred(area, 60, h, true);
    let inner = panel(r, "palette", ink(Ink::Accent, theme), false, buf);
    let q = if query.is_empty() {
        Line::from(vec![Span::styled("› ", ink(Ink::Accent, theme)), Span::styled("type a command", ink(Ink::Dimmer, theme))])
    } else {
        Line::from(vec![Span::styled("› ", ink(Ink::Accent, theme)), Span::styled(text::clean(query), theme.normal())])
    };
    put(buf, inner.x, inner.y, &q, inner.width);
    if matches.is_empty() {
        put(buf, inner.x + 2, inner.y + 2, &Line::from(Span::styled("nothing matches", ink(Ink::Dim, theme))), inner.width);
        return;
    }
    for (i, cmd) in matches.iter().enumerate() {
        let selected = i == sel;
        let y = inner.y + 2 + i as u16;
        if y >= inner.bottom() {
            break;
        }
        let label_style = if selected { ink(Ink::Accent, theme).add_modifier(Modifier::BOLD) } else { theme.normal() };
        let marker = if selected { Span::styled("› ", ink(Ink::Accent, theme)) } else { Span::raw("  ") };
        let chord = cmd.chord();
        let gap = (inner.width as usize).saturating_sub(2 + text::width(cmd.label()) + text::width(chord));
        put(
            buf,
            inner.x,
            y,
            &Line::from(vec![
                marker,
                Span::styled(cmd.label(), label_style),
                Span::raw(" ".repeat(gap)),
                Span::styled(chord, ink(Ink::Dimmer, theme)),
            ]),
            inner.width,
        );
    }
}

pub fn sessions(area: Rect, state: &State, sel: usize, theme: &Theme, buf: &mut Buffer) {
    let n = state.sessions.as_ref().map_or(1, |s| s.len().max(1));
    let r = centred(area, 96, n as u16 + 4, true);
    let inner = panel(r, "sessions", ink(Ink::Accent, theme), false, buf);
    let dim = ink(Ink::Dim, theme);
    let dimmer = ink(Ink::Dimmer, theme);
    let Some(list) = &state.sessions else {
        put(buf, inner.x, inner.y, &Line::from(Span::styled("loading…", dim)), inner.width);
        return;
    };
    if list.is_empty() {
        put(buf, inner.x, inner.y, &Line::from(Span::styled("no sessions yet", dim)), inner.width);
        return;
    }
    let rows = inner.height.saturating_sub(1) as usize;
    let first = sel.saturating_sub(rows.saturating_sub(1));
    for (i, s) in list.iter().enumerate().skip(first).take(rows) {
        let selected = i == sel;
        let current = state.session.as_ref() == Some(&s.id);
        let marker = if selected { Span::styled("› ", ink(Ink::Accent, theme)) } else { Span::raw("  ") };
        let title_style = if selected { theme.bright() } else { theme.normal() };
        let title = if s.title.trim().is_empty() { "untitled".to_string() } else { text::clean(&s.title) };
        let ws = s.workspace.as_deref().map(text::short_path).unwrap_or_default();
        let mut spans = vec![
            marker,
            Span::styled(text::pad(&title, 46), title_style),
            Span::styled(text::pad(&text::truncate(&ws, 20), 22), dim),
            Span::styled(text::pad(&text::ago(s.updated_ms, state.now_ms), 11), dimmer),
            Span::styled(format!("{} turns", s.turns), dimmer),
        ];
        if current {
            spans.push(Span::styled("  open", ink(Ink::Speech, theme)));
        }
        put(buf, inner.x, inner.y + (i - first) as u16, &Line::from(spans), inner.width);
    }
}

pub fn model_picker(r: Rect, state: &State, sel: usize, theme: &Theme, buf: &mut Buffer) {
    let inner = panel(r, "conductor", ink(Ink::Accent, theme), false, buf);
    let current = state.library.conductor();
    for (i, m) in state.library.models.iter().enumerate() {
        let y = inner.y + i as u16;
        if y >= inner.bottom() {
            return;
        }
        let selected = i == sel;
        let marker = if selected { Span::styled("› ", ink(Ink::Accent, theme)) } else { Span::raw("  ") };
        let name_style = if selected { theme.bright() } else { theme.normal() };
        let mut spans = vec![
            marker,
            Span::styled(text::short_model(&m.model, Some(&m.info)), name_style),
            Span::styled(format!("  {}", m.model.provider), ink(Ink::Dimmer, theme)),
        ];
        if current.as_ref() == Some(&m.model) {
            spans.push(Span::styled("  ✓", ink(Ink::Speech, theme)));
        }
        put(buf, inner.x, y, &Line::from(spans), inner.width);
    }
    let y = inner.y + state.library.models.len() as u16;
    if y < inner.bottom() {
        let selected = sel >= state.library.models.len();
        let marker = if selected { Span::styled("› ", ink(Ink::Accent, theme)) } else { Span::raw("  ") };
        put(
            buf,
            inner.x,
            y,
            &Line::from(vec![marker, Span::styled("open the library…", ink(Ink::Dim, theme))]),
            inner.width,
        );
    }
}
