//! The boot overlay (the presence meter assembling, then systems lines ticking in with real
//! values) and the notice toasts. Foreground only; `Clear` resets cells to the terminal default.

use marlowe_proto::Severity;
use marlowe_surface::chrome::Ink;
use marlowe_surface::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Widget};

use super::{ink, meter, put, Geometry};
use crate::seq::SeqLine;
use crate::state::State;
use crate::text;
use crate::wake::{notice_glyph, Health, BOOT_LINES};

/// Label column of a systems line, leader dots included: `memory ····· `.
const LABEL_W: usize = 13;
/// The block's width: fixed, so nothing shifts as lines arrive.
const BLOCK_W: u16 = 60;
const TOAST_W: u16 = 46;

/// The boot overlay, in place of the conversation pane.
pub fn draw(r: Rect, state: &State, theme: &Theme, buf: &mut Buffer) {
    let Some(boot) = state.wake.boot else { return };
    if r.width < 30 || r.height < 10 {
        return;
    }
    let now = state.now_ms;
    let assembled = boot.assembled(now) >= 1.0;
    let shown = boot.lines_shown(now);
    let systems = state.systems();

    let mut lines: Vec<Line> = Vec::new();
    let name = if assembled {
        ink(Ink::Accent, theme).add_modifier(Modifier::BOLD)
    } else {
        ink(Ink::Dimmer, theme)
    };
    lines.push(Line::from(vec![
        Span::styled(meter(state), ink(Ink::Accent, theme)),
        Span::raw("  "),
        Span::styled("MARLOWE", name),
    ]));
    lines.push(Line::from(""));
    for (i, s) in systems.iter().enumerate().take(BOOT_LINES) {
        if i >= shown {
            lines.push(Line::from(""));
            continue;
        }
        let sl = SeqLine { health: s.health, label: s.label.to_string(), value: s.value.clone(), done: s.health != Health::Pending };
        lines.push(super::seq::line(&sl, LABEL_W, r.width.saturating_sub(20) as usize, theme));
    }
    lines.push(Line::from(""));
    let hint = if shown >= BOOT_LINES { "any key to begin" } else { "any key skips" };
    lines.push(Line::from(Span::styled(hint, ink(Ink::Dimmer, theme))));

    // A block centred as a whole, left-aligned inside, a little above the middle.
    let x = r.x + r.width.saturating_sub(BLOCK_W) / 2;
    let top = r.y + r.height.saturating_sub(lines.len() as u16) * 2 / 5;
    for (i, l) in lines.iter().enumerate() {
        let y = top + i as u16;
        if y < r.bottom() {
            put(buf, x, y, l, r.right().saturating_sub(x));
        }
    }
}

/// Crew runs that need the user or finished, then new Attention/Alarm notices, stacked at the
/// top right of the main pane so the rail stays readable. Newest of each on top.
pub fn toasts(g: &Geometry, state: &State, theme: &Theme, buf: &mut Buffer) {
    let pane = g.conversation;
    let w = TOAST_W.min(pane.width.saturating_sub(4));
    let x = pane.right().saturating_sub(w + 1);
    let mut y = pane.y;
    let crew = state.crew.live_toasts(state.now_ms).collect::<Vec<_>>();
    let notices = state.wake.live_toasts(state.now_ms).collect::<Vec<_>>();
    let items = crew
        .iter()
        .rev()
        .map(|t| (t.severity, crew_glyph(t.severity), t.title.as_str(), t.detail.as_str()))
        .chain(notices.iter().rev().map(|t| {
            let n = &t.notice;
            (n.severity, notice_glyph(n.severity), n.title.as_str(), n.detail.as_str())
        }));
    for (severity, glyph, title, detail) in items {
        let detail = text::clean(detail.lines().next().unwrap_or("").trim());
        let h: u16 = if detail.is_empty() { 3 } else { 4 };
        if y + h > pane.bottom() {
            break;
        }
        let r = Rect { x, y, width: w, height: h };
        let tone = severity_style(severity, theme);
        Clear.render(r, buf);
        Block::bordered().border_type(BorderType::Rounded).border_style(tone).render(r, buf);
        let tw = w.saturating_sub(4);
        let title = Line::from(vec![
            Span::styled(format!("{glyph} "), tone),
            Span::styled(text::truncate(&text::clean(title), (tw as usize).saturating_sub(2)), theme.bright()),
        ]);
        put(buf, x + 2, y + 1, &title, tw);
        if !detail.is_empty() {
            let d = Line::from(Span::styled(text::truncate(&detail, tw as usize), ink(Ink::Dim, theme)));
            put(buf, x + 2, y + 2, &d, tw);
        }
        y += h;
    }
}

/// A crew toast's glyph: the run status it reports.
fn crew_glyph(s: Severity) -> &'static str {
    match s {
        Severity::Attention => "◐",
        Severity::Alarm => "✗",
        _ => "✓",
    }
}

/// Colour for a severity: Info dim, Good green, Attention amber, Alarm red.
pub(crate) fn severity_style(s: Severity, theme: &Theme) -> Style {
    match s {
        Severity::Info => ink(Ink::Dim, theme),
        Severity::Good => ink(Ink::Green, theme),
        Severity::Attention => ink(Ink::Amber, theme),
        Severity::Alarm => ink(Ink::Red, theme),
    }
}
