//! The conversation pane.

use marlowe_surface::chrome::{self, Ink};
use marlowe_surface::markdown::MathBlock;
use marlowe_surface::{markdown, Theme};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::work::{self, Role};
use super::{hit, hovered, ink, lit, note_style, put};
use crate::layout_cache::TranscriptCache;
use crate::state::{pick_thought, Conn, Entry, Focus, Mark, Pin, RowHit, State, Target, Viewport};
use crate::text;

/// Left padding inside the pane.
const PAD: u16 = 2;
/// Width of the `you  ` label.
const USER_LABEL: usize = 5;
/// The label column of an in-transcript sequence line (`Semantic Scholar ····`).
const SEQ_LABEL_W: usize = 22;

pub fn draw(full: Rect, state: &State, theme: &Theme, buf: &mut Buffer) {
    if full.width < 10 || full.height == 0 {
        return;
    }
    // The pinned todo list takes the bottom of the pane while anything is open.
    let todo_h = todo_height(state, full);
    let strip_h = super::bg::strip_height(state, full);
    let live_h = super::strip::height(state, full);
    let r = Rect { height: full.height.saturating_sub(todo_h + strip_h + live_h), ..full };
    if strip_h > 0 {
        super::bg::draw_strip(Rect { y: r.bottom() + live_h, height: strip_h, ..full }, state, theme, buf, PAD);
    }
    if todo_h > 0 {
        draw_todos(Rect { y: r.bottom() + live_h + strip_h, height: todo_h, ..full }, state, theme, buf);
    }
    // Text column: padding on the left, one blank column and the scrollbar on the right.
    let tw = r.width.saturating_sub(PAD + 2) as usize;
    // The entries' lines come from the layout cache (only what changed is laid out again); what
    // follows them (the pending message, the queue, the wait) is a few lines, built each frame.
    let mut cache = state.layout.borrow_mut();
    sync(&mut cache, state, tw, theme);
    state.projects.pane_rows.set(r.height);
    let (tail, tail_blocks) = tail_lines(state, tw, theme, cache.lines.is_empty());
    let base = cache.lines.len();
    let total = base + tail.len();
    // The live strip sits directly under the output: at the pane's bottom once the text fills it,
    // right below the last line before that. Its rows are reserved either way.
    let under = |rows: usize| Rect { y: r.y + rows.min(r.height as usize) as u16, height: live_h, ..full };
    if total == 0 {
        state.marks.borrow_mut().clear();
        empty_state(r, state, theme, buf);
        if live_h > 0 {
            super::strip::draw(under(0), state, theme, buf, PAD);
        }
        return;
    }
    let h = r.height as usize;
    let max = total.saturating_sub(h);
    // A fold toggled since the last frame: its marker's line now, and the row it was on.
    let anchor = state
        .anchor
        .take()
        .filter(|a| a.epoch == state.epoch)
        .and_then(|a| cache.marks.iter().find(|m| m.entry == a.entry).map(|m| (m.line, a.row)));
    let start = view_top(state, (r.width, r.height), live_h, total, max, anchor);
    // Past the tail the reader can still scroll up to the first line: the range runs from here.
    let past = state.viewport.get().pin == Pin::Tail;
    state.scroll_max.set(if past { start + state.scroll } else { max });
    let scroll = max.saturating_sub(start);
    // Only the rows on screen are copied out of the cache.
    let mut window: Vec<Line<'static>> = (start.min(total)..(start + h).min(total))
        .map(|i| if i < base { cache.lines[i].clone() } else { tail[i - base].clone() })
        .collect();
    if let Some(svc) = marlowe_typeset::installed() {
        let origin = (r.x + PAD, r.y);
        typeset_images(&mut window, &cache.maths, svc, (origin, start, total), h, tw, theme);
    }
    // The Conversation box's cursor: the fold marker Enter acts on.
    let keys = state.focus == Focus::Conversation && state.overlay.is_none() && state.approvals.is_empty();
    let marks = cache.marks.clone();
    // The click targets of the rows on screen.
    let hits = &cache.hits;
    let on_screen: Vec<RowHit> = hits[hits.partition_point(|x| x.line < start)..hits.partition_point(|x| x.line < start + h)].to_vec();
    // A project's result in the transcript: where its rows end, for the click map.
    let results: Vec<(String, usize)> = state
        .entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| match e {
            Entry::Project { id } => cache.entry_end(i).map(|end| (id.clone(), end)),
            _ => None,
        })
        .collect();
    drop(cache);
    let cursor = if keys { pick_thought(&marks, start, h, state.thought) } else { None };
    for (i, l) in window.iter().enumerate() {
        let y = r.y + i as u16;
        // Marks are in line order.
        let at = marks.partition_point(|m| m.line < start + i);
        match marks.get(at).filter(|m| m.line == start + i) {
            // A sent file's row is not a fold: its name is a click target of its own.
            Some(m) if matches!(state.entries.get(m.entry), Some(Entry::File { .. })) => {
                let line = if cursor == Some(m.entry) { work::file_cursor(l.clone(), theme) } else { l.clone() };
                put(buf, r.x + PAD, y, &line, tw as u16)
            }
            Some(m) => {
                let on = cursor == Some(m.entry);
                marker_row(state, buf, (r.x + PAD, y, tw as u16), l.clone(), m.entry, on, theme)
            }
            None => put(buf, r.x + PAD, y, l, tw as u16),
        }
    }
    for h in on_screen {
        let rect = Rect { x: r.x + PAD + h.x, y: r.y + (h.line - start) as u16, width: h.w.min((tw as u16).saturating_sub(h.x)), height: 1 };
        if hovered(state, &h.target) {
            light(buf, rect, theme);
        }
        hit(state, rect, h.target);
    }
    state.marks.replace(marks);
    let pane = Rect { x: r.x + PAD, width: tw as u16, ..r };
    for (id, end) in results {
        super::projects_entry::entry_hits(state, &id, theme, pane, tw, end, start);
    }
    for (at, rows) in &tail_blocks {
        super::projects_entry::register_hits(state, rows, (base + at) as i64, start, pane);
    }
    if live_h > 0 {
        super::strip::draw(under(total), state, theme, buf, PAD);
    }
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

/// The cells of a click target under the pointer, one weight up: the accent goes to the hover
/// colour, and the dim weights as [`lit`] has them. Never a fill.
fn light(buf: &mut Buffer, r: Rect, theme: &Theme) {
    let (dim, dimmer, accent) = (theme.dim_color(), theme.dimmer_color(), ink(Ink::Accent, theme).fg);
    let hover = ink(Ink::Hover, theme).fg;
    for x in r.x..r.right() {
        let Some(c) = buf.cell_mut((x, r.y)) else { continue };
        c.fg = match c.fg {
            f if f == Color::Reset => Color::Reset,
            f if f == dimmer => dim,
            f if f == dim => Color::Reset,
            f if Some(f) == accent => hover.unwrap_or(f),
            f => f,
        };
        c.modifier.insert(Modifier::BOLD);
    }
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
fn view_top(state: &State, size: (u16, u16), strip: u16, total: usize, max: usize, anchor: Option<(usize, usize)>) -> usize {
    let prev = state.viewport.get();
    let turn = state.live.as_ref().map(|l| l.turn);
    // The live strip coming or going changes the pane's height by its own rows and nothing else.
    let same_size = prev.size == size || (prev.size.0 == size.0 && prev.size.1 + prev.strip == size.1 + strip);
    let same = same_size && prev.epoch == state.epoch;
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
        // The strip going away widens the pane: the text stays where it is, with the freed rows
        // empty under it until the next message fills them, rather than the reply sliding down.
        let released = same && strip < prev.strip && prev.pin == Pin::None && prev.total <= total;
        if released {
            let top = max.max(prev.top.min(last));
            (top, Pin::of(top, max))
        } else if same && streaming && held {
            (max.max(prev.top.min(last)), Pin::None)
        } else {
            (max, Pin::None)
        }
    } else {
        (max - state.scroll.min(max), Pin::None)
    };
    state.viewport.set(Viewport { top, total, size, strip, scroll: state.scroll, turn, epoch: state.epoch, pin });
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
/// `window` is the pane's rows; `at` is the pane's origin, the transcript line of its first row and
/// the transcript's length. The image fills exactly the rows the grid layout reserved, so nothing
/// moves when it lands. Until it does, the block shows the held-maths placeholder over the same
/// rows rather than the grid, so a formula appears once. One Typst refuses keeps its grid rows.
fn typeset_images(
    window: &mut [Line<'static>],
    maths: &[MathBlock],
    svc: &marlowe_typeset::Service,
    at: ((u16, u16), usize, usize),
    h: usize,
    tw: usize,
    theme: &Theme,
) {
    use marlowe_typeset::{Job, Slot};
    let (origin, start, total) = at;
    // The window's row for a transcript line, when it is on screen.
    let rows = window.len();
    let row = move |line: usize| line.checked_sub(start).filter(|r| *r < rows);
    for b in maths {
        let Some(markup) = &b.formula.typst else { continue };
        // The grid's colour: an explicit one, or the terminal's own foreground for the default.
        let fg = match b.fg {
            ratatui::style::Color::Rgb(r, g, bl) => [r, g, bl],
            ratatui::style::Color::Reset => svc.caps().fg,
            _ => continue,
        };
        let end = b.line + b.rows;
        if end <= start || b.line >= start + h || end > total {
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
                for line in b.line..end {
                    let Some(r) = row(line) else { continue };
                    window[r] = if line == b.line {
                        Line::from(vec![
                            Span::raw(" ".repeat(b.col)),
                            Span::styled(markdown::MATHS_PLACEHOLDER, Style::default().fg(theme.speech()).add_modifier(Modifier::DIM)),
                        ])
                    } else {
                        Line::default()
                    };
                }
            }
            Slot::Ready(img) => {
                for k in 0..b.rows {
                    let Some(r) = row(b.line + k) else { continue };
                    window[r] = Line::default();
                    let strip = &img.strips[k];
                    if !strip.is_empty() {
                        crate::images::place(crate::images::Placed {
                            x: origin.0 + b.col as u16,
                            y: origin.1 + r as u16,
                            cols: img.cols.min(cols as u16),
                            strip: std::sync::Arc::clone(strip),
                        });
                    }
                }
            }
        }
    }
}

/// Whether entry `e` draws no row at all: a member of a work block with no edit to show, and the
/// relay of a finished project whose own entry is on screen.
fn silent(state: &State, idx: usize, e: &Entry, role: &Role) -> bool {
    match e {
        Entry::News { label, .. } => super::projects_entry::completion_shown(state, idx, label),
        _ => work::silent(state, e, role),
    }
}

/// Whether a blank row goes before entry `e`. `prev` is the last entry that drew anything.
fn gap_before(prev: Option<&Entry>, e: &Entry, role: &Role) -> bool {
    // An edit's row sits under the block it came from.
    if *role == Role::Member {
        return false;
    }
    match (prev, e) {
        (None, _) => false,
        (Some(_), Entry::User { .. }) => true,
        // A project's result stands apart from the lines around it, except from its own relay label.
        (Some(Entry::News { label, .. }), Entry::Project { id }) => !label.contains(id.as_str()),
        (Some(_), Entry::Project { .. }) | (Some(Entry::Project { .. }), _) => true,
        (Some(Entry::User { .. }), _) => false,
        (Some(Entry::Tool { .. } | Entry::Thinking { .. } | Entry::File { .. }), Entry::Prose { .. }) => true,
        (Some(Entry::Prose { .. }), Entry::Tool { .. } | Entry::Thinking { .. } | Entry::File { .. }) => true,
        _ => false,
    }
}

/// One entry's rows (its gap first), with the display formulas, fold markers and click targets in
/// them. `live` is the entry still being written: its unclosed maths is held behind a placeholder.
/// `block_live` is whether the work block this entry draws belongs to the turn in flight.
#[allow(clippy::too_many_arguments)]
fn render_entry(
    state: &State,
    idx: usize,
    e: &Entry,
    role: &Role,
    gap: bool,
    (live, block_live): (bool, bool),
    width: usize,
    theme: &Theme,
    out: &mut Vec<Line<'static>>,
    maths: &mut Vec<MathBlock>,
    marks: &mut Vec<Mark>,
    hits: &mut Vec<RowHit>,
) {
    if gap {
        out.push(Line::default());
    }
    match e {
        // A short line before a work block is that block's title ([`work::preamble`]).
        Entry::Prose { .. } if *role == Role::Title => {}
        Entry::Prose { text } => {
            // Marlowe's prose, with the position of each display formula kept.
            let base = Style::default().fg(theme.speech());
            let (lines, found) = markdown::render_prose_blocks(text, width, theme, base, live);
            let at = out.len();
            maths.extend(found.into_iter().map(|mut b| {
                b.line += at;
                b
            }));
            out.extend(lines);
        }
        // The turn's reasoning and calls are the block's; an edit's row stands outside it.
        Entry::Thinking { .. } | Entry::Tool { .. } => {
            if let Role::First(members) = role {
                work::block_lines(state, idx, members, block_live, width, theme, out, marks, hits);
            }
            if let Entry::Tool { call_id, .. } = e {
                work::edit_lines(state, call_id, width, theme, out, hits);
            }
        }
        Entry::File { file } => work::file_line(idx, file, width, theme, out, marks, hits),
        _ => entry_lines(e, idx, state, width, theme, live, out),
    }
}

/// The rows after the entries: the projects that run, the message just sent, what is queued
/// behind the turn, and the wait for the first word. `empty` is whether nothing is above them.
/// With the rows go the projects' blocks (where each starts in the tail, and its rows), whose
/// click targets the caller records.
fn tail_lines(state: &State, width: usize, theme: &Theme, empty: bool) -> (Vec<Line<'static>>, Vec<(usize, Vec<super::projects::BLine>)>) {
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut blocks = Vec::new();
    let gap = |out: &mut Vec<Line<'static>>| {
        if !(empty && out.is_empty()) {
            out.push(Line::default());
        }
    };
    // A running project is the last thing in the transcript, above the live strip.
    for (id, rows) in super::projects_entry::live_blocks(state, width, theme) {
        let own_relay = out.is_empty() && matches!(state.entries.last(), Some(Entry::News { label, .. }) if label.contains(id.as_str()));
        if !own_relay {
            gap(&mut out);
        }
        blocks.push((out.len(), rows.clone()));
        out.extend(rows.into_iter().map(|b| b.line));
    }
    if let Some(p) = &state.pending_user {
        gap(&mut out);
        user_lines(p, "you", ink(Ink::Dim, theme), width, theme, &mut out);
    }
    // Typed while the model works: held until it reaches a step boundary.
    for q in state.code.queue.iter().map(|q| q.text.as_str()) {
        gap(&mut out);
        let first = q.lines().next().unwrap_or("");
        let more = if q.lines().count() > 1 { " …" } else { "" };
        out.push(Line::from(vec![
            Span::styled(text::pad("queued", USER_LABEL + 1), ink(Ink::Dimmer, theme)),
            Span::styled(format!("{}{more}", text::truncate(&text::clean(first), width.saturating_sub(USER_LABEL + 3))), ink(Ink::Dim, theme)),
        ]));
    }
    // Nothing has streamed yet: the live strip under the pane says what the wait is.
    (out, blocks)
}

/// Brings the cache up to date with the entries. Every entry whose key is unchanged keeps its
/// rows; from the first one that changed, the rest are laid out again (their positions moved).
/// Streaming changes the last entry, so a streaming frame lays out one entry.
fn sync(cache: &mut TranscriptCache, state: &State, width: usize, theme: &Theme) {
    cache.reset_unless(width, theme, state.no_layout_cache);
    let streaming = state.live.as_ref().map(|_| state.entries.len().saturating_sub(1));
    let (roles, open_block) = work::roles(&state.entries);
    let mut prev: Option<&Entry> = None;
    let mut same = true;
    let mut laid = 0;
    for (idx, e) in state.entries.iter().enumerate() {
        let role = &roles[idx];
        let quiet = silent(state, idx, e, role);
        let gap = !quiet && gap_before(prev, e, role);
        let live = streaming == Some(idx);
        let block_live = state.live.is_some() && open_block == Some(idx);
        let key = entry_key(state, idx, e, role, gap, (live, block_live));
        if !quiet {
            prev = Some(e);
        }
        if same && cache.key_at(idx) == Some(key) {
            continue;
        }
        if same {
            same = false;
            cache.cut(idx);
        }
        let (lines, maths, marks, hits) = cache.parts();
        render_entry(state, idx, e, role, gap, (live, block_live), width, theme, lines, maths, marks, hits);
        cache.seal(key);
        laid += 1;
    }
    if same {
        cache.cut(state.entries.len());
    }
    cache.laid = laid;
}

/// What an entry's rows depend on besides the width and the theme (a change of either clears the
/// cache): its content, its place (fold state, the blank row before it, whether it is still being
/// written) and an edit's diff.
fn entry_key(state: &State, idx: usize, e: &Entry, role: &Role, gap: bool, (live, block_live): (bool, bool)) -> u64 {
    let mut h = crate::layout_cache::Hasher::default();
    h.entry(e);
    h.u64(u64::from(gap) | u64::from(live) << 1 | u64::from(state.show_thinking) << 3);
    h.u64(idx as u64);
    h.u64(u64::from(*role == Role::Title));
    // A block is laid out again whenever one of its entries, or the clock it shows, changes.
    if let Role::First(members) = role {
        work::key(state, idx, members, block_live, &mut h);
    }
    if let Entry::Project { id } = e {
        // The project's state, and what the pointer and the keys do to its rows.
        h.u64(state.projects.version_of(id));
        h.str(&format!("{:?}", (&state.hover, state.projects.focus.as_deref() == Some(id.as_str()) && state.focus == crate::state::Focus::Conversation, state.projects.card_sel)));
    }
    if let Entry::News { label, .. } = e {
        h.u64(u64::from(super::projects_entry::relay_label_only(state, idx, label)));
    }
    if let Entry::Tool { call_id, .. } = e {
        if state.code.tool_detail.get(call_id).is_some_and(|d| d.diff.is_some()) {
            h.u64(state.code.detail_gen);
        }
    }
    h.finish()
}

/// The whole transcript laid out from scratch: the reference the cache must equal.
#[cfg(test)]
fn transcript(state: &State, width: usize, theme: &Theme) -> (Vec<Line<'static>>, Vec<MathBlock>, Vec<Mark>, Vec<RowHit>) {
    let mut maths: Vec<MathBlock> = Vec::new();
    let mut marks: Vec<Mark> = Vec::new();
    let mut hits: Vec<RowHit> = Vec::new();
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut prev: Option<&Entry> = None;
    let streaming = state.live.as_ref().map(|_| state.entries.len().saturating_sub(1));
    let (roles, open_block) = work::roles(&state.entries);
    for (idx, e) in state.entries.iter().enumerate() {
        let role = &roles[idx];
        let quiet = silent(state, idx, e, role);
        let gap = !quiet && gap_before(prev, e, role);
        let block_live = state.live.is_some() && open_block == Some(idx);
        render_entry(state, idx, e, role, gap, (streaming == Some(idx), block_live), width, theme, &mut out, &mut maths, &mut marks, &mut hits);
        if !quiet {
            prev = Some(e);
        }
    }
    let (tail, _) = tail_lines(state, width, theme, out.is_empty());
    out.extend(tail);
    (out, maths, marks, hits)
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
        // The turn's reasoning and calls are drawn by their work block ([`work`]).
        Entry::Thinking { .. } | Entry::Tool { .. } => {}
        Entry::Job { command, output, code, .. } => job_lines(command, output, *code, width, theme, out),
        // The finished project's own entry says it; this relay draws nothing ([`silent`]).
        Entry::News { label, .. } if super::projects_entry::completion_shown(state, idx, label) => {}
        Entry::News { label, .. } if super::projects_entry::relay_label_only(state, idx, label) => {
            out.push(Line::from(vec![
                Span::styled("↳ ", ink(Ink::Dimmer, theme)),
                Span::styled(text::truncate(&text::clean(label), width.saturating_sub(4).max(8)), ink(Ink::Dim, theme)),
            ]));
        }
        Entry::News { label, text } => super::bg::news_lines(label, text, width, theme, out),
        Entry::Project { id } => super::projects_entry::entry_lines(state, id, width, theme, out),
        Entry::File { file } => work::file_line(idx, file, width, theme, out, &mut Vec::new(), &mut Vec::new()),
        Entry::Note { text: t, tone } => {
            for (i, l) in text::wrap(&text::clean(t), width.saturating_sub(2)).into_iter().enumerate() {
                let lead = if i == 0 { "· " } else { "  " };
                out.push(Line::from(vec![
                    Span::styled(lead, ink(Ink::Dimmer, theme)),
                    Span::styled(l, note_style(*tone, theme)),
                ]));
            }
        }
        Entry::Recall { count, summary, timing: Some(t) } => recall_timed(*count, summary, t, state.show_thinking, width, theme, out),
        Entry::Seq(b) => {
            for l in &b.lines {
                out.push(super::seq::line(l, SEQ_LABEL_W, width.saturating_sub(SEQ_LABEL_W + 4), theme));
            }
        }
        Entry::Timings(t) => timings_lines(t, state.show_thinking, width, theme, out),
        Entry::Recall { count, summary, timing: None } => {
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
        Entry::Usage { usage, memory } => {
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
            parts.extend(memory.clone());
            let s = parts.join(" · ");
            let pad = width.saturating_sub(text::width(&s));
            out.push(Line::from(vec![
                Span::raw(" ".repeat(pad)),
                Span::styled(s, ink(Ink::Dimmer, theme)),
            ]));
        }
    }
}

/// The recall line with its stage timings: `⋯ recall · 3 memories · 84ms  embed 12 · bm25 2 · …`,
/// what was recalled after it as room allows. With reasoning open (^t) each stage has a row.
fn recall_timed(count: u32, summary: &str, t: &crate::state::RecallTiming, open: bool, width: usize, theme: &Theme, out: &mut Vec<Line<'static>>) {
    let n = format!("{count} memor{}", if count == 1 { "y" } else { "ies" });
    let amber = |s: String| Span::styled(s, ink(Ink::Amber, theme));
    let dim = |s: String| Span::styled(s, ink(Ink::Dim, theme));
    let mut spans = vec![Span::styled("⋯ ", ink(Ink::Dimmer, theme)), dim(format!("recall · {n}"))];
    if t.abstained && count == 0 {
        spans.push(Span::styled(" · abstained", ink(Ink::Dimmer, theme)));
    }
    spans.push(Span::styled(" · ", ink(Ink::Dimmer, theme)));
    spans.push(amber(format!("{}ms", t.total_ms)));
    let used = |v: &[Span<'static>]| v.iter().map(|s| text::width(&s.content)).sum::<usize>();
    let stage_spans = |abbrev: bool| -> Vec<Span<'static>> {
        let mut v = vec![Span::raw("  ")];
        for (i, s) in t.stages.iter().filter(|s| s.name != "ingest").enumerate() {
            if i > 0 {
                v.push(Span::styled(if abbrev { " " } else { " · " }, ink(Ink::Dimmer, theme)));
            }
            let name = if abbrev { s.name.chars().take(1).collect::<String>() } else { format!("{} ", s.name) };
            v.push(dim(name));
            v.push(amber(s.ms.to_string()));
        }
        v
    };
    if !t.stages.is_empty() {
        for abbrev in [false, true] {
            let v = stage_spans(abbrev);
            if used(&spans) + used(&v) <= width {
                spans.extend(v);
                break;
            }
        }
    }
    let room = width.saturating_sub(used(&spans) + 3);
    if !summary.trim().is_empty() && room > 8 {
        let first = summary.lines().next().unwrap_or("");
        spans.push(Span::styled(format!(" · {}", text::truncate(first, room)), ink(Ink::Dimmer, theme)));
    }
    out.push(Line::from(spans));
    if open {
        for s in &t.stages {
            let mut v = vec![Span::raw("    "), dim(text::pad(&s.name, 10)), amber(format!("{}ms", s.ms))];
            if let Some(n) = s.n {
                v.push(Span::styled(format!(" · {}", text::thousands(n)), ink(Ink::Dimmer, theme)));
            }
            out.push(Line::from(v));
        }
    }
}

/// A turn's measured phases, one dim row (two with the overhead numbers), only with reasoning
/// open: `recall 180ms · prompt 12ms · prefill 1.9s (8.2k) · first token 6.2s`.
fn timings_lines(t: &crate::state::Timings, open: bool, width: usize, theme: &Theme, out: &mut Vec<Line<'static>>) {
    use marlowe_proto::PhaseKind as K;
    if !open {
        return;
    }
    let amber = |s: String| Span::styled(s, ink(Ink::Amber, theme));
    let dim = |s: &str| Span::styled(s.to_string(), ink(Ink::Dim, theme));
    let sep = || Span::styled(" · ", ink(Ink::Dimmer, theme));
    let row = |parts: Vec<(String, String, Option<String>)>| -> Option<Line<'static>> {
        if parts.is_empty() {
            return None;
        }
        let mut spans = vec![Span::raw("  ")];
        for (i, (name, value, extra)) in parts.into_iter().enumerate() {
            if i > 0 {
                spans.push(sep());
            }
            spans.push(dim(&format!("{name} ")));
            spans.push(amber(value));
            if let Some(x) = extra {
                spans.push(dim(&format!(" ({x})")));
            }
        }
        // One row at any width: what does not fit is cut, not wrapped.
        let mut acc = 0;
        let mut kept = Vec::new();
        for sp in spans {
            let sw = text::width(&sp.content);
            if acc + sw > width.saturating_sub(1) {
                kept.push(Span::styled("…", ink(Ink::Dimmer, theme)));
                break;
            }
            acc += sw;
            kept.push(sp);
        }
        Some(Line::from(kept))
    };
    let mut ordered: Vec<&marlowe_proto::PhaseSpan> = t.spans.iter().filter(|s| s.stage.is_none() || s.kind != K::Recall).collect();
    ordered.sort_by_key(|s| s.start_ms);
    let mut phases = Vec::new();
    for s in ordered {
        let name = match (s.kind, s.stage.as_deref()) {
            (K::Sending, _) => "sending",
            (K::Setup, _) => "setup",
            (K::Admit, Some("gpu")) => "queued",
            (K::Admit, Some("ollama")) => "ollama start",
            (K::Admit, _) => "admit",
            (K::Prompt, _) => "prompt",
            (K::Recall, _) => "recall",
            (K::Fit, _) => "fit",
            (K::Compact, _) => "compact",
            (K::Wire, Some(_)) => "prepare",
            (K::Load, _) => "load",
            (K::Prefill, _) => "prefill",
            _ => continue,
        };
        let extra = if s.kind == K::Prefill { s.n.map(text::compact) } else { None };
        phases.push((name.to_string(), super::strip::took(s.ms), extra));
    }
    if let Some(ms) = t.numbers.ttft_ms {
        phases.push(("first token".to_string(), super::strip::took(ms), None));
    }
    out.extend(row(phases));
    // The harness's own share: Enter to the request on the wire, and a token to the screen (the
    // daemon's leg is measured, the TUI's leg is this machine's clock, so the sum is approximate).
    let mut over = Vec::new();
    if let Some(ms) = t.numbers.enter_to_wire_ms {
        over.push(("enter→wire".to_string(), super::strip::took(ms), None));
    }
    if let Some(bus) = t.numbers.token_to_bus_ms {
        let screen = bus + t.screen_ms.unwrap_or(0);
        over.push(("token→screen".to_string(), format!("~{}", super::strip::took(screen)), None));
    }
    out.extend(row(over));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{LiveTurn, NoteTone, ToolStatus};

    fn theme() -> Theme {
        Theme::default_truecolor()
    }

    fn live_turn() -> LiveTurn {
        LiveTurn {
            turn: 1,
            model: marlowe_model::ModelRef::new(marlowe_model::ProviderKind::Ollama, "m"),
            started_ms: 0,
        }
    }

    /// A small deterministic generator, so a failure replays.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self, n: usize) -> usize {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 33) as usize) % n
        }
    }

    const PROSE: [&str; 6] = [
        "A plain sentence that is long enough to wrap at narrow widths, with `inline code` in it.",
        "## A heading\n\nThen a list:\n\n- one\n- two with $x^2$ inline\n- three",
        "Display maths:\n\n$$\n\\frac{\\partial s_i}{\\partial z_j} = s_i(\\delta_{ij} - s_j)\n$$\n\nafter it.",
        "```rust\nfn main() {\n    println!(\"hi\");\n}\n```",
        "> a quote\n\n| a | b |\n|---|---|\n| 1 | 2 |",
        "An unclosed display: $$\\begin{pmatrix} a & b \\\\",
    ];

    fn entry(r: &mut Lcg, n: usize) -> Entry {
        match r.next(12) {
            0 => Entry::User { text: format!("question {n} about the thing"), steer: r.next(5) == 0 },
            1 | 2 => Entry::Prose { text: PROSE[r.next(PROSE.len())].to_string() },
            3 => Entry::Thinking { text: format!("considering option {n}\nand then $\\alpha_{n}$"), tokens: 40, live: false, ms: 1200, read: None },
            4 => Entry::Tool { call_id: format!("c{n}"), summary: format!("bash cargo test {n}"), status: ToolStatus::Done("ok".into()) },
            5 => Entry::Note { text: format!("a note {n}"), tone: NoteTone::Amber },
            6 => Entry::Recall { count: 3, summary: "valiance - eval".into(), timing: None },
            7 => Entry::Job { id: "j".into(), command: "cargo build".into(), output: "a\nb\nc".into(), code: Some(0) },
            8 => Entry::Thinking { text: format!("weighing {n}"), tokens: 312, live: r.next(2) == 0, ms: 4_100, read: Some(read()) },
            9 => Entry::Recall { count: 3, summary: "valiance - eval".into(), timing: Some(Box::new(timing())) },
            10 => Entry::Timings(Box::new(crate::state::Timings {
                spans: vec![marlowe_proto::PhaseSpan {
                    kind: marlowe_proto::PhaseKind::Recall,
                    stage: None,
                    start_ms: 1,
                    ms: 84,
                    n: None,
                    detail: None,
                    parent: None,
                    est: false,
                }],
                numbers: marlowe_proto::TurnNumbers { ttft_ms: Some(2_100), enter_to_wire_ms: Some(5), ..Default::default() },
                screen_ms: Some(1),
            })),
            _ => Entry::News { label: "monitor".into(), text: format!("line {n}") },
        }
    }

    fn read() -> crate::live::Read {
        crate::live::Read { new: 8_200, cached: 0, ms: Some(1_900), ..Default::default() }
    }

    fn timing() -> crate::state::RecallTiming {
        let stage = |name: &str, ms| marlowe_proto::StageMs { name: name.into(), ms, n: None };
        crate::state::RecallTiming {
            total_ms: 84,
            stages: vec![stage("embed", 12), stage("bm25", 2), stage("dense", 4), stage("gate", 1), stage("rerank", 61)],
            abstained: false,
        }
    }

    fn text_of(l: &Line<'_>) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn the_recall_line_abbreviates_its_stages_by_width() {
        let th = theme();
        let at = |width| {
            let mut out = Vec::new();
            recall_timed(3, "valiance - eval", &timing(), false, width, &th, &mut out);
            assert_eq!(out.len(), 1, "one row");
            let t = text_of(&out[0]);
            assert!(text::width(&t) <= width, "{t:?} is wider than {width}");
            t
        };
        assert!(at(100).contains("84ms  embed 12 · bm25 2 · dense 4 · gate 1 · rerank 61"), "{}", at(100));
        assert!(at(56).contains("84ms  e12 b2 d4 g1 r61"), "{}", at(56));
        assert!(at(30).starts_with("⋯ recall · 3 memories · 84ms"), "{}", at(30));
        // Opened with ^t: a row for each stage.
        let mut out = Vec::new();
        recall_timed(3, "", &timing(), true, 80, &th, &mut out);
        assert_eq!(out.len(), 1 + 5);
    }

    /// The cache brought up to date, compared with the transcript laid out from scratch.
    fn same_as_scratch(cache: &mut TranscriptCache, state: &State, width: usize, th: &Theme) {
        sync(cache, state, width, th);
        let (want, maths, marks, hits) = transcript(state, width, th);
        let mut got = cache.lines.clone();
        got.extend(tail_lines(state, width, th, cache.lines.is_empty()).0);
        assert_eq!(got.len(), want.len(), "row count at width {width}");
        assert!(got == want, "rows differ at width {width}");
        assert_eq!(cache.marks, marks, "fold markers differ at width {width}");
        assert_eq!(cache.hits, hits, "click targets differ at width {width}");
        assert_eq!(cache.maths.len(), maths.len());
        for (a, b) in cache.maths.iter().zip(&maths) {
            assert_eq!((a.line, a.col, a.rows, a.cols, a.fg), (b.line, b.col, b.rows, b.cols, b.fg));
        }
    }

    #[test]
    fn the_cache_equals_laying_everything_out_again_through_random_sessions() {
        let th = theme();
        for seed in 0..12u64 {
            let mut r = Lcg(seed + 1);
            let mut state = State::new(None);
            let mut cache = TranscriptCache::default();
            let mut width = 60;
            for step in 0..60 {
                match r.next(8) {
                    0 | 1 => {
                        let e = entry(&mut r, step);
                        state.entries.push(e);
                    }
                    2 => match state.entries.last_mut() {
                        // Streaming: the last prose grows.
                        Some(Entry::Prose { text }) => text.push_str(" and more words arrive"),
                        _ => state.entries.push(Entry::Prose { text: "fresh".into() }),
                    },
                    3 => {
                        let len = state.entries.len();
                        if len > 0 {
                            let i = r.next(len);
                            let open = state.thought_open(i);
                            state.folds.insert(i, !open);
                        }
                    }
                    4 => width = [30, 44, 60, 80, 120][r.next(5)],
                    5 => state.show_thinking = !state.show_thinking,
                    6 => state.live = if state.live.is_some() { None } else { Some(live_turn()) },
                    _ => {
                        if let Some(Entry::Tool { status, .. }) = state.entries.iter_mut().rev().find(|e| matches!(e, Entry::Tool { .. })) {
                            *status = ToolStatus::Failed("exit 1".into());
                        }
                        if r.next(4) == 0 {
                            state.pending_user = Some("typed while waiting".into());
                        }
                    }
                }
                same_as_scratch(&mut cache, &state, width, &th);
            }
        }
    }

    fn session(n: usize) -> State {
        let mut state = State::new(None);
        for i in 0..n {
            state.entries.push(Entry::User { text: format!("question {i}"), steer: false });
            state.entries.push(Entry::Thinking { text: "hmm".into(), tokens: 3, live: false, ms: 0, read: None });
            state.entries.push(Entry::Prose { text: PROSE[i % PROSE.len()].to_string() });
        }
        state
    }

    fn laid(cache: &mut TranscriptCache, state: &State, width: usize, th: &Theme) -> usize {
        sync(cache, state, width, th);
        cache.laid
    }

    #[test]
    fn a_streaming_frame_lays_out_one_entry_and_a_still_frame_none() {
        let th = theme();
        let mut state = session(30);
        let mut cache = TranscriptCache::default();
        assert_eq!(laid(&mut cache, &state, 80, &th), 90, "the first frame lays out everything");
        assert_eq!(laid(&mut cache, &state, 80, &th), 0, "a still frame reuses everything");
        if let Some(Entry::Prose { text }) = state.entries.last_mut() {
            text.push_str(" streamed words");
        }
        assert_eq!(laid(&mut cache, &state, 80, &th), 1);
    }

    #[test]
    fn each_thing_a_layout_depends_on_lays_it_out_again() {
        let th = theme();
        let mut state = session(10);
        let mut cache = TranscriptCache::default();
        laid(&mut cache, &state, 80, &th);
        // Width: everything.
        assert_eq!(laid(&mut cache, &state, 70, &th), 30);
        // A fold toggled: from that entry on (rows below it moved).
        state.folds.insert(10, !state.thought_open(10));
        assert_eq!(laid(&mut cache, &state, 70, &th), 20);
        assert_eq!(laid(&mut cache, &state, 70, &th), 0);
        // Reasoning shown for all (^t): the blocks and what follows them.
        state.show_thinking = !state.show_thinking;
        state.folds.clear();
        assert!(laid(&mut cache, &state, 70, &th) >= 20);
        // The theme.
        let other = Theme::resolve(|k| (k == "MARLOWE_ACCENT").then(|| "#445566".to_string()), Some("truecolor")).unwrap();
        assert_eq!(laid(&mut cache, &state, 70, &other), 30);
        // The transcript replaced (a reopened session): different entries, different keys.
        state = session(4);
        state.epoch += 1;
        assert_eq!(laid(&mut cache, &state, 70, &other), 12);
        // A turn starting or ending flips the last entry's live flag; its block, closed by the
        // text under it, is settled either way.
        state.live = Some(live_turn());
        assert_eq!(laid(&mut cache, &state, 70, &other), 1);
        state.live = None;
        assert_eq!(laid(&mut cache, &state, 70, &other), 1);
        // A block still being written (no text after it) is live with the turn.
        state.entries.push(Entry::Tool { call_id: "t".into(), summary: "read a".into(), status: ToolStatus::Running });
        assert_eq!(laid(&mut cache, &state, 70, &other), 1);
        state.live = Some(live_turn());
        assert_eq!(laid(&mut cache, &state, 70, &other), 1);
    }

    #[test]
    fn an_edits_diff_arriving_lays_its_tool_line_out_again() {
        let th = theme();
        let mut state = State::new(None);
        state.entries.push(Entry::Tool { call_id: "e1".into(), summary: "edit a.rs".into(), status: ToolStatus::Done(String::new()) });
        state.entries.push(Entry::Prose { text: "done".into() });
        let mut cache = TranscriptCache::default();
        laid(&mut cache, &state, 80, &th);
        let before = cache.lines.len();
        let diff = "--- a/a.rs\n+++ b/a.rs\n@@ -1,1 +1,2 @@\n-old\n+new\n+newer\n";
        state.record_tool_finish("e1", true, diff);
        assert_eq!(laid(&mut cache, &state, 80, &th), 2, "the diff's rows move what follows");
        assert!(cache.lines.len() > before);
        same_as_scratch(&mut cache, &state, 80, &th);
    }
}
