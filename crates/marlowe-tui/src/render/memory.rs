//! The Memory view: search, results with kind, trust, date and provenance, forget and correct.

use marlowe_proto::{MemoryHit, MemoryScope, MemoryTrust};
use marlowe_surface::chrome::Ink;
use marlowe_surface::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use super::{ink, note_style, put};
use crate::memory::MemMode;
use crate::state::State;
use crate::text;

/// Rows above the results: heading, status, blank, query, blank.
const HEADER: u16 = 5;
/// Text lines the selected result may show.
const SELECTED_LINES: usize = 4;

fn content(main: Rect) -> Rect {
    Rect {
        x: main.x + 3,
        y: main.y + 2,
        width: main.width.saturating_sub(6),
        height: main.height.saturating_sub(4),
    }
}

/// Rows the confirm or correct prompt takes at the bottom of the content area.
fn prompt_rows(state: &State) -> u16 {
    match state.memory.mode {
        MemMode::Browse => 0,
        _ => 2,
    }
}

pub fn cursor(main: Rect, state: &State) -> Option<(u16, u16)> {
    let c = content(main);
    match &state.memory.mode {
        MemMode::Browse => Some((c.x + 2 + text::width(&state.memory.query).min(c.width as usize - 3) as u16, c.y + 3)),
        MemMode::Correct { input, .. } => {
            let before = &input.text()[..input.cursor()];
            let col = text::width(&text::clean(before)).min(c.width as usize - 3) as u16;
            Some((c.x + 2 + col, c.bottom() - 1))
        }
        MemMode::ConfirmForget { .. } => None,
    }
}

pub fn draw(main: Rect, state: &State, theme: &Theme, buf: &mut Buffer) {
    let c = content(main);
    if c.width < 20 || c.height < HEADER + 2 {
        return;
    }
    let m = &state.memory;
    let dim = ink(Ink::Dim, theme);
    let dimmer = ink(Ink::Dimmer, theme);
    let w = c.width;

    // Heading with the store's size.
    let mut head = vec![Span::styled("Memory", ink(Ink::Accent, theme).add_modifier(Modifier::BOLD))];
    if let Some(s) = &m.stats {
        let n = |v: u64| text::thousands(v);
        head.push(Span::styled(
            format!("  \u{b7}  {} memories \u{b7} {} episodes \u{b7} {} vectors", n(s.beliefs), n(s.episodes), n(s.vectors)),
            dim,
        ));
    }
    put(buf, c.x, c.y, &Line::from(head), w);

    // Second row: an import in flight, else a degraded warning, else which models are resident.
    let status = if let Some(i) = state.import {
        Line::from(Span::styled(
            format!("importing the old memories\u{2026} {} read \u{b7} {} imported", text::thousands(i.read), text::thousands(i.imported)),
            ink(Ink::Accent, theme),
        ))
    } else if let Some(why) = m.stats.as_ref().and_then(|s| s.degraded.as_ref()) {
        Line::from(Span::styled(
            text::truncate(&format!("degraded: {} \u{b7} keyword search only, nothing is injected on its own", text::clean(why)), w as usize),
            ink(Ink::Amber, theme),
        ))
    } else if let Some(s) = &m.stats {
        let models = if s.models_loaded.is_empty() {
            "search models unloaded (they load on first use)".to_string()
        } else {
            format!("loaded: {}", s.models_loaded.join(", "))
        };
        Line::from(Span::styled(text::truncate(&models, w as usize), dimmer))
    } else {
        Line::default()
    };
    put(buf, c.x, c.y + 1, &status, w);

    // Query row.
    let q = if m.query.is_empty() {
        Span::styled("search your memories", dimmer)
    } else {
        Span::styled(text::clean(&m.query), theme.normal())
    };
    put(buf, c.x, c.y + 3, &Line::from(vec![Span::styled("\u{203a} ", ink(Ink::Accent, theme)), q]), w);

    // Results.
    let list = Rect { y: c.y + HEADER, height: c.height - HEADER - prompt_rows(state), ..c };
    if m.hits.is_empty() {
        let msg = if m.searched { "nothing found" } else { "type a question or a name and press \u{21b5}" };
        put(buf, list.x + 2, list.y, &Line::from(Span::styled(msg, dim)), list.width.saturating_sub(2));
    } else {
        draw_hits(list, state, theme, buf);
    }

    // Prompt for forget or correct.
    let py = c.bottom() - 2;
    match &m.mode {
        MemMode::Browse => {}
        MemMode::ConfirmForget { id } => {
            let what = m.hits.iter().find(|h| &h.id == id).map_or(String::new(), |h| {
                text::truncate(&text::clean(h.text.lines().next().unwrap_or("")), w as usize - 24)
            });
            put(
                buf,
                c.x,
                py,
                &Line::from(vec![
                    Span::styled("forget this memory? ", ink(Ink::Amber, theme).add_modifier(Modifier::BOLD)),
                    Span::styled(what, dim),
                ]),
                w,
            );
            put(
                buf,
                c.x,
                py + 1,
                &Line::from(vec![
                    Span::styled("y ", dimmer),
                    Span::styled("forget", ink(Ink::Red, theme)),
                    Span::styled("      n ", dimmer),
                    Span::styled("keep", theme.normal()),
                ]),
                w,
            );
        }
        MemMode::Correct { input, .. } => {
            put(buf, c.x, py, &Line::from(Span::styled("correct this memory \u{b7} \u{21b5} saves, esc cancels", ink(Ink::Amber, theme))), w);
            let t = text::clean(input.text()).replace('\n', " ");
            put(
                buf,
                c.x,
                py + 1,
                &Line::from(vec![Span::styled("\u{203a} ", ink(Ink::Accent, theme)), Span::styled(t, theme.normal())]),
                w,
            );
        }
    }

    // Footer hints, then the flash.
    let hints: &[(&str, &str)] = match &m.mode {
        MemMode::Browse => &[("\u{21b5}", "search"), ("\u{2191}\u{2193}", "select"), ("^e", "correct"), ("^d", "forget"), ("esc", "back")],
        MemMode::ConfirmForget { .. } => &[("y", "forget"), ("n", "keep")],
        MemMode::Correct { .. } => &[("\u{21b5}", "save"), ("esc", "cancel")],
    };
    let mut spans = vec![Span::raw(" ")];
    for (i, (k, v)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  \u{b7}  ", dimmer));
        }
        spans.push(Span::styled(*k, dim));
        spans.push(Span::styled(format!(" {v}"), dimmer));
    }
    spans.push(Span::raw(" "));
    let l = Line::from(spans);
    let hw = l.width() as u16;
    put(buf, main.right().saturating_sub(hw + 2), main.bottom() - 1, &l, hw);
    if let Some(f) = &state.flash {
        let max = main.width.saturating_sub(hw + 8);
        let fl = Line::from(Span::styled(
            format!(" {} ", text::truncate(&text::clean(&f.text), max as usize)),
            note_style(f.tone, theme),
        ));
        put(buf, main.x + 2, main.bottom() - 1, &fl, max + 2);
    }
}

fn draw_hits(list: Rect, state: &State, theme: &Theme, buf: &mut Buffer) {
    let m = &state.memory;
    let tw = list.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let (mut sel_start, mut sel_end) = (0, 0);
    for (i, h) in m.hits.iter().enumerate() {
        let selected = i == m.sel;
        if selected {
            sel_start = lines.len();
        }
        let marker = if selected { Span::styled("\u{203a} ", ink(Ink::Accent, theme)) } else { Span::raw("  ") };
        let style = if selected { theme.bright() } else { theme.normal() };
        let body = text::clean(h.text.trim());
        if selected {
            let wrapped = text::wrap(&body, tw);
            let more = wrapped.len() > SELECTED_LINES;
            for (j, l) in wrapped.into_iter().take(SELECTED_LINES).enumerate() {
                let l = if more && j == SELECTED_LINES - 1 { text::truncate(&format!("{l}\u{2026}"), tw) } else { l };
                let lead = if j == 0 { marker.clone() } else { Span::raw("  ") };
                lines.push(Line::from(vec![lead, Span::styled(l, style)]));
            }
        } else {
            let first = body.lines().next().unwrap_or("");
            lines.push(Line::from(vec![marker, Span::styled(text::truncate(first, tw), style)]));
        }
        lines.push(meta_line(h, tw, theme));
        if selected {
            sel_end = lines.len();
        }
    }
    let h = list.height as usize;
    let first = sel_end.saturating_sub(h).min(sel_start);
    for (i, l) in lines.iter().skip(first).take(h).enumerate() {
        put(buf, list.x, list.y + i as u16, l, list.width);
    }
    if first > 0 {
        mark(list, list.y, "\u{2191}", theme, buf);
    }
    if lines.len() > first + h {
        mark(list, list.bottom() - 1, "\u{2193}", theme, buf);
    }
}

fn mark(list: Rect, y: u16, sym: &str, theme: &Theme, buf: &mut Buffer) {
    put(buf, list.right().saturating_sub(2), y, &Line::from(Span::styled(sym.to_string(), ink(Ink::Dimmer, theme))), 1);
}

/// `kind · trust · date · provenance`, indented under the text.
fn meta_line(h: &MemoryHit, width: usize, theme: &Theme) -> Line<'static> {
    let sep = || Span::styled(" \u{b7} ", ink(Ink::Dimmer, theme));
    let trust_ink = match h.trust {
        MemoryTrust::User => Ink::Accent,
        MemoryTrust::Untrusted => Ink::Amber,
        _ => Ink::Dim,
    };
    let date = chrono::DateTime::from_timestamp_millis(h.occurred_at_ms)
        .map_or_else(|| "undated".to_string(), |d| d.format("%Y-%m-%d").to_string());
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(h.kind.label(), ink(Ink::Dim, theme)),
        sep(),
        Span::styled(h.trust.label(), ink(trust_ink, theme)),
        sep(),
        Span::styled(date, ink(Ink::Dim, theme)),
    ];
    if let MemoryScope::Project(p) = &h.scope {
        spans.push(sep());
        spans.push(Span::styled(text::truncate(&text::short_path(p), 24), ink(Ink::Dim, theme)));
    }
    let used: usize = spans.iter().map(|s| s.width()).sum::<usize>() + 3;
    let room = width.saturating_sub(used);
    if room > 6 && !h.provenance.trim().is_empty() {
        spans.push(sep());
        spans.push(Span::styled(text::truncate(&text::clean(&h.provenance), room), ink(Ink::Dimmer, theme)));
    }
    Line::from(spans)
}
