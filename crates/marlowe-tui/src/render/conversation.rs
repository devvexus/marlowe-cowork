//! The conversation pane.

use marlowe_surface::chrome::{self, Ink};
use marlowe_surface::{markdown, Theme};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::{ink, note_style, put};
use crate::state::{Conn, Entry, State, ToolStatus};
use crate::text;

/// Left padding inside the pane.
const PAD: u16 = 2;
/// Width of the `you  ` label.
const USER_LABEL: usize = 5;

pub fn draw(r: Rect, state: &State, theme: &Theme, buf: &mut Buffer) {
    if r.width < 10 || r.height == 0 {
        return;
    }
    // Text column: padding on the left, one blank column and the scrollbar on the right.
    let tw = r.width.saturating_sub(PAD + 2) as usize;
    let lines = transcript_lines(state, tw, theme);
    if lines.is_empty() {
        empty_state(r, state, theme, buf);
        return;
    }
    let h = r.height as usize;
    let total = lines.len();
    let max = total.saturating_sub(h);
    state.scroll_max.set(max);
    let scroll = state.scroll.min(max);
    let start = total.saturating_sub(h + scroll);
    for (i, l) in lines.iter().skip(start).take(h).enumerate() {
        put(buf, r.x + PAD, r.y + i as u16, l, tw as u16);
    }
    if total > h {
        scrollbar(r, total, h, start, max, theme, buf);
    }
    if scroll > 0 {
        let tag = Line::from(Span::styled(format!(" ↓ {scroll} more "), ink(Ink::Dim, theme)));
        let w = tag.width() as u16;
        put(buf, r.right().saturating_sub(w + 2), r.bottom() - 1, &tag, w);
    }
}

fn scrollbar(r: Rect, total: usize, h: usize, start: usize, max: usize, theme: &Theme, buf: &mut Buffer) {
    let x = r.right() - 1;
    let thumb = (h * h / total).clamp(1, h);
    let pos = (start * (h - thumb)).checked_div(max).unwrap_or(0);
    for i in 0..h {
        let (sym, st) = if i >= pos && i < pos + thumb {
            (chrome::SCROLL_THUMB, ink(Ink::Hover, theme))
        } else {
            (chrome::SCROLL_TRACK, ink(Ink::StructureDim, theme))
        };
        if let Some(c) = buf.cell_mut((x, r.y + i as u16)) {
            c.set_symbol(&sym.to_string()).set_style(st);
        }
    }
}

/// The whole transcript as styled lines, wrapped to `width`.
pub fn transcript_lines(state: &State, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut prev: Option<&Entry> = None;
    for e in &state.entries {
        let gap = match (prev, e) {
            (None, _) => false,
            (Some(_), Entry::User { .. }) => true,
            (Some(Entry::User { .. }), _) => false,
            (Some(Entry::Tool { .. } | Entry::Thinking { .. }), Entry::Prose { .. }) => true,
            (Some(Entry::Prose { .. }), Entry::Tool { .. } | Entry::Thinking { .. }) => true,
            _ => false,
        };
        if gap {
            out.push(Line::default());
        }
        entry_lines(e, state, width, theme, &mut out);
        prev = Some(e);
    }
    if let Some(p) = &state.pending_user {
        if !out.is_empty() {
            out.push(Line::default());
        }
        user_lines(p, "you", ink(Ink::Dim, theme), width, theme, &mut out);
    }
    // Nothing has streamed yet: say so rather than sit still.
    let waiting = state.pending_user.is_some()
        || (state.live.is_some() && matches!(state.entries.last(), Some(Entry::User { .. }) | None));
    if waiting {
        out.push(Line::from(vec![
            Span::styled(format!("{} ", chrome::DISCLOSURE_CLOSED), ink(Ink::Hover, theme)),
            Span::styled("thinking…", ink(Ink::Dim, theme)),
        ]));
    }
    out
}

fn entry_lines(e: &Entry, state: &State, width: usize, theme: &Theme, out: &mut Vec<Line<'static>>) {
    match e {
        Entry::User { text, steer } => {
            let label = if *steer { "steer" } else { "you" };
            user_lines(text, label, theme.normal(), width, theme, out);
        }
        Entry::Prose { text } => {
            let base = Style::default().fg(theme.speech());
            out.extend(markdown::render_prose(text, width, theme, base));
        }
        Entry::Thinking { text, tokens, live } => {
            let n = if *tokens > 0 { *tokens } else { (text.chars().count() as u64).div_ceil(4) };
            let open = state.show_thinking && !text.trim().is_empty();
            let marker = if open { chrome::DISCLOSURE_OPEN } else { chrome::DISCLOSURE_CLOSED };
            let head = if *live {
                vec![
                    Span::styled(format!("{marker} "), ink(Ink::Hover, theme)),
                    Span::styled("thinking… ", ink(Ink::Dim, theme)),
                    Span::styled(format!("{} tokens", text::thousands(n)), ink(Ink::Dimmer, theme)),
                ]
            } else {
                vec![
                    Span::styled(format!("{marker} "), ink(Ink::Dimmer, theme)),
                    Span::styled(format!("thought · {} tokens", text::thousands(n)), ink(Ink::Dimmer, theme)),
                ]
            };
            out.push(Line::from(head));
            if open {
                let rule = format!("{} ", chrome::QUOTE_RULE);
                for l in text::wrap(&text::clean(text.trim()), width.saturating_sub(2)) {
                    out.push(Line::from(vec![
                        Span::styled(rule.clone(), ink(Ink::Dimmer, theme)),
                        Span::styled(l, ink(Ink::Dim, theme)),
                    ]));
                }
            }
        }
        Entry::Tool { summary, status, .. } => {
            let marker = format!("{} ", chrome::TOOL_MARKER);
            let summary = text::clean(summary.lines().next().unwrap_or(""));
            let (marker_style, tail) = match status {
                ToolStatus::Running => (ink(Ink::Accent, theme), Some(("…".to_string(), ink(Ink::Dim, theme)))),
                ToolStatus::Done(s) if s.trim().is_empty() => (ink(Ink::Dim, theme), None),
                ToolStatus::Done(s) => (ink(Ink::Dim, theme), Some((text::clean(s), ink(Ink::Dim, theme)))),
                ToolStatus::Failed(s) => {
                    let s = if s.trim().is_empty() { "failed".to_string() } else { text::clean(s) };
                    (ink(Ink::Red, theme), Some((s, ink(Ink::Red, theme))))
                }
            };
            let tail_w = tail.as_ref().map_or(0, |(t, _)| text::width(t) + 3);
            let head = text::truncate(&summary, width.saturating_sub(2 + tail_w.min(width / 2)).max(8));
            let mut spans = vec![Span::styled(marker, marker_style), Span::styled(head, ink(Ink::Dim, theme))];
            if let Some((t, st)) = tail {
                let room = width.saturating_sub(2 + text::width(&spans[1].content) + 3);
                if t == "…" {
                    spans.push(Span::styled(" …", st));
                } else {
                    spans.push(Span::styled(" · ", ink(Ink::Dimmer, theme)));
                    spans.push(Span::styled(text::truncate(t.lines().next().unwrap_or(""), room), st));
                }
            }
            out.push(Line::from(spans));
        }
        Entry::Note { text: t, tone } => {
            for (i, l) in text::wrap(&text::clean(t), width.saturating_sub(2)).into_iter().enumerate() {
                let lead = if i == 0 { "· " } else { "  " };
                out.push(Line::from(vec![
                    Span::styled(lead, ink(Ink::Dimmer, theme)),
                    Span::styled(l, note_style(*tone, theme)),
                ]));
            }
        }
        Entry::Usage { usage } => {
            let mut parts = vec![
                format!("{} in", text::compact(usage.input)),
                format!("{} out", text::compact(usage.output)),
            ];
            if usage.reasoning > 0 {
                parts.push(format!("{} reasoning", text::compact(usage.reasoning)));
            }
            if let Some(c) = usage.cost_usd {
                parts.push(text::cost(c));
            }
            let s = parts.join(" · ");
            let pad = width.saturating_sub(text::width(&s));
            out.push(Line::from(vec![
                Span::raw(" ".repeat(pad)),
                Span::styled(s, ink(Ink::Dimmer, theme)),
            ]));
        }
    }
}

fn user_lines(t: &str, label: &str, style: Style, width: usize, theme: &Theme, out: &mut Vec<Line<'static>>) {
    let body = width.saturating_sub(USER_LABEL);
    for (i, l) in text::wrap(&text::clean(t.trim_end()), body).into_iter().enumerate() {
        let lead = if i == 0 { text::pad(label, USER_LABEL) } else { " ".repeat(USER_LABEL) };
        out.push(Line::from(vec![Span::styled(lead, ink(Ink::Dim, theme)), Span::styled(l, style)]));
    }
}

fn empty_state(r: Rect, state: &State, theme: &Theme, buf: &mut Buffer) {
    let accent = ink(Ink::Accent, theme);
    let dim = ink(Ink::Dim, theme);
    let dimmer = ink(Ink::Dimmer, theme);
    let lines: Vec<Line> = match &state.conn {
        Conn::Connecting => vec![Line::from(Span::styled("connecting to the daemon…", dim))],
        Conn::Offline { reason, .. } if !state.library.known => vec![
            Line::from(Span::styled("the daemon is not answering", ink(Ink::Red, theme))),
            Line::from(""),
            Line::from(Span::styled(text::truncate(&text::clean(reason), 70), dim)),
            Line::from(Span::styled("retrying…", dimmer)),
        ],
        Conn::Refused(msg) => vec![
            Line::from(Span::styled("the daemon refused this client", ink(Ink::Red, theme))),
            Line::from(""),
            Line::from(Span::styled(text::truncate(&text::clean(msg), 70), dim)),
        ],
        _ if state.library.known && !state.library.has_models() => vec![
            Line::from(Span::styled("⣿  MARLOWE", accent.add_modifier(Modifier::BOLD))),
            Line::from(""),
            Line::from(""),
            Line::from(Span::styled("You have no models. Add one?", theme.bright())),
            Line::from(""),
            Line::from(Span::styled("Marlowe thinks with whichever models you give it —", dim)),
            Line::from(Span::styled("local ones through Ollama, hosted ones through OpenRouter.", dim)),
            Line::from(""),
            Line::from(""),
            Line::from(vec![
                Span::styled("↵", accent),
                Span::styled(" open the library      ", dim),
                Span::styled("^k", accent),
                Span::styled(" palette", dim),
            ]),
        ],
        _ => {
            let greeting = greeting(&state.clock);
            vec![
                Line::from(Span::styled("⣿  MARLOWE", accent.add_modifier(Modifier::BOLD))),
                Line::from(""),
                Line::from(""),
                Line::from(Span::styled(greeting, Style::default().fg(theme.speech()))),
                Line::from(""),
                Line::from(vec![
                    Span::styled("ask anything  ·  ", dimmer),
                    Span::styled("^o", dim),
                    Span::styled(" past sessions  ·  ", dimmer),
                    Span::styled("^l", dim),
                    Span::styled(" library  ·  ", dimmer),
                    Span::styled("^k", dim),
                    Span::styled(" palette", dimmer),
                ]),
            ]
        }
    };
    let top = r.y + r.height.saturating_sub(lines.len() as u16) * 2 / 5;
    for (i, l) in lines.iter().enumerate() {
        let w = l.width() as u16;
        let x = r.x + r.width.saturating_sub(w) / 2;
        let y = top + i as u16;
        if y < r.bottom() {
            put(buf, x, y, l, r.width);
        }
    }
}

/// `Good morning.` etc. from the clock's hour; plain `Hello.` when the clock is unknown.
fn greeting(clock: &str) -> &'static str {
    let hour = clock.split(':').next().and_then(|h| h.trim().parse::<u32>().ok());
    match hour {
        Some(5..=11) => "Good morning.",
        Some(12..=17) => "Good afternoon.",
        Some(18..=23) | Some(0..=4) => "Good evening.",
        _ => "Hello.",
    }
}
