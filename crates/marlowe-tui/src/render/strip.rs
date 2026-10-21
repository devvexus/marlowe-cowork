//! The live strip: two rows under the streaming output that say what the turn is doing right now.
//!
//! Row one is the activity line, `⣾ thinking · 4.1s · 312 tok`; row two carries the detail that
//! has its own line (recall stages, a model load, a compaction, the memory write). The height is a
//! constant while the strip is up, so nothing above it moves as phases come and go, and numbers
//! that tick are fixed width so nothing beside them moves either.
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
use crate::state::State;
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
    if r.height > 1 {
        if let Some(line) = detail(state, theme) {
            put(buf, r.x + pad, r.y + 1, &line, w as u16);
        }
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
    match ms {
        0..=999 => format!("{ms}ms"),
        1_000..=9_999 => format!("{:.1}s", ms as f64 / 1000.0),
        _ => text::took(ms),
    }
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
/// `short` is the narrow form `8.2k/1.9s`. Numbers still estimated carry a `~`.
pub(super) fn read_spans(r: &Read, short: bool, theme: &Theme) -> Vec<Span<'static>> {
    let plus = if r.later { "+" } else { "" };
    let new = format!("{plus}{}{}", tilde(r.new_est), text::compact(r.new));
    let ms = r.ms.map(|ms| format!("{}{}", tilde(r.ms_est), took(ms)));
    if short {
        return match ms {
            Some(ms) => vec![num(format!("{new}/{ms}"), theme)],
            None => vec![num(new, theme)],
        };
    }
    let mut out = vec![label("read ", theme), num(new, theme)];
    if r.cached > 0 {
        out.push(label(" (", theme));
        out.push(num(format!("{}{}", tilde(r.cached_est), text::compact(r.cached)), theme));
        out.push(label(" cached)", theme));
    }
    if let Some(ms) = ms {
        out.push(sep(theme));
        out.push(num(ms, theme));
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
        let body = vec![
            Span::styled(text::truncate(&text::clean(&t.summary), room.max(8)), ink(Ink::Green, theme)),
            sep(theme),
            label(clock(now - t.since), theme),
            label(more, theme),
        ];
        return Some(lead(g, Ink::Green, body, theme));
    }
    if let Some(l) = &s.load {
        let body = vec![label("loading the model", theme), sep(theme), label(clock(now - l.started), theme)];
        return Some(lead(g, Ink::Hover, body, theme));
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
        return Some(lead(g, Ink::Accent, with_read(c.read.as_ref(), vec![tail], w, theme), theme));
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
        return Some(lead(g, Ink::Speech, with_read(c.read.as_ref(), tails, w, theme), theme));
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

/// The read segment before the state of the call, shortened until the line fits. `tails` run from
/// the fullest to the barest; the read segment goes only after every tail has been tried short.
fn with_read(read: Option<&Read>, tails: Vec<Vec<Span<'static>>>, w: usize, theme: &Theme) -> Vec<Span<'static>> {
    let Some(r) = read else { return pick(tails, w) };
    let mut variants = Vec::new();
    let mut full = read_spans(r, false, theme);
    full.push(Span::styled("  ›  ", ink(Ink::Dimmer, theme)));
    full.extend(tails[0].clone());
    variants.push(full);
    for t in &tails {
        let mut short = read_spans(r, true, theme);
        short.push(Span::styled(" › ", ink(Ink::Dimmer, theme)));
        short.extend(t.clone());
        variants.push(short);
    }
    variants.push(tails[tails.len() - 1].clone());
    pick(variants, w)
}

// ---- row two: the detail ------------------------------------------------------------------------

fn detail(state: &State, theme: &Theme) -> Option<Line<'static>> {
    let s = &state.strip;
    let now = state.now_ms;
    let g = spin(now, 3);
    if let Some(l) = &s.load {
        let name = text::truncate(&l.model, 24);
        let mut v = vec![label(format!("loading {name}"), theme)];
        match (l.done_mib, l.total_mib) {
            (done, Some(total)) if done > 0 && total > 0 => {
                v.push(Span::raw(" "));
                v.extend(bar(done as f64 / total as f64, Ink::Hover, theme));
                v.push(num(format!(" {} / {}{} GB", gb(done), tilde(l.est), gb(total)), theme));
            }
            (done, None) if done > 0 => v.push(num(format!(" {} GB", gb(done)), theme)),
            _ => {
                v.push(sep(theme));
                v.push(num(clock(now - l.started), theme));
            }
        }
        return Some(lead(g, Ink::Hover, v, theme));
    }
    if let Some(c) = &s.compact {
        let before = if c.before > 0 { format!(" {}", text::compact(c.before)) } else { String::new() };
        let mut v = vec![label(format!("compacting{before} → …"), theme)];
        if c.cap > 0 {
            // The cap is the most the summary can be, so the figure is an estimate.
            v.push(sep(theme));
            v.push(num(format!("~{}/{} tok", text::thousands(c.written), text::thousands(c.cap)), theme));
        }
        v.push(sep(theme));
        v.push(num(clock(now - c.started), theme));
        return Some(lead(g, Ink::Hover, v, theme));
    }
    if let Some(RecallItem::Stage(st)) = s.recall_item() {
        return Some(lead(g, Ink::Hover, recall_spans(&st, theme), theme));
    }
    let shown = |until: Option<i64>| state.live.is_some() || until.is_some_and(|u| now < u);
    if let Some(c) = s.compacted.as_ref().filter(|c| shown(c.until)) {
        let v = if c.failed {
            vec![Span::styled("compaction failed", ink(Ink::Red, theme)), label(format!(" · kept {}", text::compact(c.before)), theme)]
        } else {
            vec![
                label("compacted ", theme),
                num(format!("{} → {}", text::compact(c.before), text::compact(c.after)), theme),
                sep(theme),
                num(took(c.ms), theme),
            ]
        };
        let (glyph, ik) = if c.failed { ('✗', Ink::Red) } else { ('✓', Ink::Green) };
        return Some(lead(glyph, ik, v, theme));
    }
    if let Some(l) = s.loaded.as_ref().filter(|l| shown(l.until)) {
        let mut v = vec![label(format!("loaded {}", text::truncate(&l.model, 24)), theme)];
        if let Some(mib) = l.mib {
            v.push(sep(theme));
            v.push(num(format!("{} GB", gb(mib)), theme));
        }
        v.push(sep(theme));
        v.push(num(took(l.ms), theme));
        return Some(lead('✓', Ink::Green, v, theme));
    }
    if s.tail_showing(now) {
        let t = s.tail.as_ref()?;
        let mut v = Vec::new();
        if t.ingested > 0 {
            v.push(num(format!("+{}", t.ingested), theme));
            v.push(label(format!(" memor{}", if t.ingested == 1 { "y" } else { "ies" }), theme));
        } else {
            v.push(label("memory", theme));
        }
        if let Some((done, total)) = t.embedding {
            v.push(sep(theme));
            v.push(label("embedding ", theme));
            v.push(num(format!("{done}/{total}"), theme));
        }
        if let Some((_, ms)) = t.embedded {
            v.push(sep(theme));
            v.push(label("embedded ", theme));
            v.push(num(took(ms), theme));
        }
        if t.indexed {
            v.push(sep(theme));
            v.push(label("indexed", theme));
            return Some(lead('✓', Ink::Green, v, theme));
        }
        return Some(lead(g, Ink::Hover, v, theme));
    }
    None
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
