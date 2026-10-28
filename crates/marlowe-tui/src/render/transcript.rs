//! Ctrl+O: the conversation with every tool call open: its arguments, its output or diff in full,
//! the thinking as written. It is the verbose view of Claude Code, over the same entries.

use marlowe_surface::chrome::{self, Ink};
use marlowe_surface::{markdown, Theme};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Widget};

use super::conversation::{entry_lines, user_lines};
use super::overlay::panel_hints;
use super::{ink, put};
use crate::panel::Scroll;
use crate::state::{Entry, State, ToolStatus};
use crate::text;

/// Rows of one tool's output the view shows before the rest (the daemon keeps all of it).
const OUTPUT_ROWS: usize = 400;

/// The transcript as styled rows wrapped to `width`.
pub fn lines(state: &State, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut prev: Option<&Entry> = None;
    for (idx, e) in state.entries.iter().enumerate() {
        let gap = !matches!((prev, e), (None, _) | (Some(Entry::User { .. }), _) | (Some(Entry::Tool { .. }), Entry::Tool { .. }))
            && !matches!(e, Entry::Usage { .. } | Entry::Recall { .. } | Entry::Note { .. } | Entry::Job { .. } | Entry::Timings(_) | Entry::Seq(_));
        if gap || matches!(e, Entry::User { .. }) && prev.is_some() {
            out.push(Line::default());
        }
        match e {
            Entry::User { text, steer } => {
                user_lines(text, if *steer { "steer" } else { "you" }, theme.normal(), width, theme, &mut out);
            }
            Entry::Prose { text } => {
                out.extend(markdown::render_prose(text, width, theme, Style::default().fg(theme.speech())));
            }
            Entry::Thinking { text: t, tokens, .. } => {
                let n = if *tokens > 0 { *tokens } else { (t.chars().count() as u64).div_ceil(4) };
                out.push(Line::from(vec![
                    Span::styled(format!("{} ", chrome::DISCLOSURE_OPEN), ink(Ink::Dimmer, theme)),
                    Span::styled(format!("thought · {} tokens", text::thousands(n)), ink(Ink::Dimmer, theme)),
                ]));
                let rule = format!("{} ", chrome::QUOTE_RULE);
                for l in text::wrap(&text::clean(t.trim()), width.saturating_sub(2)) {
                    out.push(Line::from(vec![Span::styled(rule.clone(), ink(Ink::Dimmer, theme)), Span::styled(l, ink(Ink::Dim, theme))]));
                }
            }
            Entry::Tool { call_id, summary, status } => tool(call_id, summary, status, state, width, theme, &mut out),
            Entry::Job { command, output, code, .. } => {
                let (word, ik) = match code {
                    None => ("running".to_string(), Ink::Green),
                    Some(0) => ("done".to_string(), Ink::Dim),
                    Some(c) => (format!("exit {c}"), Ink::Red),
                };
                out.push(Line::from(vec![
                    Span::styled(format!("{} $ ", chrome::TOOL_MARKER), ink(ik, theme)),
                    Span::styled(text::truncate(&text::clean(command), width.saturating_sub(16)), theme.normal()),
                    Span::styled(format!(" · {word}"), ink(ik, theme)),
                ]));
                body(output, width, theme, &mut out);
            }
            other => entry_lines(other, idx, state, width, theme, false, &mut out),
        }
        prev = Some(e);
    }
    out
}

/// One tool call in full: the diff viewer's rows for an edit, the quoted output for anything else.
pub fn call(area: Rect, state: &State, ui: &crate::panel::CallUi, theme: &Theme, buf: &mut Buffer) {
    let entry = state.entries.iter().find_map(|e| match e {
        Entry::Tool { call_id, summary, status } if *call_id == ui.call_id => Some((summary, status)),
        _ => None,
    });
    let title = entry.map_or_else(|| "call".to_string(), |(s, _)| format!("call · {}", text::truncate(&text::clean(s.lines().next().unwrap_or("")), 80)));
    let h = area.height.saturating_sub(4).max(8);
    let inner = super::panel::frame(area, 116, h, &title, &[("↑↓", "scroll"), ("esc", "back")], theme, buf);
    let mut lines: Vec<Line<'static>> = Vec::new();
    match entry {
        Some((summary, status)) => tool(&ui.call_id, summary, status, state, inner.width as usize, theme, &mut lines),
        None => lines.push(Line::from(Span::styled("this call is no longer in the conversation", ink(Ink::Dim, theme)))),
    }
    // What the call took, which the one-line summary leaves out.
    if let Some(&(start, Some(end))) = state.code.tool_times.get(&ui.call_id) {
        lines.insert(1.min(lines.len()), Line::from(Span::styled(format!("  took {}", super::strip::took((end - start).max(0) as u64)), ink(Ink::Dimmer, theme))));
    }
    let n = inner.height as usize;
    ui.scroll.max.set(lines.len().saturating_sub(n));
    for (i, l) in lines.iter().skip(ui.scroll.at()).take(n).enumerate() {
        put(buf, inner.x, inner.y + i as u16, l, inner.width);
    }
}

pub(super) fn tool(call_id: &str, summary: &str, status: &ToolStatus, state: &State, width: usize, theme: &Theme, out: &mut Vec<Line<'static>>) {
    let (marker_ink, tail) = match status {
        ToolStatus::Running => (Ink::Accent, "running…".to_string()),
        ToolStatus::Done(s) => (Ink::Dim, text::clean(s)),
        ToolStatus::Failed(s) => (Ink::Red, if s.trim().is_empty() { "failed".to_string() } else { text::clean(s) }),
    };
    let head = text::clean(summary.lines().next().unwrap_or(""));
    let mut spans = vec![
        Span::styled(format!("{} ", chrome::TOOL_MARKER), ink(marker_ink, theme)),
        Span::styled(text::truncate(&head, width.saturating_sub(4).max(8)), ink(Ink::Dim, theme)),
    ];
    if !tail.trim().is_empty() {
        spans.push(Span::styled(" · ", ink(Ink::Dimmer, theme)));
        spans.push(Span::styled(text::truncate(tail.lines().next().unwrap_or(""), width / 2), ink(marker_ink, theme)));
    }
    out.push(Line::from(spans));
    let Some(d) = state.code.tool_detail.get(call_id) else { return };
    // The rest of what the call said it was doing: arguments.
    let (_, args) = crate::state::split_summary(&d.started);
    if !args.trim().is_empty() && d.diff.is_none() {
        body(&args, width, theme, out);
    }
    match &d.diff {
        Some(diff) => {
            for l in crate::diff::render(diff, crate::diff::Layout::full(width.saturating_sub(4)), theme).lines {
                let mut spans = vec![Span::raw("    ")];
                spans.extend(l.spans);
                out.push(Line::from(spans));
            }
        }
        None => body(&d.output, width, theme, out),
    }
}

/// Text under a tool line, quoted with the rule and dimmed, capped.
fn body(s: &str, width: usize, theme: &Theme, out: &mut Vec<Line<'static>>) {
    if s.trim().is_empty() {
        return;
    }
    let rule = format!("  {} ", chrome::QUOTE_RULE);
    let all: Vec<&str> = s.lines().collect();
    for l in all.iter().take(OUTPUT_ROWS) {
        for w in text::wrap(&text::clean(l), width.saturating_sub(4)) {
            out.push(Line::from(vec![Span::styled(rule.clone(), ink(Ink::Dimmer, theme)), Span::styled(w, ink(Ink::Dim, theme))]));
        }
    }
    if all.len() > OUTPUT_ROWS {
        out.push(Line::from(Span::styled(format!("  … {} more rows", all.len() - OUTPUT_ROWS), ink(Ink::Dimmer, theme))));
    }
}

pub fn draw(area: Rect, state: &State, sc: &Scroll, theme: &Theme, buf: &mut Buffer) {
    if area.width < 20 || area.height < 6 {
        return;
    }
    let r = Rect { x: area.x, y: area.y + 1, width: area.width, height: area.height - 2 };
    Clear.render(r, buf);
    let border = ink(Ink::Accent, theme);
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(Span::styled(" transcript ", border))
        .render(r, buf);
    panel_hints(r, &[("↑↓ PgUp PgDn", "scroll"), ("home end", "first / latest"), ("ctrl+o or esc", "back")], theme, buf);
    let inner = Rect { x: r.x + 2, y: r.y + 1, width: r.width.saturating_sub(4), height: r.height.saturating_sub(2) };
    let all = lines(state, inner.width as usize, theme);
    if all.is_empty() {
        put(buf, inner.x, inner.y, &Line::from(Span::styled("nothing yet", ink(Ink::Dim, theme))), inner.width);
        return;
    }
    let rows = inner.height as usize;
    sc.max.set(all.len().saturating_sub(rows));
    let up = sc.at();
    let start = all.len().saturating_sub(rows + up);
    for (i, l) in all.iter().skip(start).take(rows).enumerate() {
        put(buf, inner.x, inner.y + i as u16, l, inner.width);
    }
    if up > 0 {
        let tag = Line::from(Span::styled(format!(" ↓ {up} more "), ink(Ink::Dim, theme)));
        let w = tag.width() as u16;
        put(buf, r.right().saturating_sub(w + 2), r.bottom() - 1, &tag, w);
    }
}
