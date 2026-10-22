//! The live strip: one row under the streaming output that says what the turn is doing right now,
//! `⣾ thinking 4.1s · 312 tok`. It is the bottom of the conversation, so it is only ever the
//! present: one state, one line, one clock. A state the transcript already shows (a running tool's
//! summary, a thinking block) is not repeated here, and what a stage leaves behind (a load, a
//! compaction, the memory write) is a transcript note, not a strip line. The height is a constant
//! while the strip is up, so nothing above it moves as phases come and go, and numbers that tick
//! are fixed width so nothing beside them moves either.
//!
//! Inks follow the kind of work: thinking is Marlowe's voice colour, writing the accent, tools the
//! state green, waiting amber; labels are dim and the numbers that were measured are amber.

use marlowe_surface::chrome::Ink;
use marlowe_surface::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::{ink, put};
use crate::live::{self, Read, RecallItem, StageRun};
use crate::state::{Entry, State, ToolStatus};
use crate::text;

const SPIN: [char; 8] = ['⣾', '⣽', '⣻', '⢿', '⡿', '⣟', '⣯', '⣷'];
/// The pane is too short for the strip below this many rows: the transcript keeps them.
const MIN_PANE: u16 = 8;
const BAR: usize = 10;

/// Rows the strip takes in a pane `full` rows tall: a function of state alone, never of text.
pub(super) fn height(state: &State, full: Rect) -> u16 {
    if state.strip_up() && full.height >= MIN_PANE {
        live::ROWS
    } else {
        0
    }
}

pub(super) fn draw(r: Rect, state: &State, theme: &Theme, buf: &mut Buffer, pad: u16) {
    let w = r.width.saturating_sub(pad + 1) as usize;
    if w < 8 {
        return;
    }
    if let Some(line) = activity(state, theme, w) {
        put(buf, r.x + pad, r.y, &line, w as u16);
    }
    state.strip.drawn();
}

fn spin(now: i64, offset: i64) -> char {
    SPIN[((now.max(0) / 80 + offset).rem_euclid(8)) as usize]
}

// ---- number formats ---------------------------------------------------------------------------

/// A time that ticks, five columns wide so what follows it never moves: ` 4.1s`, `12.3s`, `1m05s`.
fn clock(ms: i64) -> String {
    let ms = ms.max(0) as u64;
    match ms {
        0..=999 => format!("{ms:>3}ms"),
        1_000..=59_999 => format!("{:>4.1}s", ms as f64 / 1000.0),
        _ => format!("{}m{:02}s", ms / 60_000, (ms / 1000) % 60),
    }
}

/// A time that has settled: `340ms`, `1.9s`, `12s`, `1m05s`.
pub(super) fn took(ms: u64) -> String {
    live::took(ms)
}

fn per_second(r: f32) -> String {
    match r {
        r if r >= 1_000_000.0 => format!("{:.1}M", r / 1_000_000.0),
        r if r >= 1_000.0 => format!("{:.1}k", r / 1_000.0),
        r => format!("{r:.0}"),
    }
}

fn gb(mib: u64) -> String {
    format!("{:.1}", mib as f64 / 1024.0)
}

// ---- spans ------------------------------------------------------------------------------------

fn label(s: impl Into<String>, theme: &Theme) -> Span<'static> {
    Span::styled(s.into(), ink(Ink::Dim, theme))
}

fn num(s: impl Into<String>, theme: &Theme) -> Span<'static> {
    Span::styled(s.into(), ink(Ink::Amber, theme))
}

fn sep(theme: &Theme) -> Span<'static> {
    Span::styled(" · ", ink(Ink::Dimmer, theme))
}

fn tilde(est: bool) -> &'static str {
    if est {
        "~"
    } else {
        ""
    }
}

/// What a model call read, as `read 8.2k · 1.9s`; a later step `read +1.2k (8.0k cached) · 0.3s`.
/// Numbers still estimated carry a `~`. Shown under an opened thinking block, never on the strip.
pub(super) fn read_spans(r: &Read, theme: &Theme) -> Vec<Span<'static>> {
    let plus = if r.later { "+" } else { "" };
    let new = format!("{plus}{}{}", tilde(r.new_est), text::compact(r.new));
    let mut out = vec![label("read ", theme), num(new, theme)];
    if r.cached > 0 {
        out.push(label(" (", theme));
        out.push(num(format!("{}{}", tilde(r.cached_est), text::compact(r.cached)), theme));
        out.push(label(" cached)", theme));
    }
    if let Some(ms) = r.ms {
        out.push(sep(theme));
        out.push(num(format!("{}{}", tilde(r.ms_est), took(ms)), theme));
    }
    out
}

fn width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| text::width(&s.content)).sum()
}

/// The first variant that fits `w`, else the last (which `put` clips).
fn pick(mut variants: Vec<Vec<Span<'static>>>, w: usize) -> Vec<Span<'static>> {
    let at = variants.iter().position(|v| width(v) <= w).unwrap_or(variants.len().saturating_sub(1));
    variants.swap_remove(at)
}

fn bar(frac: f64, filled: Ink, theme: &Theme) -> Vec<Span<'static>> {
    let at = ((frac.clamp(0.0, 1.0) * BAR as f64) as usize).min(BAR);
    let mut out = Vec::new();
    for i in 0..BAR {
        let (g, ik) = match i.cmp(&at) {
            std::cmp::Ordering::Less => ('━', filled),
            std::cmp::Ordering::Equal if at < BAR => ('╺', Ink::Accent),
            _ => ('━', Ink::Dimmer),
        };
        out.push(Span::styled(g.to_string(), ink(ik, theme)));
    }
    out
}

fn lead(glyph: char, ik: Ink, mut body: Vec<Span<'static>>, theme: &Theme) -> Line<'static> {
    let mut spans = vec![Span::styled(format!("{glyph} "), ink(ik, theme))];
    spans.append(&mut body);
    Line::from(spans)
}

// ---- row one: the activity line -----------------------------------------------------------------

fn activity(state: &State, theme: &Theme, w: usize) -> Option<Line<'static>> {
    let s = &state.strip;
    let now = state.now_ms;
    let w = w.saturating_sub(2);
    let g = spin(now, 0);
    // Waiting on the user outranks everything: it is the one thing only they can end.
    if state.approvals.iter().any(|a| state.mine(&a.session)) {
        let since = if state.approval_since_ms > 0 { now - state.approval_since_ms } else { 0 };
        let body = vec![Span::styled("waiting for your approval", ink(Ink::Amber, theme)), sep(theme), label(clock(since), theme)];
        return Some(lead(g, Ink::Amber, body, theme));
    }
    if let Some(wait) = &s.wait {
        let body = vec![Span::styled(text::truncate(&wait.detail, w.saturating_sub(8)), ink(Ink::Amber, theme)), sep(theme), label(clock(now - wait.since), theme)];
        return Some(lead(g, Ink::Amber, body, theme));
    }
    if let Some(t) = s.tools.last() {
        let more = if s.tools.len() > 1 { format!("  +{}", s.tools.len() - 1) } else { String::new() };
        let room = w.saturating_sub(9 + text::width(&more));
        // The transcript's running row already says which call: the strip says only that it runs.
        let above = state.entries.iter().rev().any(|e| matches!(e, Entry::Tool { call_id, status: ToolStatus::Running, .. } if *call_id == t.call_id));
        let what = if above { "running".to_string() } else { text::truncate(&text::clean(&t.summary), room.max(8)) };
        let body = vec![
            Span::styled(what, ink(Ink::Green, theme)),
            sep(theme),
            label(clock(now - t.since), theme),
            label(more, theme),
        ];
        return Some(lead(g, Ink::Green, body, theme));
    }
    if let Some(l) = &s.load {
        return Some(lead(g, Ink::Hover, pick(load_variants(l, now, theme), w), theme));
    }
    if let Some(c) = &s.compact {
        return Some(lead(g, Ink::Hover, pick(compact_variants(c, now, theme), w), theme));
    }
    if let Some(RecallItem::Stage(st)) = s.recall_item() {
        return Some(lead(g, Ink::Hover, recall_spans(&st, theme), theme));
    }
    let c = &s.call;
    if let Some(since) = c.write_since {
        let rate = c.window.rate(now);
        let mut tail = vec![Span::styled("writing", ink(Ink::Accent, theme))];
        if let Some(r) = rate {
            tail.push(sep(theme));
            tail.push(Span::styled(format!("~{r:>3} tok/s"), ink(Ink::Accent, theme)));
        } else {
            tail.push(sep(theme));
            tail.push(Span::styled(clock(now - since), ink(Ink::Accent, theme)));
        }
        return Some(lead(g, Ink::Accent, tail, theme));
    }
    if let Some(since) = c.think_since {
        let mut tail = vec![
            Span::styled("thinking ", ink(Ink::Speech, theme)),
            Span::styled(clock(now - since), ink(Ink::Speech, theme)),
            Span::styled(format!(" · {} tok", text::thousands(c.think_tokens)), ink(Ink::Speech, theme)),
        ];
        // Narrow widths drop the thinking rate, then the count, before any of the read segment.
        let mut tails = Vec::new();
        if let Some(r) = c.window.rate(now) {
            tails.push(tail.clone());
            tails.last_mut().unwrap().push(Span::styled(format!(" · {r:>3} tok/s"), ink(Ink::Speech, theme)));
        }
        tails.push(tail.clone());
        tail.truncate(2);
        tails.push(tail);
        return Some(lead(g, Ink::Speech, pick(tails, w), theme));
    }
    if let Some(p) = &c.prefill {
        let since = s.read_from.unwrap_or(p.started);
        let tokens = p.total.unwrap_or(p.est_total);
        let mut head = vec![label("reading ", theme), num(format!("~{}", text::compact(tokens)), theme), label(" tok", theme)];
        let plain = {
            let mut v = head.clone();
            v.push(sep(theme));
            v.push(num(clock(now - since), theme));
            v
        };
        let mut variants = Vec::new();
        if let (Some(rate), Some(total)) = (p.rate, p.total.filter(|t| *t > 0)) {
            let frac = (p.done as f64 / total as f64).min(0.99);
            head.push(sep(theme));
            head.push(num(format!("~{} tok/s", text::thousands(rate.round() as u64)), theme));
            let mut with_bar = head.clone();
            with_bar.push(Span::raw(" "));
            with_bar.extend(bar(frac, Ink::Accent, theme));
            with_bar.push(num(format!(" ~{:>2}%", (frac * 100.0) as u64), theme));
            variants.push(with_bar);
            variants.push(head);
        }
        variants.push(plain);
        return Some(lead(g, Ink::Speech, pick(variants, w), theme));
    }
    // Every short phase carries the turn's running time, so a wait with no stage of its own
    // (a remote model before its first frame) still ticks.
    let waited = |text: &str| {
        let mut v = vec![label(text.to_string(), theme)];
        if let Some(b) = s.began {
            v.push(sep(theme));
            v.push(label(clock(now - b), theme));
        }
        lead(g, Ink::Hover, v, theme)
    };
    if let Some(label_text) = s.short_label() {
        return Some(waited(label_text));
    }
    if s.sending.is_some() {
        return Some(waited("sending"));
    }
    if state.live.is_some() {
        return Some(waited("working"));
    }
    None
}

/// A model load: `loading dusk ━━━━╺━━━━ 4.1 / ~9.0 GB · 3.4s`, shortened until it fits.
fn load_variants(l: &live::Load, now: i64, theme: &Theme) -> Vec<Vec<Span<'static>>> {
    let name = text::truncate(&l.model, 24);
    let head = vec![label(format!("loading {name}"), theme)];
    let clock = vec![sep(theme), num(clock(now - l.started), theme)];
    let mut out = Vec::new();
    match (l.done_mib, l.total_mib) {
        (done, Some(total)) if done > 0 && total > 0 => {
            let gbs = num(format!(" {} / {}{} GB", gb(done), tilde(l.est), gb(total)), theme);
            let mut with_bar = head.clone();
            with_bar.push(Span::raw(" "));
            with_bar.extend(bar(done as f64 / total as f64, Ink::Hover, theme));
            with_bar.push(gbs.clone());
            with_bar.extend(clock.clone());
            out.push(with_bar);
            let mut plain = head.clone();
            plain.push(gbs);
            plain.extend(clock.clone());
            out.push(plain);
        }
        (done, None) if done > 0 => {
            let mut v = head.clone();
            v.push(num(format!(" {} GB", gb(done)), theme));
            v.extend(clock.clone());
            out.push(v);
        }
        _ => {}
    }
    let mut bare = head;
    bare.extend(clock);
    out.push(bare);
    out
}

/// A compaction: `compacting 22.7k → … · ~812/4,096 tok · 43s`, shortened until it fits.
fn compact_variants(c: &live::Compact, now: i64, theme: &Theme) -> Vec<Vec<Span<'static>>> {
    let before = if c.before > 0 { format!(" {}", text::compact(c.before)) } else { String::new() };
    let clock = vec![sep(theme), num(clock(now - c.started), theme)];
    let mut out = Vec::new();
    if c.cap > 0 {
        // The cap is the most the summary can be, so the figure is an estimate.
        let mut v = vec![label(format!("compacting{before} → …"), theme), sep(theme)];
        v.push(num(format!("~{}/{} tok", text::thousands(c.written), text::thousands(c.cap)), theme));
        v.extend(clock.clone());
        out.push(v);
    }
    let mut v = vec![label(format!("compacting{before} → …"), theme)];
    v.extend(clock.clone());
    out.push(v);
    let mut bare = vec![label("compacting", theme)];
    bare.extend(clock);
    out.push(bare);
    out
}

fn recall_spans(st: &StageRun, theme: &Theme) -> Vec<Span<'static>> {
    let mut v = vec![label("recall", theme), sep(theme)];
    if st.name == "load" {
        v.push(label(format!("loading {}", st.detail.as_deref().unwrap_or("a model")), theme));
        if let Some(ms) = st.ms {
            v.push(sep(theme));
            v.push(num(took(ms), theme));
        }
        return v;
    }
    v.push(label(st.name.clone(), theme));
    match (st.ms, st.done, st.total) {
        (Some(ms), _, _) => {
            if let Some(n) = st.total.filter(|n| *n > 0) {
                v.push(num(format!(" {}", text::thousands(n)), theme));
            }
            v.push(sep(theme));
            v.push(num(took(ms), theme));
        }
        (None, Some(done), Some(total)) => {
            v.push(num(format!(" {}/{}", text::thousands(done), text::thousands(total)), theme));
            if let Some(r) = st.rate.filter(|r| *r > 0.0) {
                let unit = match st.unit.as_deref() {
                    Some("items") | None => String::new(),
                    Some(u) => format!(" {u}"),
                };
                v.push(sep(theme));
                v.push(num(format!("{}{unit}/s", per_second(r)), theme));
            }
        }
        _ => {}
    }
    if let Some(d) = st.detail.as_deref().filter(|d| !d.is_empty() && st.name != "load") {
        v.push(sep(theme));
        v.push(label(d.to_string(), theme));
    }
    v
}
