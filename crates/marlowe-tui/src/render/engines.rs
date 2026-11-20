//! The engine window (the llama.cpp backends: what is installed, what is newer, the buttons) and
//! the release prompt. Ink tokens only; the window is a panel like the planner, never a fill.

use marlowe_proto::{BackendBusy, BackendReport, BackendUpdate};
use marlowe_surface::chrome::Ink;
use marlowe_surface::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use super::overlay::{centred, panel, panel_hints, scrim};
use super::{hit, hovered, ink, put};
use crate::engines::{ago, buttons, installed_line, Btn, Button};
use crate::state::{Overlay, State, Target};
use crate::text;

/// Widest the window gets.
const MAX_W: u16 = 100;

/// One row of a card: its line, and the buttons on it as (column in the row, width, index).
struct Row {
    line: Line<'static>,
    buttons: Vec<(u16, u16, usize)>,
}

impl Row {
    fn of(line: Line<'static>) -> Self {
        Row { line, buttons: Vec::new() }
    }
}

fn dim(s: impl Into<String>, theme: &Theme) -> Span<'static> {
    Span::styled(s.into(), ink(Ink::Dim, theme))
}

fn dimmer(s: impl Into<String>, theme: &Theme) -> Span<'static> {
    Span::styled(s.into(), ink(Ink::Dimmer, theme))
}

/// The window.
pub fn window(area: Rect, state: &State, theme: &Theme, buf: &mut Buffer) {
    let Some(Overlay::Engines { sel, button }) = &state.overlay else { return };
    let e = &state.engines;
    let w = area.width.saturating_sub(4).min(MAX_W);
    let tw = w.saturating_sub(4) as usize;
    let cards = |compact: bool| -> Vec<Vec<Row>> {
        e.backends.iter().enumerate().map(|(i, r)| card(state, i, r, *sel, *button, tw, compact, theme)).collect()
    };
    let mut flash = state.flash.as_ref().map(|f| (text::truncate(&text::clean(&f.text), tw), f.tone));
    // The roomy layout, unless the terminal is too short for it.
    let room = area.height.saturating_sub(4) as usize;
    let extra = 1 + usize::from(flash.is_some());
    let mut cards_v = cards(false);
    if cards_v.iter().map(Vec::len).sum::<usize>() + extra > room {
        cards_v = cards(true);
    }
    if cards_v.iter().map(Vec::len).sum::<usize>() + extra > room {
        flash = None;
    }
    let body: usize = cards_v.iter().map(Vec::len).sum();
    let h = (body + usize::from(flash.is_some()) + 2).min(u16::MAX as usize) as u16;
    scrim(area, theme, buf);
    let r = centred(area, w, h, false);
    let inner = panel(r, "engine \u{b7} the llama.cpp that serves local models", ink(Ink::Accent, theme), false, buf);
    if !e.known {
        let msg = if state.conn.is_online() { "asking the daemon\u{2026}" } else { "offline \u{2014} the engine window needs the daemon" };
        put(buf, inner.x, inner.y + 1, &Line::from(dim(msg, theme)), inner.width);
    }
    let mut y = inner.y;
    for (i, rows) in cards_v.into_iter().enumerate() {
        for row in rows {
            if y >= inner.bottom() {
                break;
            }
            hit(state, Rect { x: inner.x, y, width: inner.width, height: 1 }, Target::EngineCard(i, None));
            put(buf, inner.x, y, &row.line, inner.width);
            for (x, bw, b) in row.buttons {
                let t = Target::EngineCard(i, Some(b));
                hit(state, Rect { x: inner.x + x, y, width: bw, height: 1 }, t);
            }
            y += 1;
        }
    }
    if let Some((text, tone)) = flash {
        let at = inner.bottom().saturating_sub(1).max(y);
        if at < inner.bottom() {
            put(buf, inner.x, at, &Line::from(Span::styled(text, super::note_style(tone, theme))), inner.width);
        }
    }
    panel_hints(r, &[("\u{2191}\u{2193}", "backend"), ("\u{2190}\u{2192}", "button"), ("\u{21b5}", "press"), ("s u c b a", "keys"), ("esc", "close")], theme, buf);
}

/// One backend: name, what it is, what is installed, what is upstream, the buttons, the progress.
#[allow(clippy::too_many_arguments)]
fn card(state: &State, i: usize, r: &BackendReport, sel: usize, button: usize, tw: usize, compact: bool, theme: &Theme) -> Vec<Row> {
    let e = &state.engines;
    let focused = i == sel;
    let busy = e.busy(r);
    let mut rows: Vec<Row> = Vec::new();

    // `› ms-llama.cpp   serving models`
    let name_style = if focused { ink(Ink::Accent, theme).add_modifier(Modifier::BOLD) } else { theme.normal() };
    let mut head = vec![Span::styled(if focused { "\u{203a} " } else { "  " }, ink(Ink::Accent, theme)), Span::styled(r.title.clone(), name_style)];
    if r.selected {
        head.push(Span::styled("   serving local models", ink(Ink::Green, theme)));
    }
    rows.push(Row::of(Line::from(head)));

    if !compact {
        for l in text::wrap(&text::clean(&r.description), tw.saturating_sub(2)).into_iter().take(2) {
            rows.push(Row::of(Line::from(vec![Span::raw("  "), dim(l, theme)])));
        }
    }

    // The installed build and where it stands against the newest release.
    let mut spans = vec![Span::raw("  ")];
    match installed_line(r) {
        Some(line) => spans.push(Span::styled(line, theme.normal())),
        None => spans.push(Span::styled("not installed", ink(Ink::Amber, theme))),
    }
    spans.push(dimmer("  \u{b7}  ", theme));
    spans.push(match (r.state, &r.latest_release, r.approx_bytes) {
        (BackendUpdate::UpToDate, ..) => Span::styled("up to date", ink(Ink::Green, theme)),
        (BackendUpdate::UpdateAvailable, Some(t), _) => Span::styled(format!("update available: {}", t.tag), ink(Ink::Amber, theme)),
        (BackendUpdate::UpdateAvailable, None, _) => Span::styled("update available", ink(Ink::Amber, theme)),
        (BackendUpdate::NotInstalled, _, b) if b > 0 => dim(format!("about {} MB to download", text::megabytes(b)), theme),
        (BackendUpdate::NotInstalled, ..) => dim("install it to run local models", theme),
        (BackendUpdate::Unknown, ..) => dim("no release looked up yet", theme),
    });
    rows.push(Row::of(Line::from(spans)));

    // What upstream has beyond it.
    let mut up = vec![Span::raw("  ")];
    match (r.builds_newer, &r.newest_build) {
        (Some(0), _) => up.push(dim("no newer build upstream", theme)),
        (Some(n), tag) => {
            up.push(Span::styled(format!("{n} newer build{} upstream", if n == 1 { "" } else { "s" }), ink(Ink::Amber, theme)));
            if let Some(t) = tag {
                up.push(dim(format!(" \u{b7} newest {}", t.tag), theme));
            }
        }
        (None, Some(t)) => up.push(dim(format!("newest build {}", t.tag), theme)),
        (None, None) => up.push(dimmer("builds not looked at yet", theme)),
    }
    if let Some(at) = r.last_checked {
        up.push(dimmer(format!(" \u{b7} checked {}", ago(at, (state.now_ms / 1000).max(0) as u64)), theme));
    }
    rows.push(Row::of(Line::from(up)));

    // Running on the CPU is never silent.
    if let Some(b) = r.installed.as_ref().filter(|b| b.device == "cpu") {
        let why = b.note.as_deref().map(text::clean).unwrap_or_default();
        let msg = format!("! no GPU seen by the smoke test, so it lists the CPU only{}", if why.is_empty() { String::new() } else { format!(" ({why})") });
        rows.push(Row::of(Line::from(vec![Span::raw("  "), Span::styled(text::truncate(&msg, tw.saturating_sub(2)), ink(Ink::Amber, theme))])));
    }
    // What an upgrade or a sync would cost (the MS models it would stop running).
    let warnings = [(r.state == BackendUpdate::UpdateAvailable).then_some(&r.upgrade_warning), Some(&r.sync_warning)];
    for wn in warnings.into_iter().flatten().flatten() {
        for l in text::wrap(&format!("! {}", text::clean(wn)), tw.saturating_sub(2)).into_iter().take(2) {
            rows.push(Row::of(Line::from(vec![Span::raw("  "), Span::styled(l, ink(Ink::Amber, theme))])));
        }
    }

    // Buttons: `[Select]  [Install]  [Check for updates]  [Sync to upstream]`.
    let list = buttons(r, busy.is_some());
    let mut spans = vec![Span::raw("  ")];
    let mut hits = Vec::new();
    let mut x = 2u16;
    for (bi, b) in list.iter().enumerate() {
        let label = format!("[{}]", b.label);
        let bw = text::width(&label) as u16;
        if x as usize + bw as usize > tw {
            break;
        }
        let on = focused && bi == button;
        spans.push(Span::styled(label, button_style(b, on, hovered(state, &Target::EngineCard(i, Some(bi))), theme)));
        spans.push(Span::raw("  "));
        hits.push((x, bw, bi));
        x += bw + 2;
    }
    rows.push(Row { line: Line::from(spans), buttons: hits });

    // What is running, or why the last thing failed.
    if let Some(p) = busy {
        rows.push(Row::of(progress(p, tw.saturating_sub(2), theme)));
    } else if let Some(err) = e.errors.get(&r.id).or(r.note.as_ref()) {
        let (mark, style) = if e.errors.contains_key(&r.id) { ("\u{2717} ", ink(Ink::Red, theme)) } else { ("", ink(Ink::Amber, theme)) };
        for l in text::wrap(&format!("{mark}{}", text::clean(err)), tw.saturating_sub(2)).into_iter().take(3) {
            rows.push(Row::of(Line::from(vec![Span::raw("  "), Span::styled(l, style)])));
        }
    }
    if !compact {
        rows.push(Row::of(Line::from("")));
    }
    rows
}

fn button_style(b: &Button, on: bool, over: bool, theme: &Theme) -> ratatui::style::Style {
    match (b.off.is_some(), on, b.kind) {
        (true, ..) => ink(Ink::Dimmer, theme),
        (false, true, _) => ink(Ink::Accent, theme).add_modifier(Modifier::BOLD),
        (false, false, _) if over => ink(Ink::Hover, theme),
        (false, false, Btn::Prompts) => ink(Ink::Dim, theme),
        (false, false, _) => theme.normal(),
    }
}

/// `━━━━━────  42%  downloading llama-b12359-bin-win-cuda-12.4-x64.zip`, or the step's words.
fn progress(p: &BackendBusy, tw: usize, theme: &Theme) -> Line<'static> {
    let mut spans = vec![Span::raw("  ")];
    let bar = 16usize.min(tw / 3);
    if p.total > 0 {
        let frac = (p.done as f64 / p.total as f64).clamp(0.0, 1.0);
        let filled = (frac * bar as f64).round() as usize;
        spans.push(Span::styled("\u{2501}".repeat(filled), ink(Ink::Info, theme)));
        spans.push(Span::styled("\u{2500}".repeat(bar - filled), ink(Ink::Dimmer, theme)));
        spans.push(Span::styled(format!(" {:>3.0}%", frac * 100.0), theme.normal()));
        spans.push(dim(format!(" \u{b7} {} of {} MB", text::megabytes(p.done), text::megabytes(p.total)), theme));
    } else {
        spans.push(Span::styled("\u{2026}", ink(Ink::Info, theme)));
    }
    let used: usize = spans.iter().map(|s| text::width(&s.content)).sum();
    let msg = text::clean(&p.message);
    if !msg.is_empty() {
        spans.push(dim(format!(" \u{b7} {}", text::truncate(&msg, tw.saturating_sub(used + 3))), theme));
    }
    Line::from(spans)
}

/// The release prompt: `A new release of llama.cpp is available (v1.1.0). Upgrade?` and three answers.
pub fn upgrade(area: Rect, state: &State, theme: &Theme, buf: &mut Buffer) {
    let Some(u) = state.engines.upgrade.as_ref() else { return };
    let accent = ink(Ink::Accent, theme);
    let dimmer_s = ink(Ink::Dimmer, theme);
    let w: u16 = 76;
    let tw = (w - 4) as usize;
    let mut lines: Vec<Line> = Vec::new();
    let ask = format!("A new release of {} is available ({}). Upgrade?", text::clean(&u.title), text::clean(&u.tag));
    for l in text::wrap(&ask, tw) {
        lines.push(Line::from(Span::styled(l, theme.bright())));
    }
    if let Some(wn) = &u.warning {
        for l in text::wrap(&format!("! {wn}"), tw) {
            lines.push(Line::from(Span::styled(l, ink(Ink::Amber, theme))));
        }
    }
    lines.push(Line::from(""));
    let mut spans = Vec::new();
    let mut x = 0u16;
    let mut at: Vec<(u16, u16, usize)> = Vec::new();
    for (i, label) in ["Yes", "No", "Don't ask again"].into_iter().enumerate() {
        let on = i == u.sel;
        let t = Target::EngineChoice(i);
        let style = match (on, hovered(state, &t)) {
            (true, _) => accent.add_modifier(Modifier::BOLD),
            (false, true) => ink(Ink::Hover, theme),
            (false, false) => theme.normal(),
        };
        let item = format!("{} {}", i + 1, label);
        let iw = (text::width(&item) + 2) as u16;
        spans.push(Span::styled(if on { "\u{203a} " } else { "  " }, accent));
        spans.push(Span::styled(format!("{} ", i + 1), dimmer_s));
        spans.push(Span::styled(label, style));
        spans.push(Span::raw("      "));
        at.push((x, iw, i));
        x += iw + 6;
    }
    let choice_row = lines.len() as u16;
    lines.push(Line::from(spans));
    lines.push(Line::from(""));
    let key = |k: &'static str, what: &'static str| [Span::styled(k, accent), Span::styled(what, ink(Ink::Dim, theme))];
    lines.push(Line::from([key("\u{21b5}", " choose   "), key("\u{2190} \u{2192}", " move   "), key("esc", " not now")].concat()));

    scrim(area, theme, buf);
    let r = centred(area, w, lines.len() as u16 + 2, false);
    let inner = panel(r, "engine", accent, false, buf);
    for (i, l) in lines.iter().enumerate().take(inner.height as usize) {
        put(buf, inner.x, inner.y + i as u16, l, inner.width);
    }
    for (x, iw, i) in at {
        hit(state, Rect { x: inner.x + x, y: inner.y + choice_row, width: iw.min(inner.width), height: 1 }, Target::EngineChoice(i));
    }
}
