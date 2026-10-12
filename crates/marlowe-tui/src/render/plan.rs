//! A research plan waiting for the user: the main window's banner, the Run view's plan panel with
//! its approve / edit / reject buttons, and the plan editor.
//!
//! ui-preview5: the plan's only control was a dim `a approve plan` in the Run view's bottom border,
//! under the transcript, and the main window had nothing that lasted. Now the banner stays above
//! the composer while any plan waits, and the Run view puts the plan, its quote and its buttons
//! straight under the header, where they cannot scroll away.

use marlowe_proto::{CostQuote, CrewRunInfo, ResearchPlan};
use marlowe_surface::chrome::Ink;
use marlowe_surface::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use super::{hit, hovered, ink, lit, put, put_row};
use crate::crew::{plan_summary, PlanAction, PlanEdit, PlanRow, RunView};
use crate::state::{State, Target};
use crate::text;

/// Rows the main window's banner takes, under the conversation.
pub const BANNER_ROWS: u16 = 2;
/// Columns between two buttons.
const GAP: u16 = 3;

/// One button: its key in the accent, its label in the body weight. A button with no key (the
/// banner's, where keys type into the composer) is its label in the accent.
fn button(key: &str, label: &str, theme: &Theme) -> Line<'static> {
    let accent = ink(Ink::Accent, theme).add_modifier(Modifier::BOLD);
    if key.is_empty() {
        return Line::from(Span::styled(label.to_string(), accent));
    }
    Line::from(vec![Span::styled(key.to_string(), accent), Span::styled(format!(" {label}"), theme.normal())])
}

/// Width of a row of buttons, with their gaps.
fn buttons_width(items: &[(&str, &str, Target)], theme: &Theme) -> u16 {
    let w: usize = items.iter().map(|(k, l, _)| button(k, l, theme).width()).sum();
    (w + items.len().saturating_sub(1) * GAP as usize) as u16
}

/// Draws buttons left to right from `x`, each a click target, lit under the pointer.
fn buttons(state: &State, buf: &mut Buffer, at: (u16, u16, u16), items: &[(&str, &str, Target)], theme: &Theme) {
    let (mut x, y, right) = at;
    for (i, (k, label, t)) in items.iter().enumerate() {
        if i > 0 {
            x += GAP;
        }
        let l = button(k, label, theme);
        let w = l.width() as u16;
        if x + w > right {
            break;
        }
        hit(state, Rect { x, y, width: w, height: 1 }, t.clone());
        let l = if hovered(state, t) { lit(l, theme) } else { l };
        put(buf, x, y, &l, w);
        x += w;
    }
}

/// The main window's banner while a plan waits: who asks and for what, the quote, and buttons.
pub fn banner(r: Rect, state: &State, theme: &Theme, buf: &mut Buffer) {
    let Some((run, plan, quote)) = state.crew.waiting_plan() else { return };
    if r.height < BANNER_ROWS || r.width < 30 {
        return;
    }
    let (x, w) = (r.x + 2, r.width.saturating_sub(4));
    let amber = ink(Ink::Amber, theme);
    let mut head = vec![
        Span::styled("◐ ", amber),
        Span::styled("research plan waiting for your approval", amber.add_modifier(Modifier::BOLD)),
    ];
    if let Some(info) = state.crew.get(run) {
        head.push(Span::styled(" · ", ink(Ink::Dimmer, theme)));
        head.push(Span::styled(text::clean(&info.role), ink(Ink::Dim, theme)));
        head.push(Span::styled(" · ", ink(Ink::Dimmer, theme)));
        head.push(Span::styled(text::clean_maths(&info.title), theme.normal()));
    }
    let more = state.crew.plans.len().saturating_sub(1);
    if more > 0 {
        head.push(Span::styled(format!(" · +{more} more"), ink(Ink::Dimmer, theme)));
    }
    put(buf, x, r.y, &Line::from(head), w);

    let items = [
        ("alt-r", "review", Target::Plan(run.clone(), PlanAction::Review)),
        ("", "approve", Target::Plan(run.clone(), PlanAction::Approve)),
        ("", "edit", Target::Plan(run.clone(), PlanAction::Edit)),
        ("", "reject", Target::Plan(run.clone(), PlanAction::Reject)),
    ];
    let bw = buttons_width(&items, theme);
    let room = w.saturating_sub(bw + GAP) as usize;
    let summary = text::truncate(&format!("  {}", plan_summary(plan, quote)), room);
    let sw = text::width(&summary) as u16;
    put(buf, x, r.y + 1, &Line::from(Span::styled(summary, ink(Ink::Dim, theme))), sw);
    buttons(state, buf, (x + sw + GAP, r.y + 1, x + w), &items, theme);
}

/// Rows the Run view's plan panel wants at `width`, before any cap.
fn panel_lines(plan: &ResearchPlan, quote: &CostQuote, w: usize, max: usize, theme: &Theme) -> Vec<Line<'static>> {
    let dim = ink(Ink::Dim, theme);
    let dimmer = ink(Ink::Dimmer, theme);
    let amber = ink(Ink::Amber, theme);
    let head = Line::from(Span::styled("◐ Research plan — waiting for your approval", amber.add_modifier(Modifier::BOLD)));
    let mut tail: Vec<Line<'static>> = Vec::new();
    let recalled = if plan.recalled.is_empty() { String::new() } else { format!(" · {} memories", plan.recalled.len()) };
    tail.push(Line::from(Span::styled(
        format!("breadth {} · depth {} · up to {} sources{recalled}", plan.breadth, plan.depth, plan.source_budget),
        dim,
    )));
    if let Some(n) = plan.notes.as_deref().filter(|n| !n.trim().is_empty()) {
        for l in text::wrap(&text::clean(n.trim()), w).into_iter().take(2) {
            tail.push(Line::from(Span::styled(l, dimmer)));
        }
    }
    let basis = if quote.basis.trim().is_empty() { String::new() } else { format!(" · {}", text::clean(quote.basis.trim())) };
    tail.push(Line::from(Span::styled(text::truncate(&format!("quote: {}{basis}", plan_summary(plan, quote)), w), amber)));

    // Questions fill what the head and tail leave, two rows of text and one of channels each.
    let room = max.saturating_sub(1 + tail.len());
    let mut body: Vec<Line<'static>> = Vec::new();
    for (k, q) in plan.sub_questions.iter().enumerate() {
        let mut rows: Vec<Line<'static>> = Vec::new();
        let mut wrapped = text::wrap(&text::clean(&q.question), w.saturating_sub(5));
        if wrapped.len() > 2 {
            // Two rows of a long question, marked as cut; the editor shows it whole.
            wrapped.truncate(2);
            wrapped[1] = text::truncate(&format!("{} …", wrapped[1]), w.saturating_sub(5));
        }
        for (i, l) in wrapped.into_iter().enumerate() {
            let lead = if i == 0 { format!("{:>2}. ", q.id) } else { "    ".into() };
            rows.push(Line::from(vec![Span::styled(lead, dim), Span::styled(l, theme.normal())]));
        }
        let ch: Vec<&str> = q.channels.iter().map(|c| c.as_str()).collect();
        rows.push(Line::from(Span::styled(text::truncate(&format!("    {}", ch.join(" · ")), w), dimmer)));
        let left = plan.sub_questions.len() - k;
        let reserve = usize::from(left > 1);
        if body.len() + rows.len() + reserve > room {
            body.push(Line::from(Span::styled(format!("    +{left} more · e shows and edits them all"), dimmer)));
            break;
        }
        body.extend(rows);
    }
    let mut out = vec![head];
    out.extend(body);
    out.extend(tail);
    out
}

/// The Run view's plan panel, from the top of `r`: the plan, its quote, and the approve / edit /
/// reject buttons. Returns the rows it took, a blank one under the buttons included.
pub fn panel(r: Rect, state: &State, info: &CrewRunInfo, theme: &Theme, buf: &mut Buffer) -> u16 {
    let Some((plan, quote)) = state.crew.plan(&info.id) else { return 0 };
    if r.height < 4 {
        return 0;
    }
    let lines = panel_lines(plan, quote, r.width as usize, r.height.saturating_sub(2) as usize, theme);
    for (i, l) in lines.iter().enumerate() {
        put(buf, r.x, r.y + i as u16, l, r.width);
    }
    let y = r.y + lines.len() as u16;
    let run = info.id.clone();
    let items = [
        ("a", "approve", Target::Plan(run.clone(), PlanAction::Approve)),
        ("e", "edit", Target::Plan(run.clone(), PlanAction::Edit)),
        ("x", "reject", Target::Plan(run, PlanAction::Reject)),
    ];
    buttons(state, buf, (r.x, y, r.right()), &items, theme);
    lines.len() as u16 + 2
}

/// The plan editor, filling `r`: one row per sub-question, then add, breadth, depth and sources,
/// and the send / discard buttons. The typing cursor is left in `v.plan_cursor`.
pub fn editor(r: Rect, state: &State, v: &RunView, e: &PlanEdit, theme: &Theme, buf: &mut Buffer) {
    v.plan_cursor.set(None);
    if r.height < 6 {
        return;
    }
    let accent = ink(Ink::Accent, theme);
    let dim = ink(Ink::Dim, theme);
    let dimmer = ink(Ink::Dimmer, theme);
    let w = r.width as usize;
    put(
        buf,
        r.x,
        r.y,
        &Line::from(vec![
            Span::styled("Edit the research plan", accent.add_modifier(Modifier::BOLD)),
            Span::styled("  ·  sent as edited, and priced again", dimmer),
        ]),
        r.width,
    );
    // Rows that fit between the title and the buttons, scrolled to keep the selection on screen.
    let rows_h = r.height.saturating_sub(4) as usize;
    let first = e.sel.saturating_sub(rows_h.saturating_sub(1));
    for (n, i) in (first..e.rows()).take(rows_h).enumerate() {
        let y = r.y + 2 + n as u16;
        let sel = i == e.sel;
        let marker = if sel { Span::styled("› ", accent) } else { Span::raw("  ") };
        let line = match e.row(i) {
            PlanRow::Question(q) => {
                let lead = if e.plan.sub_questions.len() >= 10 { format!("{:>2}. ", q + 1) } else { format!("{}. ", q + 1) };
                let typing = e.typing.as_ref().filter(|_| sel);
                let body = match typing {
                    Some(c) => text::clean(c.text()),
                    None => text::clean(&e.plan.sub_questions[q].question),
                };
                let room = w.saturating_sub(2 + lead.len());
                if let Some(c) = typing {
                    let before = text::width(&text::clean(&c.text()[..c.cursor()]));
                    // A long question shows its end while it is typed.
                    let shown = text::truncate_left(&body, room.saturating_sub(1));
                    let cut = text::width(&body).saturating_sub(text::width(&shown));
                    let col = before.saturating_sub(cut).min(text::width(&shown));
                    v.plan_cursor.set(Some((r.x + 2 + lead.len() as u16 + col as u16, y)));
                    Line::from(vec![marker, Span::styled(lead, dim), Span::styled(shown, theme.bright())])
                } else {
                    let st = if sel { theme.bright() } else { theme.normal() };
                    Line::from(vec![marker, Span::styled(lead, dim), Span::styled(text::truncate(&body, room), st)])
                }
            }
            PlanRow::Add => Line::from(vec![marker, Span::styled("+ ", accent), Span::styled("add a question", dim)]),
            row => {
                let (label, value, why) = match row {
                    PlanRow::Breadth => ("breadth", e.plan.breadth, "searches per question"),
                    PlanRow::Depth => ("depth", e.plan.depth, "rounds of follow-up"),
                    _ => ("sources", e.plan.source_budget, "pages read at most"),
                };
                let arrows = if sel { accent } else { dimmer };
                Line::from(vec![
                    marker,
                    Span::styled(text::pad(label, 9), dim),
                    Span::styled("‹ ", arrows),
                    Span::styled(format!("{value:>3}"), if sel { theme.bright() } else { theme.normal() }),
                    Span::styled(" ›", arrows),
                    Span::styled(format!("  {why}"), dimmer),
                ])
            }
        };
        put_row(state, buf, (r.x, y, r.width), line, Target::PlanRow(i), theme);
    }
    let run = v.run.clone();
    let items = [
        ("s", "send", Target::Plan(run.clone(), PlanAction::Send)),
        ("esc", "discard edits", Target::Plan(run, PlanAction::Discard)),
    ];
    buttons(state, buf, (r.x, r.bottom() - 1, r.right()), &items, theme);
}
