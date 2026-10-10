//! The conversation pane.

use marlowe_surface::chrome::{self, Ink};
use marlowe_surface::markdown::MathBlock;
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
    let (mut lines, maths) = transcript(state, tw, theme);
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
    if let Some(svc) = marlowe_typeset::installed() {
        let origin = (r.x + PAD, r.y);
        typeset_images(&mut lines, &maths, svc, origin, start, h, tw, theme);
    }
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

/// Display formulas on screen that have a typeset image: the grid rows give way to it.
///
/// The image fills exactly the rows and columns the grid layout reserved, so nothing moves when
/// it lands. Until it does, the block shows the held-maths placeholder over the same rows rather
/// than the grid, so a formula appears once. One Typst refuses keeps its grid rows.
#[allow(clippy::too_many_arguments)]
fn typeset_images(
    lines: &mut [Line<'static>],
    maths: &[MathBlock],
    svc: &marlowe_typeset::Service,
    origin: (u16, u16),
    start: usize,
    h: usize,
    tw: usize,
    theme: &Theme,
) {
    use marlowe_typeset::{Job, Slot};
    for b in maths {
        let Some(markup) = &b.formula.typst else { continue };
        // The grid's colour: an explicit one, or the terminal's own foreground for the default.
        let fg = match b.fg {
            ratatui::style::Color::Rgb(r, g, bl) => [r, g, bl],
            ratatui::style::Color::Reset => svc.caps().fg,
            _ => continue,
        };
        let end = b.line + b.rows;
        if end <= start || b.line >= start + h || end > lines.len() {
            continue;
        }
        let cols = b.cols.min(tw.saturating_sub(b.col));
        let job = Job { markup: markup.clone(), fg, cols: cols as u16, rows: b.rows as u16 };
        match svc.get(&job) {
            Slot::Failed => {}
            Slot::Pending => {
                lines[b.line] = Line::from(vec![
                    Span::raw(" ".repeat(b.col)),
                    Span::styled(markdown::MATHS_PLACEHOLDER, Style::default().fg(theme.speech()).add_modifier(Modifier::DIM)),
                ]);
                for l in &mut lines[b.line + 1..end] {
                    *l = Line::default();
                }
            }
            Slot::Ready(img) => {
                for (k, l) in lines[b.line..end].iter_mut().enumerate() {
                    *l = Line::default();
                    let at = b.line + k;
                    let strip = &img.strips[k];
                    if at >= start && at < start + h && !strip.is_empty() {
                        crate::images::place(crate::images::Placed {
                            x: origin.0 + b.col as u16,
                            y: origin.1 + (at - start) as u16,
                            cols: img.cols.min(cols as u16),
                            strip: std::sync::Arc::clone(strip),
                        });
                    }
                }
            }
        }
    }
}

/// The whole transcript as styled lines wrapped to `width`, and the display formulas in Marlowe's
/// replies that an image may replace.
fn transcript(state: &State, width: usize, theme: &Theme) -> (Vec<Line<'static>>, Vec<MathBlock>) {
    let mut maths: Vec<MathBlock> = Vec::new();
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut prev: Option<&Entry> = None;
    // The entry still being written: its unclosed maths is held behind a placeholder.
    let streaming = state.live.as_ref().map(|_| state.entries.len().saturating_sub(1));
    for (idx, e) in state.entries.iter().enumerate() {
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
        if let Entry::Prose { text } = e {
            // Marlowe's prose, with the position of each display formula kept.
            let base = Style::default().fg(theme.speech());
            let (lines, found) = markdown::render_prose_blocks(text, width, theme, base, streaming == Some(idx));
            let at = out.len();
            maths.extend(found.into_iter().map(|mut b| {
                b.line += at;
                b
            }));
            out.extend(lines);
        } else {
            entry_lines(e, state, width, theme, streaming == Some(idx), &mut out);
        }
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
        || (state.live.is_some()
            && matches!(
                state.entries.iter().rev().find(|e| !matches!(e, Entry::Recall { .. })),
                Some(Entry::User { .. }) | None
            ));
    if waiting {
        out.push(Line::from(vec![
            Span::styled(format!("{} ", chrome::DISCLOSURE_CLOSED), ink(Ink::Hover, theme)),
            Span::styled("thinking…", ink(Ink::Dim, theme)),
        ]));
    }
    (out, maths)
}

fn entry_lines(e: &Entry, state: &State, width: usize, theme: &Theme, live: bool, out: &mut Vec<Line<'static>>) {
    match e {
        Entry::User { text, steer } => {
            let label = if *steer { "steer" } else { "you" };
            user_lines(text, label, theme.normal(), width, theme, out);
        }
        Entry::Prose { text } => {
            let base = Style::default().fg(theme.speech());
            if live {
                out.extend(markdown::render_prose_live(text, width, theme, base));
            } else {
                out.extend(markdown::render_prose(text, width, theme, base));
            }
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
                let body = width.saturating_sub(2);
                let rows: Vec<Line<'static>> = if markdown::has_maths(text) {
                    // Reasoning is shown as written, except that its maths is typeset: LaTeX never
                    // reaches the screen as source, here or anywhere.
                    markdown::render_text(text.trim(), body, theme, ink(Ink::Dim, theme), *live)
                } else {
                    text::wrap(&text::clean(text.trim()), body)
                        .into_iter()
                        .map(|l| Line::from(Span::styled(l, ink(Ink::Dim, theme))))
                        .collect()
                };
                for l in rows {
                    let mut spans = vec![Span::styled(rule.clone(), ink(Ink::Dimmer, theme))];
                    spans.extend(l.spans);
                    out.push(Line::from(spans));
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
        Entry::Recall { count, summary } => {
            let n = format!("{count} memor{}", if *count == 1 { "y" } else { "ies" });
            let mut spans = vec![
                Span::styled("⋯ ", ink(Ink::Dimmer, theme)),
                Span::styled(format!("recall · {n}"), ink(Ink::Dim, theme)),
            ];
            let room = width.saturating_sub(text::width(&n) + 12);
            if !summary.trim().is_empty() && room > 8 {
                let first = summary.lines().next().unwrap_or("");
                spans.push(Span::styled(format!(" · {}", text::truncate(first, room)), ink(Ink::Dimmer, theme)));
            }
            out.push(Line::from(spans));
        }
        Entry::Brief { text: t, offers } => {
            let base = Style::default().fg(theme.speech());
            out.extend(markdown::render_prose(t, width, theme, base));
            for o in offers {
                let path = text::truncate(&text::short_path(&text::clean(&o.path)), width.saturating_sub(24).max(8));
                let mut spans = vec![
                    Span::styled(format!("{} ", chrome::DISCLOSURE_CLOSED), ink(Ink::Accent, theme)),
                    Span::styled(format!("watch {path}"), ink(Ink::Dim, theme)),
                ];
                if o.live_runs > 0 {
                    let runs = format!(" · {} live run{}", o.live_runs, if o.live_runs == 1 { "" } else { "s" });
                    spans.push(Span::styled(runs, ink(Ink::Green, theme)));
                }
                spans.push(Span::styled("  ^w", ink(Ink::Dimmer, theme)));
                out.push(Line::from(spans));
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
    // The user's words are shown as typed. Only their maths is interpreted, so a `$\alpha$` reads
    // as a formula and `**` stays two asterisks.
    let rows: Vec<Line<'static>> = if markdown::has_maths(t) {
        markdown::render_text(t.trim_end(), body, theme, style, false)
    } else {
        text::wrap(&text::clean(t.trim_end()), body)
            .into_iter()
            .map(|l| Line::from(Span::styled(l, style)))
            .collect()
    };
    for (i, l) in rows.into_iter().enumerate() {
        let lead = if i == 0 { text::pad(label, USER_LABEL) } else { " ".repeat(USER_LABEL) };
        let mut spans = vec![Span::styled(lead, ink(Ink::Dim, theme))];
        spans.extend(l.spans);
        out.push(Line::from(spans));
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
            let address = state.user.as_ref().and_then(|u| u.address.as_deref());
            let greeting = greeting(&state.clock, address);
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

/// `Good morning, sir.` etc. from the clock's hour and the address form; plain `Hello` when the
/// clock is unknown.
fn greeting(clock: &str, address: Option<&str>) -> String {
    let hour = clock.split(':').next().and_then(|h| h.trim().parse::<u32>().ok());
    let head = match hour {
        Some(5..=11) => "Good morning",
        Some(12..=17) => "Good afternoon",
        Some(18..=23) | Some(0..=4) => "Good evening",
        _ => "Hello",
    };
    match address.map(crate::text::clean).filter(|a| !a.trim().is_empty()) {
        Some(a) => format!("{head}, {}.", a.trim()),
        None => format!("{head}."),
    }
}
