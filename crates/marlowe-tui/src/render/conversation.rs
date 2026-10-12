//! The conversation pane.

use marlowe_surface::chrome::{self, Ink};
use marlowe_surface::markdown::MathBlock;
use marlowe_surface::{markdown, Theme};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::{hit, hovered, ink, lit, note_style, put};
use crate::state::{pick_thought, Conn, Entry, Focus, Mark, Pin, State, Target, ToolStatus, Viewport};
use crate::text;

/// Left padding inside the pane.
const PAD: u16 = 2;
/// Width of the `you  ` label.
const USER_LABEL: usize = 5;

pub fn draw(full: Rect, state: &State, theme: &Theme, buf: &mut Buffer) {
    if full.width < 10 || full.height == 0 {
        return;
    }
    // The pinned todo list takes the bottom of the pane while anything is open.
    let todo_h = todo_height(state, full);
    let strip_h = super::bg::strip_height(state, full);
    let r = Rect { height: full.height.saturating_sub(todo_h + strip_h), ..full };
    if strip_h > 0 {
        super::bg::draw_strip(Rect { y: r.bottom(), height: strip_h, ..full }, state, theme, buf, PAD);
    }
    if todo_h > 0 {
        draw_todos(Rect { y: r.bottom() + strip_h, height: todo_h, ..full }, state, theme, buf);
    }
    // Text column: padding on the left, one blank column and the scrollbar on the right.
    let tw = r.width.saturating_sub(PAD + 2) as usize;
    let (mut lines, maths, marks) = transcript(state, tw, theme);
    if lines.is_empty() {
        state.marks.borrow_mut().clear();
        empty_state(r, state, theme, buf);
        return;
    }
    let h = r.height as usize;
    let total = lines.len();
    let max = total.saturating_sub(h);
    // A fold toggled since the last frame: its marker's line now, and the row it was on.
    let anchor = state
        .anchor
        .take()
        .filter(|a| a.epoch == state.epoch)
        .and_then(|a| marks.iter().find(|m| m.entry == a.entry).map(|m| (m.line, a.row)));
    let start = view_top(state, (r.width, r.height), total, max, anchor);
    // Past the tail the reader can still scroll up to the first line: the range runs from here.
    let past = state.viewport.get().pin == Pin::Tail;
    state.scroll_max.set(if past { start + state.scroll } else { max });
    let scroll = max.saturating_sub(start);
    if let Some(svc) = marlowe_typeset::installed() {
        let origin = (r.x + PAD, r.y);
        typeset_images(&mut lines, &maths, svc, origin, start, h, tw, theme);
    }
    // The Conversation box's cursor: the fold marker Enter acts on.
    let keys = state.focus == Focus::Conversation && state.overlay.is_none() && state.approvals.is_empty();
    let cursor = if keys { pick_thought(&marks, start, h, state.thought) } else { None };
    for (i, l) in lines.iter().skip(start).take(h).enumerate() {
        let y = r.y + i as u16;
        match marks.iter().find(|m| m.line == start + i) {
            Some(m) => {
                let on = cursor == Some(m.entry);
                marker_row(state, buf, (r.x + PAD, y, tw as u16), l.clone(), m.entry, on, theme)
            }
            None => put(buf, r.x + PAD, y, l, tw as u16),
        }
    }
    state.marks.replace(marks);
    if total > h {
        scrollbar(r, total, h, start.min(max), max, theme, buf);
    }
    if scroll > 0 {
        let tag = Line::from(Span::styled(format!(" ↓ {scroll} more "), ink(Ink::Dim, theme)));
        let w = tag.width() as u16;
        put(buf, r.right().saturating_sub(w + 2), r.bottom() - 1, &tag, w);
    }
}

/// Lines the transcript may lose from one frame to the next while the pane following a reply
/// holds its place. A bigger loss is a real change of layout, and the view goes to the tail.
const HOLD_LINES: usize = 3;

/// A reasoning block's fold marker on screen: a click target, lit under the pointer, and with the
/// accent and Enter's keycap while the Conversation box's cursor is on it. Restyling only: the
/// marker is one row whatever its state, so nothing under it moves.
fn marker_row(
    state: &State,
    buf: &mut Buffer,
    at: (u16, u16, u16),
    mut line: Line<'static>,
    entry: usize,
    cursor: bool,
    theme: &Theme,
) {
    let (x, y, max) = at;
    if cursor {
        if let Some(glyph) = line.spans.first_mut() {
            glyph.style = ink(Ink::Accent, theme);
        }
        line.spans.push(Span::styled(format!("  {}", chrome::KEYCAP_ENTER), ink(Ink::Dim, theme)));
    }
    let t = Target::Thought(entry);
    hit(state, Rect { x, y, width: (line.width() as u16).min(max), height: 1 }, t.clone());
    let line = if hovered(state, &t) { lit(line, theme) } else { line };
    put(buf, x, y, &line, max);
}

/// The transcript line at the top of the pane, and the record of it for the next frame.
///
/// **Steady while a reply streams.** Following the tail, the view moves down with new text and
/// never back up: if the transcript got a few lines shorter mid-reply, the pane would keep its
/// place and leave the rows below empty until text fills them, rather than pull every line on
/// screen down. A larger loss (reasoning collapsed, a reflow) is followed to the new tail.
/// Scrolled up, the view stays on the lines it shows while the reply grows below them, until the
/// reader scrolls again, and through a pane that changes height (a resize, a composer grown by a
/// paste). After a width change it keeps the same fraction of the transcript above it, since
/// every line has wrapped again. `tests/stream_replay.rs` replays a streamed maths reply.
///
/// **A fold opens or closes in place.** `anchor` is a toggled marker's line and the pane row it was
/// on: the view keeps it there, so only the lines under it move. That can leave the view above the
/// tail ([`Pin::Above`], held like a scrolled-up view) or past it ([`Pin::Tail`], empty rows under
/// a fold closed near the end, until text fills them). `tests/folds.rs` replays both.
fn view_top(state: &State, size: (u16, u16), total: usize, max: usize, anchor: Option<(usize, usize)>) -> usize {
    let prev = state.viewport.get();
    let turn = state.live.as_ref().map(|l| l.turn);
    let same = prev.size == size && prev.epoch == state.epoch;
    let kept = prev.scroll == state.scroll && prev.epoch == state.epoch && prev.total > 0;
    let last = total.saturating_sub(1);
    let (top, pin) = if let Some((line, row)) = anchor {
        let top = line.saturating_sub(row);
        (top, Pin::of(top, max))
    } else if same && prev.pin == Pin::Tail {
        // Past the tail: scrolling moves from here, and once text has filled the empty rows the
        // view follows the tail again.
        let top = (prev.top + prev.scroll).saturating_sub(state.scroll).min(last);
        match Pin::of(top, max) {
            Pin::Tail => (top, Pin::Tail),
            _ if state.scroll == 0 => (max, Pin::None),
            _ => (top, Pin::None),
        }
    } else if kept && (state.scroll != 0 || prev.pin == Pin::Above) {
        // The reader's place as last drawn: `state.scroll` has gone stale while text arrived.
        let top = if prev.size.0 == size.0 { prev.top.min(max) } else { (prev.top * total / prev.total).min(max) };
        (top, if prev.pin == Pin::Above { Pin::of(top, max) } else { Pin::None })
    } else if state.scroll == 0 {
        let streaming = turn.is_some() && prev.turn == turn && prev.scroll == 0;
        let held = prev.total.saturating_sub(total) <= HOLD_LINES;
        if same && streaming && held {
            (max.max(prev.top.min(last)), Pin::None)
        } else {
            (max, Pin::None)
        }
    } else {
        (max - state.scroll.min(max), Pin::None)
    };
    state.viewport.set(Viewport { top, total, size, scroll: state.scroll, turn, epoch: state.epoch, pin });
    top
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
/// The image fills exactly the rows the grid layout reserved, so nothing moves when it lands.
/// Until it does, the block shows the held-maths placeholder over the same rows rather than the
/// grid, so a formula appears once. One Typst refuses keeps its grid rows.
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
        // The grid's rows, and the text column's width from the block's column: a display block
        // is alone on its rows, so the image may run wider than the grid did rather than shrink.
        // The cache is not keyed on this width: a resize re-renders only an image it no longer fits.
        let cols = tw.saturating_sub(b.col);
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

/// The whole transcript as styled lines wrapped to `width`, the display formulas in Marlowe's
/// replies that an image may replace, and where each foldable reasoning block's marker is.
pub(super) fn transcript(state: &State, width: usize, theme: &Theme) -> (Vec<Line<'static>>, Vec<MathBlock>, Vec<Mark>) {
    let mut maths: Vec<MathBlock> = Vec::new();
    let mut marks: Vec<Mark> = Vec::new();
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut prev: Option<&Entry> = None;
    // The entry still being written: its unclosed maths is held behind a placeholder.
    let streaming = state.live.as_ref().map(|_| state.entries.len().saturating_sub(1));
    for (idx, e) in state.entries.iter().enumerate() {
        // An expanded reasoning block ends with a blank row, so the tool call under it does not
        // read as one more line of it.
        let after_open_thought = idx > 0
            && matches!(prev, Some(Entry::Thinking { text, .. }) if !text.trim().is_empty())
            && state.thought_open(idx - 1);
        let gap = match (prev, e) {
            (None, _) => false,
            (Some(_), Entry::User { .. }) => true,
            (Some(Entry::User { .. }), _) => false,
            _ if after_open_thought => true,
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
            if matches!(e, Entry::Thinking { text, .. } if !text.trim().is_empty()) {
                marks.push(Mark { entry: idx, line: out.len() });
            }
            entry_lines(e, idx, state, width, theme, streaming == Some(idx), &mut out);
        }
        prev = Some(e);
    }
    if let Some(p) = &state.pending_user {
        if !out.is_empty() {
            out.push(Line::default());
        }
        user_lines(p, "you", ink(Ink::Dim, theme), width, theme, &mut out);
    }
    // Typed while the model works: held until it reaches a step boundary.
    for q in state.code.queue.iter().map(|q| q.text.as_str()) {
        if !out.is_empty() {
            out.push(Line::default());
        }
        let first = q.lines().next().unwrap_or("");
        let more = if q.lines().count() > 1 { " …" } else { "" };
        out.push(Line::from(vec![
            Span::styled(text::pad("queued", USER_LABEL + 1), ink(Ink::Dimmer, theme)),
            Span::styled(format!("{}{more}", text::truncate(&text::clean(first), width.saturating_sub(USER_LABEL + 3))), ink(Ink::Dim, theme)),
        ]));
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
    (out, maths, marks)
}

/// A tool call's inks: its marker's, then its call's. The accent marks every call (red one that
/// failed) and the call is the body weight, so a call never shares the reasoning block's dim
/// weight and dimmer rule, open or closed.
fn tool_ink(status: &ToolStatus) -> (Ink, Ink) {
    match status {
        ToolStatus::Failed(_) => (Ink::Red, Ink::Body),
        ToolStatus::Running | ToolStatus::Done(_) => (Ink::Accent, Ink::Body),
    }
}

pub(super) fn entry_lines(e: &Entry, idx: usize, state: &State, width: usize, theme: &Theme, live: bool, out: &mut Vec<Line<'static>>) {
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
        Entry::Thinking { text, tokens, live, ms } => {
            let n = if *tokens > 0 { *tokens } else { (text.chars().count() as u64).div_ceil(4) };
            let open = state.thought_open(idx) && !text.trim().is_empty();
            let marker = if open { chrome::DISCLOSURE_OPEN } else { chrome::DISCLOSURE_CLOSED };
            let head = if *live {
                vec![
                    Span::styled(format!("{marker} "), ink(Ink::Hover, theme)),
                    Span::styled("thinking… ", ink(Ink::Dim, theme)),
                    Span::styled(format!("{} tokens", text::thousands(n)), ink(Ink::Dimmer, theme)),
                ]
            } else {
                // How long it took, once that is known and at least a second.
                let took = if *ms >= 1000 { format!(" for {}", text::took(*ms)) } else { String::new() };
                vec![
                    Span::styled(format!("{marker} "), ink(Ink::Dimmer, theme)),
                    Span::styled(format!("thought{took} · {} tokens", text::thousands(n)), ink(Ink::Dimmer, theme)),
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
            // A tool call is an action, not reasoning: the accent's marker and the body weight,
            // where a reasoning block is the dim weight behind a dimmer rule ([`tool_ink`]).
            let marker = format!("{} ", chrome::TOOL_MARKER);
            let summary = text::clean(summary.lines().next().unwrap_or(""));
            let (marker_ink, call_ink) = tool_ink(status);
            let tail = match status {
                ToolStatus::Running => Some(("…".to_string(), ink(Ink::Dim, theme))),
                ToolStatus::Done(s) if s.trim().is_empty() => None,
                ToolStatus::Done(s) => Some((text::clean(s), ink(Ink::Dim, theme))),
                ToolStatus::Failed(s) => {
                    let s = if s.trim().is_empty() { "failed".to_string() } else { text::clean(s) };
                    Some((s, ink(Ink::Red, theme)))
                }
            };
            let tail_w = tail.as_ref().map_or(0, |(t, _)| text::width(t) + 3);
            let head = text::truncate(&summary, width.saturating_sub(2 + tail_w.min(width / 2)).max(8));
            let mut spans = vec![Span::styled(marker, ink(marker_ink, theme)), Span::styled(head, ink(call_ink, theme))];
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
            if let Entry::Tool { call_id, .. } = e {
                inline_diff(call_id, state, width, theme, out);
            }
        }
        Entry::Job { command, output, code, .. } => job_lines(command, output, *code, width, theme, out),
        Entry::News { label, text } => super::bg::news_lines(label, text, width, theme, out),
        Entry::File { file } => super::bg::file_lines(file, width, theme, out),
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

/// Rows the pinned todo list takes: none unless something is open and the pane is tall enough to
/// spare them.
fn todo_height(state: &State, r: Rect) -> u16 {
    if state.code.todos_hidden || state.code.open_todos() == 0 || r.height < 16 {
        return 0;
    }
    (state.code.todos.len() as u16 + 1).min(TODO_ROWS + 1)
}

/// Most items the strip lists (the header is one more row).
const TODO_ROWS: u16 = 6;

fn draw_todos(r: Rect, state: &State, theme: &Theme, buf: &mut Buffer) {
    use marlowe_proto::TodoStatus;
    let todos = &state.code.todos;
    let done = todos.iter().filter(|t| t.status == TodoStatus::Completed).count();
    let head = Line::from(vec![
        Span::styled("todos ", ink(Ink::Dim, theme)),
        Span::styled(format!("{done} of {}", todos.len()), ink(Ink::Dimmer, theme)),
    ]);
    put(buf, r.x + PAD, r.y, &head, r.width.saturating_sub(PAD));
    // Past the strip's height the oldest finished items go first.
    let room = r.height.saturating_sub(1) as usize;
    let skip = todos.len().saturating_sub(room).min(done);
    let shown: Vec<_> = todos.iter().enumerate().filter(|(i, t)| !(t.status == TodoStatus::Completed && *i < skip)).take(room).collect();
    for (row, (_, t)) in shown.into_iter().enumerate() {
        // An item whose blocker is not completed cannot be started (T34).
        let blockers: Vec<&str> = t
            .blocked_by
            .iter()
            .filter(|b| todos.iter().any(|o| o.id.as_deref() == Some(b.as_str()) && o.status != TodoStatus::Completed))
            .map(String::as_str)
            .collect();
        let blocked = t.status != TodoStatus::Completed && !blockers.is_empty();
        let (glyph, gink, text_ink) = match t.status {
            TodoStatus::Completed => ("✓", Ink::Green, Ink::Dimmer),
            _ if blocked => ("⊘", Ink::Amber, Ink::Dim),
            TodoStatus::InProgress => ("●", Ink::Accent, Ink::Body),
            TodoStatus::Pending => ("○", Ink::Dim, Ink::Dim),
        };
        let label = match (t.status, &t.active_form) {
            (TodoStatus::InProgress, Some(a)) if !a.trim().is_empty() => a.as_str(),
            _ => t.content.as_str(),
        };
        // `#3` for a task-list item, `@name` for who has it, `needs #1` while it waits.
        let mut tail = String::new();
        if let Some(o) = t.owner.as_deref().filter(|o| !o.trim().is_empty()) {
            tail.push_str(&format!("  @{}", text::clean(o)));
        }
        if blocked {
            tail.push_str(&format!("  needs {}", blockers.iter().map(|b| format!("#{b}")).collect::<Vec<_>>().join(", ")));
        }
        let lead = t.id.as_deref().map(|i| format!("#{i} ")).unwrap_or_default();
        let room = (r.width as usize).saturating_sub(PAD as usize + 4 + text::width(&tail) + text::width(&lead));
        let line = Line::from(vec![
            Span::styled(format!("{glyph} "), ink(gink, theme)),
            Span::styled(lead, ink(Ink::Dimmer, theme)),
            Span::styled(text::truncate(&text::clean(label), room), ink(text_ink, theme)),
            Span::styled(tail, ink(if blocked { Ink::Amber } else { Ink::Dimmer }, theme)),
        ]);
        put(buf, r.x + PAD, r.y + 1 + row as u16, &line, r.width.saturating_sub(PAD));
    }
}

/// The diff under an edit's tool line, indented, capped, with the way to the rest.
fn inline_diff(call_id: &str, state: &State, width: usize, theme: &Theme, out: &mut Vec<Line<'static>>) {
    let Some(d) = state.code.tool_detail.get(call_id).and_then(|d| d.diff.as_ref()) else { return };
    let r = crate::diff::render(d, crate::diff::Layout::inline(width.saturating_sub(4)), theme);
    for l in r.lines {
        let mut spans = vec![Span::raw("    ")];
        spans.extend(l.spans);
        out.push(Line::from(spans));
    }
    if r.hidden > 0 {
        out.push(Line::from(Span::styled(format!("    … {} more rows · ^o shows all", r.hidden), ink(Ink::Dimmer, theme))));
    }
}

/// How many output rows a job shows under its command before the rest is for ^o.
const JOB_ROWS: usize = 8;

fn job_lines(command: &str, output: &str, code: Option<i32>, width: usize, theme: &Theme, out: &mut Vec<Line<'static>>) {
    let (word, ik) = match code {
        None => ("running".to_string(), Ink::Green),
        Some(0) => ("done".to_string(), Ink::Dim),
        Some(c) => (format!("exit {c}"), Ink::Red),
    };
    let head = text::truncate(&text::clean(command), width.saturating_sub(14).max(8));
    out.push(Line::from(vec![
        Span::styled(format!("{} ", chrome::TOOL_MARKER), ink(ik, theme)),
        Span::styled("$ ", ink(Ink::Dimmer, theme)),
        Span::styled(head, theme.normal()),
        Span::styled(" · ", ink(Ink::Dimmer, theme)),
        Span::styled(word, ink(ik, theme)),
    ]));
    let lines: Vec<&str> = output.lines().collect();
    let skip = lines.len().saturating_sub(JOB_ROWS);
    if skip > 0 {
        out.push(Line::from(Span::styled(format!("  {} {skip} earlier rows", chrome::QUOTE_RULE), ink(Ink::Dimmer, theme))));
    }
    for l in lines.into_iter().skip(skip) {
        out.push(Line::from(vec![
            Span::styled(format!("  {} ", chrome::QUOTE_RULE), ink(Ink::Dimmer, theme)),
            Span::styled(text::truncate(&text::clean(l), width.saturating_sub(4)), ink(Ink::Dim, theme)),
        ]));
    }
}

pub(super) fn user_lines(t: &str, label: &str, style: Style, width: usize, theme: &Theme, out: &mut Vec<Line<'static>>) {
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
