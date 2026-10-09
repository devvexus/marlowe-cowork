//! Floating panels: the approval modal, the ^k palette, the sessions list, the model picker.
//! `Clear` resets cells to the terminal default; nothing here sets a background.

use marlowe_surface::chrome::Ink;
use marlowe_surface::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Widget};

use super::{ink, put};
use crate::input::palette_matches;
use crate::state::{Approval, State, APPROVAL_CHOICES, APPROVAL_DETAIL_ROWS};
use crate::text;

fn centred(area: Rect, w: u16, h: u16, top_bias: bool) -> Rect {
    let w = w.min(area.width.saturating_sub(4));
    let h = h.min(area.height.saturating_sub(2));
    let y = if top_bias {
        area.y + area.height.saturating_sub(h) / 4
    } else {
        area.y + area.height.saturating_sub(h) / 2
    };
    Rect { x: area.x + area.width.saturating_sub(w) / 2, y, width: w, height: h }
}

fn panel(r: Rect, title: &str, border: ratatui::style::Style, double: bool, buf: &mut Buffer) -> Rect {
    Clear.render(r, buf);
    let bt = if double { BorderType::Double } else { BorderType::Rounded };
    Block::bordered()
        .border_type(bt)
        .border_style(border)
        .title(Span::styled(format!(" {title} "), border))
        .render(r, buf);
    Rect { x: r.x + 2, y: r.y + 1, width: r.width.saturating_sub(4), height: r.height.saturating_sub(2) }
}

/// Recedes everything already drawn: a foreground rewrite to the dimmest weight, never a fill.
fn scrim(area: Rect, theme: &Theme, buf: &mut Buffer) {
    let fg = Ink::Dimmer.color(theme);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(c) = buf.cell_mut((x, y)) {
                let s = c.style().remove_modifier(Modifier::BOLD).fg(fg);
                c.set_style(s);
            }
        }
    }
}

pub fn approval(area: Rect, state: &State, a: &Approval, theme: &Theme, buf: &mut Buffer) {
    scrim(area, theme, buf);
    let amber = ink(Ink::Amber, theme);
    let dim = ink(Ink::Dim, theme);
    let dimmer = ink(Ink::Dimmer, theme);
    let w: u16 = 78;
    let tw = (w - 4) as usize;

    let detail = a.detail_lines();
    let top = state.approval_scroll.min(a.detail_scroll_max());
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(text::clean(&a.tool), amber.add_modifier(Modifier::BOLD)),
        Span::styled("  wants to act", dim),
    ]));
    lines.push(Line::from(""));
    for l in text::wrap(&text::clean(&a.summary), tw) {
        lines.push(Line::from(Span::styled(l, theme.bright())));
    }
    if !detail.is_empty() {
        lines.push(Line::from(""));
        if top > 0 {
            lines.push(Line::from(Span::styled(format!("  ↑ {top} lines above"), dimmer)));
        }
        for l in detail.iter().skip(top).take(APPROVAL_DETAIL_ROWS) {
            lines.push(Line::from(vec![Span::styled("▏ ", dimmer), Span::styled(l.clone(), theme.normal())]));
        }
        let below = detail.len().saturating_sub(top + APPROVAL_DETAIL_ROWS);
        if below > 0 {
            lines.push(Line::from(Span::styled(format!("  ↓ {below} more lines"), dimmer)));
        }
    }
    if let Some(run) = &a.run {
        // A crew run asks: say which, so the request is read against its goal.
        let who = match state.crew.get(run) {
            Some(r) => format!("from the crew · {} · {}", text::clean(&r.role), text::clean(&r.title)),
            None => format!("from crew run {}", run.short()),
        };
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(text::truncate(&who, tw), ink(Ink::Accent, theme))));
    } else if state.session.as_ref() != Some(&a.session) {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("from another session", dimmer)));
    }
    lines.push(Line::from(""));
    let labels = ["allow", "allow for session", "deny"];
    let keys = ["y", "a", "n"];
    let mut choice = Vec::new();
    for (i, (label, key)) in labels.iter().zip(keys).enumerate() {
        let sel = i == state.approval_sel;
        let allowed = APPROVAL_CHOICES[i].is_allowed();
        let marker = if sel { "› " } else { "  " };
        let style = match (sel, allowed) {
            (_, true) if !state.approval_read => dimmer,
            (true, true) => ink(Ink::Accent, theme).add_modifier(Modifier::BOLD),
            (true, false) => ink(Ink::Red, theme).add_modifier(Modifier::BOLD),
            (false, _) => theme.normal(),
        };
        choice.push(Span::styled(marker, ink(Ink::Accent, theme)));
        choice.push(Span::styled(format!("{key} "), dimmer));
        choice.push(Span::styled(*label, style));
        choice.push(Span::raw("      "));
    }
    lines.push(Line::from(choice));
    if !state.approval_read {
        lines.push(Line::from(Span::styled("read the whole request (↓ / PgDn) to enable allow", amber)));
    }
    let more = state.approvals.len().saturating_sub(1);
    if more > 0 {
        lines.push(Line::from(Span::styled(format!("{more} more waiting"), dimmer)));
    }

    let r = centred(area, w, lines.len() as u16 + 2, false);
    let inner = panel(r, "approval", amber, true, buf);
    for (i, l) in lines.iter().enumerate().take(inner.height as usize) {
        put(buf, inner.x, inner.y + i as u16, l, inner.width);
    }
}

pub fn palette(area: Rect, query: &str, sel: usize, theme: &Theme, buf: &mut Buffer) {
    let matches = palette_matches(query);
    let h = matches.len().max(1) as u16 + 4;
    let r = centred(area, 60, h, true);
    let inner = panel(r, "palette", ink(Ink::Accent, theme), false, buf);
    let q = if query.is_empty() {
        Line::from(vec![Span::styled("› ", ink(Ink::Accent, theme)), Span::styled("type a command", ink(Ink::Dimmer, theme))])
    } else {
        Line::from(vec![Span::styled("› ", ink(Ink::Accent, theme)), Span::styled(text::clean(query), theme.normal())])
    };
    put(buf, inner.x, inner.y, &q, inner.width);
    if matches.is_empty() {
        put(buf, inner.x + 2, inner.y + 2, &Line::from(Span::styled("nothing matches", ink(Ink::Dim, theme))), inner.width);
        return;
    }
    for (i, cmd) in matches.iter().enumerate() {
        let selected = i == sel;
        let y = inner.y + 2 + i as u16;
        if y >= inner.bottom() {
            break;
        }
        let label_style = if selected { ink(Ink::Accent, theme).add_modifier(Modifier::BOLD) } else { theme.normal() };
        let marker = if selected { Span::styled("› ", ink(Ink::Accent, theme)) } else { Span::raw("  ") };
        let chord = cmd.chord();
        let gap = (inner.width as usize).saturating_sub(2 + text::width(cmd.label()) + text::width(chord));
        put(
            buf,
            inner.x,
            y,
            &Line::from(vec![
                marker,
                Span::styled(cmd.label(), label_style),
                Span::raw(" ".repeat(gap)),
                Span::styled(chord, ink(Ink::Dimmer, theme)),
            ]),
            inner.width,
        );
    }
}

pub fn sessions(area: Rect, state: &State, sel: usize, theme: &Theme, buf: &mut Buffer) {
    let n = state.sessions.as_ref().map_or(1, |s| s.len().max(1));
    let r = centred(area, 96, n as u16 + 4, true);
    let inner = panel(r, "sessions", ink(Ink::Accent, theme), false, buf);
    let dim = ink(Ink::Dim, theme);
    let dimmer = ink(Ink::Dimmer, theme);
    let Some(list) = &state.sessions else {
        put(buf, inner.x, inner.y, &Line::from(Span::styled("loading…", dim)), inner.width);
        return;
    };
    if list.is_empty() {
        put(buf, inner.x, inner.y, &Line::from(Span::styled("no sessions yet", dim)), inner.width);
        return;
    }
    let rows = inner.height.saturating_sub(1) as usize;
    let first = sel.saturating_sub(rows.saturating_sub(1));
    for (i, s) in list.iter().enumerate().skip(first).take(rows) {
        let selected = i == sel;
        let current = state.session.as_ref() == Some(&s.id);
        let marker = if selected { Span::styled("› ", ink(Ink::Accent, theme)) } else { Span::raw("  ") };
        let title_style = if selected { theme.bright() } else { theme.normal() };
        let title = if s.title.trim().is_empty() { "untitled".to_string() } else { text::clean(&s.title) };
        let ws = s.workspace.as_deref().map(text::short_path).unwrap_or_default();
        let mut spans = vec![
            marker,
            Span::styled(text::pad(&title, 46), title_style),
            Span::styled(text::pad(&text::truncate(&ws, 20), 22), dim),
            Span::styled(text::pad(&text::ago(s.updated_ms, state.now_ms), 11), dimmer),
            Span::styled(format!("{} turns", s.turns), dimmer),
        ];
        if current {
            spans.push(Span::styled("  open", ink(Ink::Speech, theme)));
        }
        put(buf, inner.x, inner.y + (i - first) as u16, &Line::from(spans), inner.width);
    }
}

pub fn model_picker(r: Rect, state: &State, sel: usize, theme: &Theme, buf: &mut Buffer) {
    let inner = panel(r, "conductor", ink(Ink::Accent, theme), false, buf);
    let current = state.library.conductor();
    for (i, m) in state.library.models.iter().enumerate() {
        let y = inner.y + i as u16;
        if y >= inner.bottom() {
            return;
        }
        let selected = i == sel;
        let marker = if selected { Span::styled("› ", ink(Ink::Accent, theme)) } else { Span::raw("  ") };
        let name_style = if selected { theme.bright() } else { theme.normal() };
        let mut spans = vec![
            marker,
            Span::styled(text::short_model(&m.model, Some(&m.info)), name_style),
            Span::styled(format!("  {}", m.model.provider), ink(Ink::Dimmer, theme)),
        ];
        if current.as_ref() == Some(&m.model) {
            spans.push(Span::styled("  ✓", ink(Ink::Speech, theme)));
        }
        put(buf, inner.x, y, &Line::from(spans), inner.width);
    }
    let y = inner.y + state.library.models.len() as u16;
    if y < inner.bottom() {
        let selected = sel >= state.library.models.len();
        let marker = if selected { Span::styled("› ", ink(Ink::Accent, theme)) } else { Span::raw("  ") };
        put(
            buf,
            inner.x,
            y,
            &Line::from(vec![marker, Span::styled("open the library…", ink(Ink::Dim, theme))]),
            inner.width,
        );
    }
}

/// The first-run offer to import the old harness memories.
pub fn import_offer(area: Rect, state: &State, sel: usize, theme: &Theme, buf: &mut Buffer) {
    let n = state.legacy.as_ref().map_or(0, |l| l.approx_entries);
    let msg = format!("I found memories from the old harness \u{2014} about {} entries. Import them?", text::thousands(n));
    let w: u16 = 62;
    let body = text::wrap(&msg, (w - 4) as usize);
    let r = centred(area, w, body.len() as u16 + 5, false);
    let inner = panel(r, "memories", ink(Ink::Accent, theme), false, buf);
    for (i, l) in body.iter().enumerate() {
        put(buf, inner.x, inner.y + i as u16, &Line::from(Span::styled(l.clone(), theme.normal())), inner.width);
    }
    let mut spans = Vec::new();
    for (i, (key, label)) in [("y", "Yes"), ("n", "Not now")].into_iter().enumerate() {
        let on = i == sel;
        let style = if on { ink(Ink::Accent, theme).add_modifier(Modifier::BOLD) } else { theme.normal() };
        spans.push(Span::styled(if on { "\u{203a} " } else { "  " }, ink(Ink::Accent, theme)));
        spans.push(Span::styled(format!("{key} "), ink(Ink::Dimmer, theme)));
        spans.push(Span::styled(label, style));
        spans.push(Span::raw("      "));
    }
    put(buf, inner.x, inner.y + body.len() as u16 + 1, &Line::from(spans), inner.width);
}

/// First-run onboarding. Returns where the cursor goes on the name step.
pub fn onboard(area: Rect, state: &State, theme: &Theme, buf: &mut Buffer) -> Option<(u16, u16)> {
    use crate::onboard::{address_choices, ItemKind, ItemState, Onboard, Step};
    let Onboard::Active { step, .. } = &state.onboard else { return None };
    let accent = ink(Ink::Accent, theme);
    let dim = ink(Ink::Dim, theme);
    let dimmer = ink(Ink::Dimmer, theme);
    let w: u16 = 72;
    let tw = (w - 4) as usize;
    let key = |k: &'static str, what: &'static str| [Span::styled(k, accent), Span::styled(what, dim)];
    let mut lines: Vec<Line> = Vec::new();
    let mut cursor_row = None;
    let hints: Vec<Span> = match step {
        Step::Name { input } => {
            lines.push(Line::from(Span::styled("What should I call you?", theme.bright())));
            lines.push(Line::from(""));
            let shown = text::clean(input.text());
            let field = if shown.is_empty() {
                Span::styled("your name", dimmer)
            } else {
                Span::styled(text::truncate(&shown, tw - 2), theme.normal())
            };
            cursor_row = Some((lines.len(), text::width(&text::clean(&input.text()[..input.cursor()])).min(tw - 2)));
            lines.push(Line::from(vec![Span::styled("› ", accent), field]));
            [key("↵", " next   "), key("esc", " skip setup")].concat()
        }
        Step::Address { name, sel } => {
            lines.push(Line::from(Span::styled(
                "In greetings: \u{201c}sir\u{201d}, your name, or nothing?",
                theme.bright(),
            )));
            lines.push(Line::from(""));
            let mut spans = Vec::new();
            for (i, f) in address_choices(name.as_deref()).into_iter().enumerate() {
                let label = match f {
                    marlowe_proto::AddressForm::Sir => "sir".to_string(),
                    marlowe_proto::AddressForm::Name => text::truncate(&text::clean(name.as_deref().unwrap_or("")), 24),
                    marlowe_proto::AddressForm::Nothing => "nothing".to_string(),
                };
                let on = i == *sel;
                spans.push(Span::styled(if on { "› " } else { "  " }, accent));
                spans.push(Span::styled(format!("{} ", i + 1), dimmer));
                spans.push(Span::styled(label, if on { accent.add_modifier(Modifier::BOLD) } else { theme.normal() }));
                spans.push(Span::raw("      "));
            }
            lines.push(Line::from(spans));
            [key("↵", " choose   "), key("← →", " move   "), key("⌫", " back   "), key("esc", " skip setup")].concat()
        }
        Step::Checklist { items, sel } => {
            lines.push(Line::from(Span::styled("A few next steps. Each one is optional.", theme.bright())));
            lines.push(Line::from(""));
            for (i, it) in items.iter().enumerate() {
                let on = i == *sel;
                let (glyph, gstyle) = match it.state {
                    ItemState::Open => ("○ ", dim),
                    ItemState::Done => ("✓ ", ink(Ink::Green, theme)),
                    ItemState::Skipped => ("– ", dimmer),
                };
                let (label, detail) = match &it.kind {
                    ItemKind::Import { approx_entries } => (
                        "Import memories from the old harness".to_string(),
                        format!("about {}", text::thousands(*approx_entries)),
                    ),
                    ItemKind::Watch(c) => (format!("Watch {}", text::short_path(&c.path)), text::clean(&c.reason)),
                    ItemKind::AddModel => ("Add a model".to_string(), "opens the library".to_string()),
                };
                let lstyle = match (on, it.state) {
                    (_, ItemState::Skipped) => dimmer,
                    (true, _) => accent.add_modifier(Modifier::BOLD),
                    (false, _) => theme.normal(),
                };
                let room = tw.saturating_sub(4);
                let label = text::truncate(&text::clean(&label), room);
                let rest = room.saturating_sub(text::width(&label) + 3);
                let mut spans = vec![
                    Span::styled(if on { "› " } else { "  " }, accent),
                    Span::styled(glyph, gstyle),
                    Span::styled(label, lstyle),
                ];
                if rest > 4 {
                    spans.push(Span::styled(format!(" · {}", text::truncate(&detail, rest)), dimmer));
                }
                lines.push(Line::from(spans));
            }
            if state.wake.candidates.is_none() && state.wake.sentinel != crate::wake::Presence::Absent {
                lines.push(Line::from(Span::styled("  looking for run folders…", dimmer)));
            }
            [key("↵", " do it   "), key("s", " skip   "), key("esc", " done")].concat()
        }
    };
    lines.push(Line::from(""));
    lines.push(Line::from(hints));

    let r = centred(area, w, lines.len() as u16 + 2, false);
    let inner = panel(r, "setup", accent, false, buf);
    for (i, l) in lines.iter().enumerate().take(inner.height as usize) {
        put(buf, inner.x, inner.y + i as u16, l, inner.width);
    }
    cursor_row.map(|(row, col)| (inner.x + 2 + col as u16, inner.y + row as u16))
}
