//! A project's report in the panel: the file the project wrote, through the markdown renderer in
//! Marlowe's voice colour, with the project's name and the file's path above it.

use marlowe_project::derive;
use marlowe_project::view::{self, Cell};
use marlowe_surface::chrome::Ink;
use marlowe_surface::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use super::panel::frame;
use super::projects::{report_lines, wrap_clip, Pal};
use super::{ink, put};
use crate::panel::ReportUi;
use crate::state::State;
use crate::text;

/// The report panel's rows: the heading, the path, then the report.
pub fn rows(state: &State, ui: &ReportUi, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let pal = Pal::new(theme);
    let mut out: Vec<Line<'static>> = Vec::new();
    let Some(l) = state.projects.get(&ui.project) else {
        out.push(Line::from(Span::styled("this project is no longer open", pal.dim)));
        return out;
    };
    let s = &l.state;
    let (g, gs, word) = pal.project(s.status);
    out.push(Line::from(vec![
        Span::styled(format!("{g} "), gs),
        Span::styled(text::truncate(&text::clean_maths(&s.title), width.saturating_sub(4)), pal.bright),
    ]));
    let mut facts = vec![Span::styled(format!("{} · {} · {word}", s.id, s.kind), pal.dim)];
    if let Some(it) = derive::result_item(s) {
        for c in view::view(it, s, state.projects.now(state.now_ms)).cells {
            if let Cell::Badge { text, tone, .. } = c.cell {
                facts.push(Span::styled(" · ", pal.dimmer));
                facts.push(Span::styled(format!("{text} confidence"), pal.tone(tone)));
                break;
            }
        }
    }
    out.push(Line::from(facts));
    if let Some(r) = &l.report {
        out.push(Line::from(vec![Span::styled("file  ", pal.dim), Span::styled(text::truncate_left(&r.path, width.saturating_sub(6)), pal.body)]));
    }
    out.push(Line::default());
    let body = report_lines(l, width, theme);
    if body.is_empty() {
        // The file could not be read: what the project recorded about it stands in.
        let fallback = derive::result_item(s).map(|it| view::view(it, s, state.projects.now(state.now_ms)));
        let mut any = false;
        for c in fallback.iter().flat_map(|r| r.cells.iter()) {
            if let Cell::Text { text, lines } = &c.cell {
                if c.key == "path" {
                    continue;
                }
                for t in wrap_clip(text, width, (*lines).max(1) as usize) {
                    out.push(Line::from(Span::styled(t, pal.speech)));
                    any = true;
                }
            }
        }
        out.push(Line::default());
        out.push(Line::from(Span::styled(if any { "the file could not be read; this is what the project recorded" } else { "the file could not be read" }, ink(Ink::Amber, theme))));
    } else {
        out.extend(body);
    }
    out
}

pub fn draw(area: Rect, state: &State, ui: &ReportUi, theme: &Theme, buf: &mut Buffer) {
    let h = area.height.saturating_sub(4).max(8);
    let inner = frame(area, 116, h, "report", &[("↑↓", "scroll"), ("esc", "back")], theme, buf);
    let lines = rows(state, ui, inner.width as usize, theme);
    let n = inner.height as usize;
    ui.scroll.max.set(lines.len().saturating_sub(n));
    let first = ui.scroll.at();
    for (i, l) in lines.iter().skip(first).take(n).enumerate() {
        put(buf, inner.x, inner.y + i as u16, l, inner.width);
    }
    if first > 0 {
        let tag = Line::from(Span::styled(format!(" ↑ {first} above "), ink(Ink::Dim, theme).add_modifier(Modifier::empty())));
        let w = tag.width() as u16;
        put(buf, inner.right().saturating_sub(w), inner.bottom().saturating_sub(1), &tag, w);
    }
}
