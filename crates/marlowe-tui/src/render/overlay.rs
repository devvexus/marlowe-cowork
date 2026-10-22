//! Floating panels: the approval modal, the ^k palette, the sessions list, the model picker.
//! `Clear` resets cells to the terminal default; nothing here sets a background.

use marlowe_surface::chrome::Ink;
use marlowe_surface::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Widget};

use super::{hit, hovered, ink, put, put_row};
use crate::input::palette_matches;
use crate::state::{Approval, State, Target, APPROVAL_DETAIL_ROWS};
use crate::text;

/// `› ` on the cursor's row (accent) or under the pointer (hover tone).
pub(super) fn row_marker(selected: bool, over: bool, theme: &Theme) -> Span<'static> {
    match (selected, over) {
        (true, _) => Span::styled("› ", ink(Ink::Accent, theme)),
        (false, true) => Span::styled("› ", ink(Ink::Hover, theme)),
        (false, false) => Span::raw("  "),
    }
}

pub(super) fn centred(area: Rect, w: u16, h: u16, top_bias: bool) -> Rect {
    let w = w.min(area.width.saturating_sub(4));
    let h = h.min(area.height.saturating_sub(2));
    let y = if top_bias {
        area.y + area.height.saturating_sub(h) / 4
    } else {
        area.y + area.height.saturating_sub(h) / 2
    };
    Rect { x: area.x + area.width.saturating_sub(w) / 2, y, width: w, height: h }
}

/// Key hints on a panel's bottom border, at the right.
pub(super) fn panel_hints(r: Rect, items: &[(&str, &str)], theme: &Theme, buf: &mut Buffer) {
    let l = super::hints(items, theme);
    let l = Line::from([vec![Span::raw(" ")], l.spans, vec![Span::raw(" ")]].concat());
    let w = l.width() as u16;
    if w + 4 <= r.width {
        put(buf, r.right().saturating_sub(w + 2), r.bottom().saturating_sub(1), &l, w);
    }
}

pub(super) fn panel(r: Rect, title: &str, border: ratatui::style::Style, double: bool, buf: &mut Buffer) -> Rect {
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
pub(crate) fn scrim(area: Rect, theme: &Theme, buf: &mut Buffer) {
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

/// Draws the approval modal; the cursor is where the reason line is typed, when it is open.
pub fn approval(area: Rect, state: &State, a: &Approval, theme: &Theme, buf: &mut Buffer) -> Option<(u16, u16)> {
    scrim(area, theme, buf);
    let amber = ink(Ink::Amber, theme);
    let dim = ink(Ink::Dim, theme);
    let dimmer = ink(Ink::Dimmer, theme);
    let w: u16 = 78;
    let tw = (w - 4) as usize;
    // A server restart is allowed once or denied: never "for this session".
    let once_only = a.tool == marlowe_proto::SERVER_RESTART_TOOL;

    // An edit shows as the diff it makes; anything else as its arguments.
    let body: Vec<Line<'static>> = match &a.diff {
        Some(d) => {
            let mut v: Vec<Line<'static>> = a.extra_lines().into_iter().map(|l| Line::from(Span::styled(l, amber))).collect();
            v.extend(crate::diff::render(d, crate::diff::Layout::full(crate::state::APPROVAL_DETAIL_WIDTH), theme).lines);
            v
        }
        None => a.detail_lines().into_iter().map(|l| Line::from(Span::styled(l, theme.normal()))).collect(),
    };
    let top = state.approval_scroll.min(a.detail_scroll_max());
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(text::clean(&a.tool), amber.add_modifier(Modifier::BOLD)),
        Span::styled(if a.tool == "exit_plan" { "  has a plan" } else { "  wants to act" }, dim),
    ]));
    lines.push(Line::from(""));
    for l in text::wrap(&text::clean(&a.summary), tw) {
        lines.push(Line::from(Span::styled(l, theme.bright())));
    }
    if !body.is_empty() {
        lines.push(Line::from(""));
        if top > 0 {
            lines.push(Line::from(Span::styled(format!("  ↑ {top} lines above"), dimmer)));
        }
        for l in body.iter().skip(top).take(APPROVAL_DETAIL_ROWS) {
            let mut spans = vec![Span::styled("▏ ", dimmer)];
            spans.extend(l.spans.iter().cloned());
            lines.push(Line::from(spans));
        }
        let below = body.len().saturating_sub(top + APPROVAL_DETAIL_ROWS);
        if below > 0 {
            lines.push(Line::from(Span::styled(format!("  ↓ {below} more lines"), dimmer)));
        }
    }
    if let Some(run) = &a.run {
        // A crew run asks: say which, so the request is read against its goal.
        let who = match state.crew.get(run) {
            Some(r) => format!("from the crew · {} · {}", text::clean(&r.role), text::clean_maths(&r.title)),
            None => format!("from crew run {}", run.short()),
        };
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(text::truncate(&who, tw), ink(Ink::Accent, theme))));
    } else if once_only {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("from the context planner · every Ollama model is affected", amber)));
    } else if state.session.as_ref() != Some(&a.session) {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("from another session", dimmer)));
    }
    lines.push(Line::from(""));
    // With a proposed rule: allow once, allow the rule for the session, allow it for the project.
    let choices = a.choices();
    if let (Some(rule), 4) = (&a.proposed_rule, choices.len()) {
        lines.push(Line::from(vec![
            Span::styled("always allow  ", dimmer),
            Span::styled(text::truncate(&text::clean(rule), tw.saturating_sub(14)), ink(Ink::Accent, theme)),
        ]));
        lines.push(Line::from(""));
    }
    let (labels, keys): (&[&str], &[&str]) = if choices.len() == 4 {
        (&["allow", "always · session", "always · project", "deny"], &["y", "a", "p", "n"])
    } else if choices.len() == 2 {
        (&["approve the plan", "keep planning"], &["y", "n"])
    } else {
        (&["allow", "allow for session", "deny"], &["y", "a", "n"])
    };
    let gap = if choices.len() == 4 { 4 } else { 6 };
    let mut choice = Vec::new();
    // Each button's column span in the row, for the mouse.
    let mut buttons: Vec<(usize, u16, u16)> = Vec::new();
    let mut x = 0u16;
    for (i, (label, key)) in labels.iter().zip(keys).enumerate() {
        if once_only && i == 1 {
            continue;
        }
        let sel = i == state.approval_sel;
        let allowed = choices[i].is_allowed();
        let over = hovered(state, &Target::Button(i)) && !(allowed && !state.approval_read);
        let marker = if sel || over { "› " } else { "  " };
        let marker_style = if sel { ink(Ink::Accent, theme) } else { ink(Ink::Hover, theme) };
        let style = match (sel, allowed) {
            (_, true) if !state.approval_read => dimmer,
            (true, true) => ink(Ink::Accent, theme).add_modifier(Modifier::BOLD),
            (true, false) => ink(Ink::Red, theme).add_modifier(Modifier::BOLD),
            (false, _) if over => theme.bright(),
            (false, _) => theme.normal(),
        };
        let w = (2 + key.len() + 1 + text::width(label)) as u16;
        buttons.push((i, x, w));
        x += w + gap;
        choice.push(Span::styled(marker, marker_style));
        choice.push(Span::styled(format!("{key} "), dimmer));
        choice.push(Span::styled(*label, style));
        choice.push(Span::raw(" ".repeat(gap as usize)));
    }
    let buttons_row = lines.len();
    lines.push(Line::from(choice));
    // Under the buttons: deny and say why. Opening it turns the row into a one-line input.
    let why_row = lines.len();
    let why_sel = state.approval_sel == a.why_index();
    let why_over = hovered(state, &Target::Button(a.why_index()));
    let mut cursor_at: Option<(usize, usize)> = None;
    if let Some(r) = &state.approval_reason {
        let shown = text::clean(r.text());
        let room = tw.saturating_sub(24);
        let before = text::clean(&r.text()[..r.cursor()]);
        let col = text::width(&before).min(room);
        // A long reason shows its tail, where the cursor is.
        let tail: String = {
            let w = text::width(&shown);
            if w > room { shown.chars().skip(shown.chars().count().saturating_sub(room)).collect() } else { shown.clone() }
        };
        cursor_at = Some((col, lines.len()));
        lines.push(Line::from(vec![
            Span::styled("› ", ink(Ink::Accent, theme)),
            Span::styled("tell Marlowe why  ", dimmer),
            Span::styled(tail, theme.normal()),
        ]));
        lines.push(Line::from(Span::styled("↵ deny with this reason · esc back", dimmer)));
    } else {
        let style = match (why_sel, why_over) {
            (true, _) => ink(Ink::Red, theme).add_modifier(Modifier::BOLD),
            (false, true) => theme.bright(),
            (false, false) => theme.normal(),
        };
        lines.push(Line::from(vec![
            Span::styled(if why_sel || why_over { "› " } else { "  " }, if why_sel { ink(Ink::Accent, theme) } else { ink(Ink::Hover, theme) }),
            Span::styled("w ", dimmer),
            Span::styled("deny, and tell Marlowe why", style),
        ]));
    }
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
    if buttons_row < inner.height as usize {
        for (i, bx, bw) in buttons {
            let rect = Rect { x: inner.x + bx, y: inner.y + buttons_row as u16, width: bw, height: 1 };
            hit(state, rect.intersection(inner), Target::Button(i));
        }
    }
    if state.approval_reason.is_none() && why_row < inner.height as usize {
        let rect = Rect { x: inner.x, y: inner.y + why_row as u16, width: 30.min(inner.width), height: 1 };
        hit(state, rect, Target::Button(a.why_index()));
    }
    cursor_at.filter(|(_, row)| *row < inner.height as usize).map(|(col, row)| (inner.x + 2 + 18 + col as u16, inner.y + row as u16))
}

pub fn palette(area: Rect, state: &State, query: &str, sel: usize, theme: &Theme, buf: &mut Buffer) {
    let matches = palette_matches(query);
    let h = matches.len().max(1) as u16 + 4;
    let r = centred(area, 60, h, true);
    let inner = panel(r, "palette", ink(Ink::Accent, theme), false, buf);
    panel_hints(r, &[("↑↓", "select"), ("↵", "run"), ("esc", "close")], theme, buf);
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
        let marker = row_marker(selected, hovered(state, &Target::Row(i)), theme);
        let chord = cmd.chord();
        let gap = (inner.width as usize).saturating_sub(2 + text::width(cmd.label()) + text::width(chord));
        let line = Line::from(vec![
            marker,
            Span::styled(cmd.label(), label_style),
            Span::raw(" ".repeat(gap)),
            Span::styled(chord, ink(Ink::Dimmer, theme)),
        ]);
        put_row(state, buf, (inner.x, y, inner.width), line, Target::Row(i), theme);
    }
}

pub fn sessions(area: Rect, state: &State, sel: usize, theme: &Theme, buf: &mut Buffer) {
    let list = state.session_rows();
    let n = list.len().max(1);
    let r = centred(area, 96, n as u16 + 5, true);
    let inner = panel(r, "sessions", ink(Ink::Accent, theme), false, buf);
    panel_hints(r, &[("type", "to search"), ("↑↓", "select"), ("↵", "open"), ("esc", "close")], theme, buf);
    let dim = ink(Ink::Dim, theme);
    let dimmer = ink(Ink::Dimmer, theme);
    let q = &state.code.sessions_query;
    let field = if q.is_empty() { Span::styled("search sessions", dimmer) } else { Span::styled(text::clean(q), theme.normal()) };
    put(buf, inner.x, inner.y, &Line::from(vec![Span::styled("› ", ink(Ink::Accent, theme)), field]), inner.width);
    let top = inner.y + 2;
    if state.sessions.is_none() {
        put(buf, inner.x, top, &Line::from(Span::styled("loading…", dim)), inner.width);
        return;
    }
    if list.is_empty() {
        let msg = if q.is_empty() { "no sessions yet" } else { "no session matches" };
        put(buf, inner.x, top, &Line::from(Span::styled(msg, dim)), inner.width);
        return;
    }
    let rows = inner.height.saturating_sub(2) as usize;
    let first = sel.saturating_sub(rows.saturating_sub(1));
    for (i, s) in list.iter().enumerate().skip(first).take(rows) {
        let selected = i == sel;
        let current = state.session.as_ref() == Some(&s.id);
        let marker = row_marker(selected, hovered(state, &Target::Row(i)), theme);
        let title_style = if selected { theme.bright() } else { theme.normal() };
        let title = if s.title.trim().is_empty() { "untitled".to_string() } else { text::clean_maths(&s.title) };
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
        put_row(state, buf, (inner.x, top + (i - first) as u16, inner.width), Line::from(spans), Target::Row(i), theme);
    }
}

pub fn model_picker(r: Rect, state: &State, sel: usize, theme: &Theme, buf: &mut Buffer) {
    let inner = panel(r, "conductor", ink(Ink::Accent, theme), false, buf);
    panel_hints(r, &[("↵", "choose"), ("c", "context"), ("esc/⌫", "close")], theme, buf);
    let current = state.library.conductor();
    for (i, m) in state.library.models.iter().enumerate() {
        let y = inner.y + i as u16;
        if y >= inner.bottom() {
            return;
        }
        let selected = i == sel;
        let marker = row_marker(selected, hovered(state, &Target::Row(i)), theme);
        let name_style = if selected { theme.bright() } else { theme.normal() };
        let mut spans = vec![
            marker,
            Span::styled(text::short_model(&m.model, Some(&m.info)), name_style),
            Span::styled(format!("  {}", m.model.provider), ink(Ink::Dimmer, theme)),
        ];
        if current.as_ref() == Some(&m.model) {
            spans.push(Span::styled("  ✓", ink(Ink::Speech, theme)));
        }
        // Each model's window, right-aligned: what its agents get at most.
        let window = state.context.model(&m.model).map(|c| c.max_ctx).or(m.info.context_window);
        if let Some(wn) = window {
            let label = marlowe_proto::ctx_label(wn);
            let used = Line::from(spans.clone()).width();
            let gap = (inner.width as usize).saturating_sub(used + text::width(&label));
            if gap >= 2 {
                spans.push(Span::raw(" ".repeat(gap)));
                spans.push(Span::styled(label, ink(Ink::Dim, theme)));
            }
        }
        put_row(state, buf, (inner.x, y, inner.width), Line::from(spans), Target::Row(i), theme);
    }
    let n = state.library.models.len();
    let y = inner.y + n as u16;
    if y < inner.bottom() {
        let selected = sel >= n;
        let marker = row_marker(selected, hovered(state, &Target::Row(n)), theme);
        let line = Line::from(vec![marker, Span::styled("open the library…", ink(Ink::Dim, theme))]);
        put_row(state, buf, (inner.x, y, inner.width), line, Target::Row(n), theme);
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
    let y = inner.y + body.len() as u16 + 1;
    let mut x = inner.x;
    for (i, (key, label)) in [("y", "Yes"), ("n", "Not now")].into_iter().enumerate() {
        let on = i == sel;
        let over = hovered(state, &Target::Button(i));
        let style = match (on, over) {
            (true, _) => ink(Ink::Accent, theme).add_modifier(Modifier::BOLD),
            (false, true) => theme.bright(),
            (false, false) => theme.normal(),
        };
        spans.push(row_marker(on, over, theme));
        spans.push(Span::styled(format!("{key} "), ink(Ink::Dimmer, theme)));
        spans.push(Span::styled(label, style));
        spans.push(Span::raw("      "));
        let w = (2 + key.len() + 1 + label.len()) as u16;
        if y < inner.bottom() {
            hit(state, Rect { x, y, width: w, height: 1 }.intersection(inner), Target::Button(i));
        }
        x += w + 6;
    }
    put(buf, inner.x, y, &Line::from(spans), inner.width);
}

/// A question with buttons: quitting with work running, or resuming what a close interrupted.
pub fn choice(area: Rect, state: &State, sel: usize, theme: &Theme, buf: &mut Buffer) {
    let Some(dialog) = state.closing.dialog.as_ref() else { return };
    if matches!(dialog, crate::closing::Dialog::Quit { .. }) {
        quit_dialog(area, state, sel, theme, buf);
        return;
    }
    let w: u16 = 70;
    let body = text::wrap(&dialog.prompt(), (w - 4) as usize);
    let r = centred(area, w, body.len() as u16 + 5, false);
    let inner = panel(r, dialog.title(), ink(Ink::Accent, theme), false, buf);
    for (i, l) in body.iter().enumerate() {
        put(buf, inner.x, inner.y + i as u16, &Line::from(Span::styled(l.clone(), theme.normal())), inner.width);
    }
    let mut spans = Vec::new();
    let y = inner.y + body.len() as u16 + 1;
    let mut x = inner.x;
    for (i, (key, label)) in dialog.choices().iter().enumerate() {
        let on = i == sel;
        let over = hovered(state, &Target::ChoiceButton(i));
        let style = match (on, over) {
            (true, _) => ink(Ink::Accent, theme).add_modifier(Modifier::BOLD),
            (false, true) => theme.bright(),
            (false, false) => theme.normal(),
        };
        spans.push(row_marker(on, over, theme));
        spans.push(Span::styled(format!("{key} "), ink(Ink::Dimmer, theme)));
        spans.push(Span::styled(*label, style));
        spans.push(Span::raw("      "));
        let w = (2 + key.len() + 1 + label.len()) as u16;
        if y < inner.bottom() {
            hit(state, Rect { x, y, width: w, height: 1 }.intersection(inner), Target::ChoiceButton(i));
        }
        x += w + 6;
    }
    put(buf, inner.x, y, &Line::from(spans), inner.width);
}

/// The quit modal: what is running, three buttons one under the other, and "Remember my choice".
fn quit_dialog(area: Rect, state: &State, sel: usize, theme: &Theme, buf: &mut Buffer) {
    let Some(dialog) = state.closing.dialog.as_ref() else { return };
    let w: u16 = 78;
    let body = text::wrap(&dialog.prompt(), (w - 4) as usize);
    let n = dialog.choices().len() as u16;
    let r = centred(area, w, body.len() as u16 + n * 2 + 5, false);
    let inner = panel(r, dialog.title(), ink(Ink::Accent, theme), false, buf);
    panel_hints(r, &[("\u{21b5}", "choose"), ("r", "remember"), ("esc", "cancel")], theme, buf);
    for (i, l) in body.iter().enumerate() {
        put(buf, inner.x, inner.y + i as u16, &Line::from(Span::styled(l.clone(), theme.normal())), inner.width);
    }
    let mut y = inner.y + body.len() as u16 + 1;
    for (i, (key, label)) in dialog.choices().iter().enumerate() {
        let on = i == sel;
        let over = hovered(state, &Target::ChoiceButton(i));
        let style = match (on, over) {
            (true, _) => ink(Ink::Accent, theme).add_modifier(Modifier::BOLD),
            (false, true) => theme.bright(),
            (false, false) => theme.normal(),
        };
        let line = Line::from(vec![row_marker(on, over, theme), Span::styled(format!("{key} "), ink(Ink::Dimmer, theme)), Span::styled(*label, style)]);
        hit(state, Rect { x: inner.x, y, width: inner.width, height: 1 }, Target::ChoiceButton(i));
        put(buf, inner.x, y, &line, inner.width);
        if let Some(help) = crate::closing::QUIT_HELP.get(i) {
            put(buf, inner.x + 6, y + 1, &Line::from(Span::styled(*help, ink(Ink::Dimmer, theme))), inner.width.saturating_sub(6));
        }
        y += 2;
    }
    let remember = state.closing.remember;
    let over = hovered(state, &Target::Remember);
    let st = if over { theme.bright() } else { theme.normal() };
    let line = Line::from(vec![
        Span::raw("  "),
        Span::styled(if remember { "[x] " } else { "[ ] " }, ink(if remember { Ink::Accent } else { Ink::Dim }, theme)),
        Span::styled("Remember my choice", st),
        Span::styled("  (the window's X then does the same)", ink(Ink::Dimmer, theme)),
    ]);
    hit(state, Rect { x: inner.x, y, width: inner.width, height: 1 }, Target::Remember);
    put(buf, inner.x, y, &line, inner.width);
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

/// The workspace dropdown under the Workspace box: a path field, then the known directories.
/// Returns the cursor position in the field.
pub fn workspace_picker(r: Rect, state: &State, query: &str, sel: usize, theme: &Theme, buf: &mut Buffer) -> Option<(u16, u16)> {
    let inner = panel(r, "workspace", ink(Ink::Accent, theme), false, buf);
    if inner.height == 0 {
        return None;
    }
    let dim = ink(Ink::Dim, theme);
    let dimmer = ink(Ink::Dimmer, theme);
    let field = if query.is_empty() {
        Span::styled("type a path, or pick one", dimmer)
    } else {
        Span::styled(text::clean(query), theme.normal())
    };
    put(buf, inner.x, inner.y, &Line::from(vec![Span::styled("› ", ink(Ink::Accent, theme)), field]), inner.width);
    // Why Enter was refused, on the line under the field.
    if let Some(why) = &state.wspick.note {
        // Long paths lose their start: the reason is at the end.
        let room = (inner.width as usize).saturating_sub(2);
        let line = Line::from(Span::styled(text::truncate_left(&text::clean(why), room), ink(Ink::Amber, theme)));
        put(buf, inner.x + 2, inner.y + 1, &line, inner.width.saturating_sub(2));
    }
    let choices = state.workspace_choices(query);
    if choices.is_empty() {
        put(buf, inner.x + 2, inner.y + 2, &Line::from(Span::styled("nothing matches — type a full path", dim)), inner.width);
    }
    let roots: Vec<String> = state
        .wake
        .watch
        .iter()
        .flat_map(|w| w.roots.iter().map(|r| crate::wake::norm(&r.path)))
        .collect();
    let current = state.shown_workspace().map(crate::wake::norm);
    for (i, p) in choices.iter().enumerate() {
        let y = inner.y + 2 + i as u16;
        if y >= inner.bottom() {
            break;
        }
        let n = crate::wake::norm(p);
        let tag = if current.as_deref() == Some(n.as_str()) {
            "current"
        } else if roots.contains(&n) {
            "watched"
        } else if state.launch_workspace.as_deref().map(crate::wake::norm).as_deref() == Some(n.as_str()) {
            "launch dir"
        } else {
            ""
        };
        let on = i == sel;
        let room = (inner.width as usize).saturating_sub(4 + text::width(tag));
        let mut spans = vec![
            row_marker(on, hovered(state, &Target::Row(i)), theme),
            Span::styled(
                text::truncate_left(&text::short_path(&text::clean(p)), room),
                if on { theme.bright() } else { theme.normal() },
            ),
        ];
        if !tag.is_empty() {
            spans.push(Span::styled(format!("  {tag}"), dimmer));
        }
        put_row(state, buf, (inner.x, y, inner.width), Line::from(spans), Target::Row(i), theme);
    }
    panel_hints(r, &[("↵", "choose"), ("esc", "close")], theme, buf);
    let col = text::width(&text::clean(query)).min(inner.width.saturating_sub(3) as usize) as u16;
    Some((inner.x + 2 + col, inner.y))
}
