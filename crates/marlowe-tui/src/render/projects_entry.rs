//! A project in the conversation: one entry that narrates the run in the boot sequence's voice
//! (the plan card, tick lines while it runs, the checkin card, the result), and the board's rows.
//!
//! The entry is drawn from the generic view-model and `marlowe_project::derive` only. A rule `▏`
//! on its left is amber while it needs the user and dimmer once settled; there is no box and no
//! fill. While the project runs the entry is the last thing in the transcript (above the live
//! strip); when it ends it settles where it ended.

use std::hash::{Hash, Hasher};

use marlowe_project::state::*;
use marlowe_project::view::{self, Cell};
use marlowe_project::derive;
use marlowe_surface::Theme;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::projects::*;
use crate::projects::{Loaded, ProjTarget};
use crate::state::{Entry, State, Target};
use crate::text;

/// The name column of a tick line, like the boot sequence's `memory ·····`.
const LEAD: usize = 18;
/// A conversation pane shorter than this folds a running entry to two rows.
const SHORT_PANE: u16 = 20;

/// `{glyph}  {name} ····· ` with the leader column `lead` cells wide.
fn tick_lead(l: &mut L, p: &Pal, glyph: (&str, Style), name: &str, name_style: Style, lead: usize) {
    l.s(glyph.0, glyph.1);
    l.sp(2);
    l.s(name, name_style);
    l.sp(1);
    l.s("·".repeat(lead.saturating_sub(text::width(name) + 1).max(3)), p.dimmer);
    l.sp(1);
}

// ---- briefs: a step on one line ---------------------------------------------------------------------

/// `fetch 40 (✗3 –2)`: the tasks done, running ●, waiting ○, failed ✗, then the step's items that
/// did not end done.
struct Brief {
    id: String,
    status: StageStatus,
    kind: StepKind,
    c: Counts,
    failed_items: u32,
    skipped_items: u32,
    /// A gate step says what was answered, or that it waits.
    note: Option<String>,
    /// Its tasks held back from calling a model; the step's `running` count includes them.
    held: u32,
}

fn briefs(cx: &Cx, ph: &Phase) -> Vec<Brief> {
    ph.steps
        .iter()
        .map(|st| {
            let (mut f, mut k) = (0, 0);
            for i in cx.s().items.values().filter(|i| i.phase == ph.id && i.step == st.id) {
                match i.status {
                    ItemStatus::Failed => f += 1,
                    ItemStatus::Skipped => k += 1,
                    _ => {}
                }
            }
            let note = (st.kind == StepKind::Gate).then(|| match cx.s().gates.iter().find(|g| g.phase == ph.id && g.step == st.id) {
                Some(g) if g.status == GateStatus::Open => "waiting for you".to_string(),
                Some(g) => g.answer.as_ref().map(|a| format!("answered {}", a.choice.replace('_', " "))).unwrap_or_default(),
                None => String::new(),
            });
            let held = derive::waiting_agents(cx.s())
                .into_iter()
                .filter(|a| a.step == st.id && a.item.as_deref().and_then(|i| cx.s().items.get(i)).is_none_or(|i| i.phase == ph.id))
                .count() as u32;
            Brief { id: st.id.clone(), status: st.status, kind: st.kind, c: st.counts, failed_items: f, skipped_items: k, note, held }
        })
        .collect()
}

fn brief_spans(p: &Pal, b: &Brief) -> Vec<Span<'static>> {
    let mut v = Vec::new();
    if let Some(note) = &b.note {
        v.push(Span::styled(format!("{} ", b.id.replace('_', " ")), p.dim));
        v.push(Span::styled(note.clone(), if b.status == StageStatus::Running || note == "waiting for you" { p.amber } else { p.body }));
        return v;
    }
    let total = b.c.queued + b.c.running + b.c.done + b.c.failed;
    if total == 0 {
        v.push(Span::styled(b.id.clone(), p.dim));
        return v;
    }
    v.push(Span::styled(format!("{} ", b.id), p.dim));
    v.push(Span::styled(b.c.done.to_string(), p.body));
    let calling = b.c.running.saturating_sub(b.held);
    if calling > 0 {
        v.push(Span::styled(format!(" ●{calling}"), p.info));
    }
    if b.held > 0 {
        v.push(Span::styled(format!(" ◌{}", b.held), p.amber));
    }
    if b.c.queued > 0 {
        v.push(Span::styled(format!(" ○{}", b.c.queued), p.dim));
    }
    if b.c.failed > 0 {
        v.push(Span::styled(format!(" ✗{}", b.c.failed), p.red));
    }
    let mut extra: Vec<(String, Style)> = Vec::new();
    if b.failed_items > b.c.failed {
        extra.push((format!("✗{}", b.failed_items - b.c.failed), p.red));
    }
    if b.skipped_items > 0 {
        extra.push((format!("–{}", b.skipped_items), p.dimmer));
    }
    if !extra.is_empty() && b.kind != StepKind::Merge {
        v.push(Span::styled(" (", p.dimmer));
        for (i, (t, st)) in extra.into_iter().enumerate() {
            if i > 0 {
                v.push(Span::raw(" "));
            }
            v.push(Span::styled(t, st));
        }
        v.push(Span::styled(")", p.dimmer));
    }
    v
}

/// The steps of a phase on one line; finished steps fold first, then waiting ones.
fn phase_briefs(cx: &Cx, ph: &Phase, width: usize) -> Vec<Span<'static>> {
    let p = &cx.pal;
    let bs = briefs(cx, ph);
    let join = |parts: Vec<Vec<Span<'static>>>| -> Vec<Span<'static>> {
        let mut v = Vec::new();
        for (i, part) in parts.into_iter().enumerate() {
            if i > 0 {
                v.push(Span::styled(" · ", p.dimmer));
            }
            v.extend(part);
        }
        v
    };
    let spans_w = |v: &[Span<'static>]| v.iter().map(|s| text::width(&s.content)).sum::<usize>();
    // A step that has not started never shows by name: the steps still to come count as `n waiting`.
    let not_started = |b: &Brief| b.note.is_none() && b.status == StageStatus::Pending && b.c.queued + b.c.running + b.c.done + b.c.failed == 0;
    let mut shown: Vec<Vec<Span<'static>>> = bs.iter().filter(|b| !not_started(b)).map(|b| brief_spans(p, b)).collect();
    let waiting_n = bs.iter().filter(|b| not_started(b)).count();
    if waiting_n > 0 {
        shown.push(vec![Span::styled(format!("{waiting_n} waiting"), p.dim)]);
    }
    let full = join(shown);
    if spans_w(&full) <= width {
        return full;
    }
    let done: Vec<&Brief> = bs.iter().filter(|b| b.status == StageStatus::Done).collect();
    let live: Vec<&Brief> = bs.iter().filter(|b| b.status == StageStatus::Running).collect();
    let wait = bs.iter().filter(|b| !matches!(b.status, StageStatus::Done | StageStatus::Running)).count();
    let mut parts: Vec<Vec<Span<'static>>> = Vec::new();
    if !done.is_empty() {
        // Each finished step once, with the items it left unfinished right after its name.
        let mut v = vec![Span::styled("✓ ", p.green)];
        for (i, b) in done.iter().enumerate() {
            if i > 0 {
                v.push(Span::raw(" "));
            }
            v.push(Span::styled(b.id.clone(), p.dim));
            let sp = brief_spans(p, b);
            if let Some(k) = sp.iter().position(|s| s.content.starts_with(" (")) {
                v.extend(sp[k..].iter().cloned());
            }
        }
        parts.push(v);
    }
    for b in live {
        parts.push(brief_spans(p, b));
    }
    if wait > 0 {
        parts.push(vec![Span::styled(format!("{wait} waiting"), p.dim)]);
    }
    let s = join(parts.clone());
    if spans_w(&s) <= width {
        return s;
    }
    if !done.is_empty() {
        parts[0] = vec![Span::styled(format!("✓{}", done.len()), p.green)];
    }
    let mut l = L::new();
    l.spans(join(parts));
    l.clip(width);
    l.done().line.spans
}

// ---- the entry -------------------------------------------------------------------------------------

fn rule(p: &Pal, amber: bool) -> Span<'static> {
    Span::styled("▏ ", if amber { p.amber } else { p.dimmer })
}

/// A row under the rule.
fn with_rule(p: &Pal, amber: bool, l: L) -> BLine {
    let inner = l.done();
    let mut b = L::new();
    b.spans(vec![rule(p, amber)]);
    b.spans(inner.line.spans);
    let mut out = b.done();
    out.hits = inner.hits.into_iter().map(|(x, w, t)| (x + 2, w, t)).collect();
    out
}

fn text_row(p: &Pal, amber: bool, s: impl AsRef<str>, st: Style) -> BLine {
    let mut l = L::new();
    l.s(s, st);
    with_rule(p, amber, l)
}

fn button(cx: &Cx, l: &mut L, key: &str, label: &str, t: ProjTarget) {
    let p = &cx.pal;
    let from = l.width();
    if cx.focused && !key.is_empty() {
        l.s(key, p.accent_b);
        l.sp(1);
    }
    l.s(label, if cx.hot(&t) { p.accent_b } else if cx.focused { p.body } else { p.accent });
    l.hit(from, t);
}

/// The entry's rows for a width. The same rows serve the transcript cache and the click map.
pub fn entry_block(cx: &Cx, width: usize) -> Vec<BLine> {
    let mut rows = match cx.s().status {
        ProjectStatus::AwaitingPlan => plan_entry(cx, width),
        ProjectStatus::AwaitingCheckin => checkin_entry(cx, width),
        ProjectStatus::Running => running_entry(cx, width),
        _ => finished_entry(cx, width),
    };
    // Under the header every row can be clicked to give the entry the keys.
    let id = cx.id();
    for r in rows.iter_mut().filter(|r| r.hits.is_empty()) {
        let w = r.line.width() as u16;
        if w > 0 {
            r.hits.push((0, w, ProjTarget::Entry(id.clone())));
        }
    }
    rows
}

/// The line under the buttons while an answer is on its way.
fn answering_row(cx: &Cx) -> Option<BLine> {
    let a = cx.l.answering(cx.state.now_ms)?;
    let mut l = L::new();
    l.s(if a.choice.is_empty() { "answered · the project goes on" } else { "sending your answer…" }, cx.pal.dim);
    Some(l.done().indented(2))
}

fn plan_entry(cx: &Cx, width: usize) -> Vec<BLine> {
    let p = &cx.pal;
    let s = cx.s();
    let mut out = Vec::new();
    let Some(f) = plan_facts(cx) else { return running_entry(cx, width) };
    let mut h = L::new();
    h.s(format!("◐ plan {} · {} · {} · ", f.no, s.kind, s.id), p.amber);
    h.s("waiting for you", p.amber);
    let asked = view::format_relative(cx.l.open_gate().map_or(0, |g| g.asked_ms), cx.now);
    lr(&mut h, width, vec![Span::styled(format!("asked {asked}"), p.dim)]);
    h.hit(0, ProjTarget::Entry(cx.id()));
    out.push(h.done());
    let w = width.saturating_sub(2);
    for l in wrap_clip(&f.objective, w, 2) {
        out.push(text_row(p, true, l, p.body));
    }
    for (id, q) in &f.questions {
        let mut l = L::new();
        l.s(format!("{id} "), p.accent);
        l.s(text::truncate(q, w.saturating_sub(id.len() + 1)), p.body);
        out.push(with_rule(p, true, l));
    }
    if !f.fields.is_empty() {
        let mut l = L::new();
        l.s("fields  ", p.dim);
        l.s(text::truncate(&f.fields.join(" · "), w.saturating_sub(8)), p.body);
        out.push(with_rule(p, true, l));
    }
    if !f.limits.is_empty() {
        let mut l = L::new();
        l.s("limits  ", p.dim);
        l.s(text::truncate(&f.limits, w.saturating_sub(8)), p.body);
        out.push(with_rule(p, true, l));
    }
    for l in text::wrap(&f.network, w).into_iter().filter(|l| !l.is_empty()).take(2) {
        out.push(text_row(p, true, l, p.body));
    }
    out.push(match answering_row(cx) {
        Some(r) => r,
        None => card_buttons(cx, width.saturating_sub(2), cx.focused).indented(2),
    });
    out.extend(card_prompt(cx).map(|l| l.indented(2)));
    out
}

fn checkin_entry(cx: &Cx, width: usize) -> Vec<BLine> {
    let p = &cx.pal;
    let s = cx.s();
    let mut out = Vec::new();
    let Some(f) = checkin_facts(cx) else { return running_entry(cx, width) };
    // The elapsed time stays: words go first (the lead's recommendation, then the project id).
    let elapsed = dur(derive::elapsed_ms(s, cx.now));
    let rec = f.recommended.as_ref().map(|r| format!(" · lead recommends {}", r.replace('_', " "))).unwrap_or_default();
    let heads = [format!("◐ checkin · round {} · {}{rec}", f.round, s.id), format!("◐ checkin · round {}{rec}", f.round), format!("◐ checkin · round {}", f.round)];
    let head = heads.iter().find(|t| text::width(t) + text::width(&elapsed) + 2 <= width).unwrap_or(&heads[2]).clone();
    let mut h = L::new();
    h.s(head, p.amber);
    lr(&mut h, width, vec![Span::styled(elapsed, p.dim)]);
    h.hit(0, ProjTarget::Entry(cx.id()));
    out.push(h.done());
    let w = width.saturating_sub(2);
    // The round's finished phases.
    let phs: Vec<&Phase> = s.phases.iter().filter(|ph| ph.status == StageStatus::Done && u64::from(ph.round) == f.round).collect();
    for full in [true, false] {
        let mut l = L::new();
        for (i, ph) in phs.iter().enumerate() {
            if i > 0 {
                l.sp(2);
            }
            let name = if full { derive::phase_name(ph) } else { derive::initials(&ph.ty) };
            l.s("✓ ", p.green);
            l.s(name, p.body);
            if ph.progress.total > 0 {
                l.s(format!(" {}/{}", ph.progress.done, ph.progress.total), p.dim);
            }
        }
        if l.width() <= w || !full {
            l.clip(w);
            out.push(with_rule(p, true, l));
            break;
        }
    }
    for l in text::wrap(&format!("{} · elapsed {}", f.summary, dur(f.elapsed_ms)), w) {
        out.push(text_row(p, true, l, p.body));
    }
    if let Some((glyph, tone, value)) = &f.decision {
        let lead = format!("{glyph} {value}  ");
        let lw = text::width(&lead);
        for (i, l) in wrap_clip(&f.why, w.saturating_sub(lw), 3).into_iter().enumerate() {
            let mut b = L::new();
            if i == 0 {
                b.s(&lead, p.tone(*tone));
            } else {
                b.sp(lw);
            }
            b.s(l, p.body);
            out.push(with_rule(p, true, b));
        }
    } else if !f.why.is_empty() {
        for l in wrap_clip(&f.why, w, 3) {
            out.push(text_row(p, true, l, p.body));
        }
    }
    const SHOW: usize = 2;
    for (i, q) in f.next.iter().take(SHOW).enumerate() {
        let mut b = L::new();
        b.s(if i == 0 { "next  " } else { "      " }, p.dim);
        b.s(text::truncate(q, w.saturating_sub(6)), p.body);
        out.push(with_rule(p, true, b));
    }
    if f.next.len() > SHOW {
        out.push(text_row(p, true, format!("      +{} more", f.next.len() - SHOW), p.dimmer));
    }
    out.push(match answering_row(cx) {
        Some(r) => r,
        None => card_buttons(cx, width.saturating_sub(2), cx.focused).indented(2),
    });
    out.extend(card_prompt(cx).map(|l| l.indented(2)));
    out
}

fn running_entry(cx: &Cx, width: usize) -> Vec<BLine> {
    let p = &cx.pal;
    let s = cx.s();
    let now = cx.now;
    let mut out = Vec::new();
    let sp = derive::spark(derive::activity(s, now), cx.l.needs_you());
    let tail = format!("{sp} {}", dur(derive::elapsed_ms(s, now)));
    let mut h = L::new();
    let (g, gs, _) = p.project(s.status);
    h.s(format!("{g} "), gs);
    h.s(format!("{} · {} · ", s.id, s.kind), p.dim);
    h.s(text::truncate(&text::clean_maths(&s.title), width.saturating_sub(text::width(&tail) + 4 + s.id.len() + s.kind.len() + 7)), p.bright);
    lr(&mut h, width, vec![Span::styled(sp.clone(), p.accent), Span::styled(format!(" {}", dur(derive::elapsed_ms(s, now))), p.dim)]);
    h.hit(0, ProjTarget::Entry(cx.id()));
    out.push(h.done());
    let short = cx.ui.pane_rows.get() > 0 && cx.ui.pane_rows.get() < SHORT_PANE;
    if cx.ui.folded.contains(&s.id) {
        return out;
    }
    if short {
        // A short pane keeps the header and the busiest step.
        if let Some(ph) = s.phases.iter().find(|ph| ph.status == StageStatus::Running) {
            let mut l = L::new();
            l.sp(3);
            let (g, gs) = p.stage(ph.status);
            tick_lead(&mut l, p, (g, gs), &derive::phase_name(ph), p.bright, LEAD.min(14));
            if ph.progress.total > 0 {
                l.s(format!("{}/{}  ", ph.progress.done, ph.progress.total), p.dim);
            }
            let room = width.saturating_sub(l.width());
            l.spans(phase_briefs(cx, ph, room));
            l.clip(width);
            out.push(l.done());
        }
        return out;
    }
    let mut pend: Vec<String> = Vec::new();
    // A run of phases of one round that has all settled folds into one line once a later phase of
    // another round exists.
    let groups = round_groups(s);
    let mut fold_at: std::collections::BTreeMap<usize, (u32, Vec<usize>)> = Default::default();
    for (gi, (round, idxs)) in groups.iter().enumerate() {
        let settled = idxs.iter().all(|i| matches!(s.phases[*i].status, StageStatus::Done | StageStatus::Skipped));
        if settled && gi + 1 < groups.len() && idxs.len() >= 2 {
            fold_at.insert(idxs[0], (*round, idxs.clone()));
        }
    }
    let skip: std::collections::BTreeSet<usize> = fold_at.values().flat_map(|(_, v)| v.iter().copied()).collect();
    // One leader column for the whole entry: the longest name shown, three dots and a space.
    let mut lead = LEAD.min(14);
    for (pi, ph) in s.phases.iter().enumerate() {
        if skip.contains(&pi) || ph.status == StageStatus::Pending {
            continue;
        }
        lead = lead.max(text::width(&derive::phase_name(ph)) + 4);
    }
    for (round, _) in fold_at.values() {
        lead = lead.max(text::width(&format!("round {round}")) + 4);
    }
    lead = lead.max(text::width("in flight") + 4);
    for (pi, ph) in s.phases.iter().enumerate() {
        let name = derive::phase_name(ph);
        if let Some((round, idxs)) = fold_at.get(&pi) {
            let sk = idxs.iter().filter(|i| s.phases[**i].status == StageStatus::Skipped).count();
            let mut l = L::new();
            l.sp(3);
            tick_lead(&mut l, p, ("✓", p.green), &format!("round {round}"), p.body, lead);
            l.s(format!("{} phases", idxs.len()), p.dim);
            if sk > 0 {
                l.s(format!(" · {sk} skipped"), p.dim);
            }
            out.push(l.done());
            continue;
        }
        if skip.contains(&pi) {
            continue;
        }
        if ph.status == StageStatus::Pending {
            pend.push(name);
            continue;
        }
        let mut l = L::new();
        l.sp(3);
        let holds_plan_card = s.gates.iter().any(|g| g.phase == ph.id && g.kind == GateKind::Plan);
        if matches!(ph.status, StageStatus::Done | StageStatus::Skipped) && holds_plan_card {
            let gate = s.gates.iter().find(|g| g.phase == ph.id);
            let what = format!(
                "{} · {} questions",
                gate.and_then(|g| g.answer.as_ref()).map(|a| format!("{}d", a.choice)).unwrap_or_else(|| "done".into()),
                question_count(s)
            );
            tick_lead(&mut l, p, ("✓", p.green), &name, p.body, lead);
            l.s(what, p.dim);
            if let Some(d) = derive::phase_span_ms(ph, now) {
                let right = dur(d);
                let rw = text::width(&right);
                if l.width() + rw + 2 <= width {
                    l.to(width - rw);
                    l.s(right, p.dim);
                }
            }
        } else {
            let gate = derive::open_gate_of(s, &ph.id).is_some();
            let (g, gs) = if gate { ("◐", p.amber) } else { p.stage(ph.status) };
            tick_lead(&mut l, p, (g, gs), &name, if ph.status == StageStatus::Running { p.bright } else { p.body }, lead);
            if ph.progress.total > 0 {
                l.s(format!("{}/{}  ", ph.progress.done, ph.progress.total), p.dim);
            }
            let room = width.saturating_sub(l.width());
            l.spans(phase_briefs(cx, ph, room));
        }
        l.clip(width);
        out.push(l.done());
    }
    let ra = derive::running_agents(s, now);
    if !ra.is_empty() {
        let mut l = L::new();
        l.sp(3);
        tick_lead(&mut l, p, ("◆", p.accent), "in flight", p.body, lead);
        for (i, (a, _, rt)) in ra.iter().enumerate() {
            if i > 0 {
                l.s(" · ", p.dimmer);
            }
            l.s(format!("{} {} ", a.id, a.role), p.dim);
            l.s(dur(*rt), p.info);
        }
        l.clip(width);
        out.push(l.done());
    }
    let held = derive::waiting_agents(s);
    if !held.is_empty() {
        let mut why: Vec<&str> = held.iter().map(|a| a.note.as_deref().unwrap_or("waiting for a place to call from")).collect();
        why.dedup();
        let mut l = L::new();
        l.sp(3);
        tick_lead(&mut l, p, ("◌", p.amber), &format!("{} waiting", held.len()), p.body, lead);
        l.s(why.join(" · "), p.amber);
        l.clip(width);
        out.push(l.done());
    }
    if !pend.is_empty() {
        let mut l = L::new();
        l.sp(3);
        l.s("○  ", p.dimmer);
        l.s(pend.join(" · "), p.dimmer);
        l.clip(width);
        out.push(l.done());
    }
    let mut l = L::new();
    l.sp(3);
    l.spans(rollup_line(cx, width.saturating_sub(3)).line.spans);
    out.push(l.done());
    let id = cx.id();
    let mut b = L::new();
    b.sp(3);
    button(cx, &mut b, "c", "checkin now", ProjTarget::Checkin(id.clone()));
    b.sp(3);
    button(cx, &mut b, "x", "cancel", ProjTarget::Cancel(id.clone()));
    if cx.focused {
        b.sp(3);
        button(cx, &mut b, "-", "fold", ProjTarget::Fold(id.clone()));
        b.sp(3);
        b.s("esc back", p.dimmer);
    } else {
        b.sp(3);
        b.s("alt-p keys", p.dimmer);
    }
    b.clip(width);
    out.push(b.done());
    out.extend(card_prompt(cx).map(|l| l.indented(3)));
    out
}

/// What the result card says about the report: the badge, the first paragraph, the short cells.
pub struct ReportFacts {
    pub badge: Option<(String, marlowe_project::form::Tone)>,
    pub first: Vec<String>,
    pub cells: Vec<(String, String)>,
}

pub fn report_facts(cx: &Cx, text_w: usize) -> Option<ReportFacts> {
    let it = derive::result_item(cx.s())?;
    let row = view::view(it, cx.s(), cx.now);
    let mut badge = None;
    let mut first = Vec::new();
    let mut cells = Vec::new();
    for c in &row.cells {
        match &c.cell {
            Cell::Badge { text, tone, .. } if badge.is_none() => badge = Some((text.clone(), *tone)),
            Cell::Text { text, lines } => {
                if c.key == "path" {
                    continue;
                }
                let one = !text.contains(char::is_whitespace) && text::width(text) <= 12;
                if one {
                    cells.push((c.label.clone(), text.clone()));
                } else if first.is_empty() {
                    first = wrap_clip(text, text_w, (*lines).max(1) as usize);
                }
            }
            _ => {}
        }
    }
    Some(ReportFacts { badge, first, cells })
}

fn finished_entry(cx: &Cx, width: usize) -> Vec<BLine> {
    let p = &cx.pal;
    let s = cx.s();
    let now = cx.now;
    let w = width.saturating_sub(2);
    let id = cx.id();
    let mut out = Vec::new();
    let (g, gs, word) = p.project(s.status);
    let facts = report_facts(cx, w);
    let mut h = L::new();
    h.s(format!("{g} "), gs);
    h.s(format!("{} · {} · {word} · {}", s.id, s.kind, dur(derive::elapsed_ms(s, now))), p.dim);
    if let Some((b, tone)) = facts.as_ref().and_then(|f| f.badge.as_ref()) {
        h.s(" · ", p.dimmer);
        h.s(format!("{b} confidence"), p.tone(*tone));
    }
    h.clip(width);
    h.hit(0, ProjTarget::Entry(id.clone()));
    out.push(h.done());
    let un = derive::unwritten(s);
    if !un.is_empty() {
        let mut kinds: Vec<(String, usize)> = Vec::new();
        for it in &un {
            match kinds.iter_mut().find(|(k, _)| *k == it.kind) {
                Some((_, n)) => *n += 1,
                None => kinds.push((it.kind.clone(), 1)),
            }
        }
        let mut errs: Vec<String> = un.iter().filter_map(|i| i.error.clone()).collect();
        errs.sort();
        errs.dedup();
        let mut l = L::new();
        l.s("✗ not written: ", p.red);
        l.s(kinds.iter().map(|(k, n)| format!("{k} {n}")).collect::<Vec<_>>().join(", "), p.red);
        l.s(format!(" · {}", errs.join("; ")), p.dim);
        l.clip(w);
        out.push(with_rule(p, false, l));
    }
    if let Some(f) = &facts {
        for l in &f.first {
            out.push(text_row(p, false, l, p.speech));
        }
        if !f.cells.is_empty() {
            let mut l = L::new();
            for (i, (label, v)) in f.cells.iter().enumerate() {
                if i > 0 {
                    l.s(" · ", p.dimmer);
                }
                l.s(format!("{label} {v}"), p.dim);
            }
            l.clip(w);
            out.push(with_rule(p, false, l));
        }
    }
    let failed = s.items.values().filter(|i| i.status == ItemStatus::Failed).count();
    match records_table(cx) {
        Some((columns, rows, total)) if !columns.is_empty() && total > 0 => {
            let mut t: Vec<Vec<String>> = vec![columns.iter().map(|c| c.label.clone()).collect()];
            for r in rows.iter().take(6) {
                t.push(r.iter().map(plain_cell).collect());
            }
            let natural: usize = (0..columns.len()).map(|i| t.iter().map(|r| text::width(&r[i])).max().unwrap_or(0)).sum::<usize>() + 2 * columns.len().saturating_sub(1);
            if !rows.is_empty() && natural <= w {
                for (i, r) in table_lines(&t, w, 2).into_iter().enumerate() {
                    out.push(text_row(p, false, r.join("  ").trim_end(), if i == 0 { p.bright } else { p.body }));
                }
                if let Some(rest) = records_rest(rows.len(), total, 6) {
                    out.push(text_row(p, false, rest, p.dimmer));
                }
            } else {
                let names = columns.iter().map(|c| c.label.clone()).collect::<Vec<_>>().join(" · ");
                let mut l = L::new();
                l.s(format!("records {total} rows · "), p.dim);
                l.s(names, p.dim);
                l.s("  in the report", p.dimmer);
                l.clip(w);
                out.push(with_rule(p, false, l));
            }
        }
        _ => {
            if failed > 0 {
                let mut groups = derive::problems(s);
                groups.sort_by_key(|g| std::cmp::Reverse(g.items.len()));
                for g in groups.iter().take(2) {
                    let (gl, st) = if g.status == ItemStatus::Failed { ("✗", p.red) } else { ("–", p.dimmer) };
                    let mut l = L::new();
                    l.s(format!("{gl} {} {} {}", g.items.len(), g.kind, g.status.as_str()), st);
                    l.s(format!(" · {}", g.error), p.dim);
                    l.clip(w);
                    out.push(with_rule(p, false, l));
                }
            }
        }
    }
    if let Some(r) = &cx.l.report {
        let mut l = L::new();
        l.s("report  ", p.dim);
        l.s(text::truncate_left(&r.path, w.saturating_sub(8)), p.body);
        l.clip(w);
        let mut row = with_rule(p, false, l);
        row.hits.push((2, (row.line.width() as u16).saturating_sub(2), ProjTarget::Report(id.clone())));
        out.push(row);
        let mut b = L::new();
        b.sp(2);
        button(cx, &mut b, "↵", "read report", ProjTarget::Report(id.clone()));
        if failed > 0 {
            let right = format!("{failed} failed item{}", if failed == 1 { "" } else { "s" });
            if b.width() + text::width(&right) + 4 <= width {
                b.to(width - text::width(&right));
                b.s(right, p.red);
            }
        }
        out.push(b.done());
    } else if failed > 0 {
        out.push(text_row(p, false, format!("{failed} failed item{}", if failed == 1 { "" } else { "s" }), p.red));
    }
    out
}

// ---- the transcript hooks -------------------------------------------------------------------------------

/// A hash of what a project's rows depend on besides its state: the pointer, the keys, the width.
fn view_key(cx: &Cx, width: usize) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let hover = match &cx.state.hover {
        Some(Target::Proj(t)) => Some(t.clone()),
        _ => None,
    };
    let ui = cx.ui;
    (width, cx.l.version, cx.now / 1000, cx.focused, ui.card_sel, format!("{:?}", (hover, &ui.prompt)), ui.folded.contains(&cx.l.state.id), ui.pane_rows.get() < SHORT_PANE).hash(&mut h);
    (cx.state.now_ms / 1000).hash(&mut h);
    h.finish()
}

/// A project's rows, laid out again only when something they depend on changed.
fn block_for(state: &State, theme: &Theme, l: &Loaded, width: usize) -> Vec<BLine> {
    let cx = Cx::new(state, theme, l);
    let key = view_key(&cx, width);
    if let Some(m) = state.projects.memo.borrow().get(&l.state.id).filter(|m| m.key == key) {
        return m.rows.clone();
    }
    let rows = entry_block(&cx, width);
    state.projects.memo.borrow_mut().insert(l.state.id.clone(), crate::projects::Memo { key, rows: rows.clone() });
    rows
}

/// A settled project's rows in the transcript.
pub fn entry_lines(state: &State, id: &str, width: usize, theme: &Theme, out: &mut Vec<Line<'static>>) {
    let Some(l) = state.projects.get(id) else { return };
    // A project that runs again is drawn after the conversation, once: its old entry waits to be taken out.
    if !l.is_final() {
        return;
    }
    out.extend(entry_block(&Cx::new(state, theme, l), width).into_iter().map(|b| b.line));
}

/// The entries of the projects that run, drawn after everything else. Each is `(id, rows)`.
pub fn live_blocks(state: &State, width: usize, theme: &Theme) -> Vec<(String, Vec<BLine>)> {
    state.projects.ordered().into_iter().rev().filter(|l| !l.is_final()).map(|l| (l.state.id.clone(), block_for(state, theme, l, width))).collect()
}

/// Registers the click targets of the rows of a project whose first transcript line is `first`
/// when the pane shows lines `top..`; `pane` is the transcript pane.
pub fn register_hits(state: &State, rows: &[BLine], first: i64, top: usize, pane: Rect) {
    for (i, b) in rows.iter().enumerate() {
        let line = first + i as i64;
        if line < top as i64 || line >= (top + pane.height as usize) as i64 {
            continue;
        }
        let y = pane.y + (line - top as i64) as u16;
        for (x, w, t) in &b.hits {
            let rect = Rect { x: pane.x + *x, y, width: (*w).min(pane.width.saturating_sub(*x)), height: 1 };
            super::hit(state, rect, Target::Proj(t.clone()));
        }
    }
}

/// The click targets of a settled project's rows.
pub fn entry_hits(state: &State, id: &str, theme: &Theme, pane: Rect, tw: usize, end_line: usize, top: usize) {
    let Some(l) = state.projects.get(id).filter(|l| l.is_final()) else { return };
    let block = block_for(state, theme, l, tw);
    let first = end_line as i64 - block.len() as i64;
    register_hits(state, &block, first, top, pane);
}

/// A project's end ("… finished", "… failed", "… cancelled"): the relay label of its completion.
fn is_completion(label: &str) -> bool {
    let l = label.trim_end();
    [ProjectStatus::Finished, ProjectStatus::Failed, ProjectStatus::Cancelled].iter().any(|s| l.ends_with(s.as_str()))
}

/// The relay of a project's end, whose own entry in the transcript says all it would: it draws no
/// row (the entry's header reads `✓ p_… · research · finished`).
pub fn completion_shown(state: &State, idx: usize, label: &str) -> bool {
    is_completion(label) && relay_label_only(state, idx, label)
}

/// A relay's label row only (`idx` is its place in the transcript): the project's entry already
/// shows what the lead said. That holds for the notes of a project (while it runs and once its
/// result stands in the transcript) and for the completion relay of its latest run. The completion
/// relay of an earlier run keeps its text while the project runs again, and next to a newer result.
pub fn relay_label_only(state: &State, idx: usize, label: &str) -> bool {
    state.projects.loaded.iter().any(|l| {
        let id = &l.state.id;
        if !label.contains(id.as_str()) {
            return false;
        }
        if !is_completion(label) {
            return !l.is_final() || state.entries.iter().any(|e| matches!(e, Entry::Project { id: p } if p == id));
        }
        if !l.is_final() {
            return false;
        }
        let latest = state.entries.iter().rposition(|e| matches!(e, Entry::News { label, .. } if label.contains(id.as_str()) && is_completion(label)));
        latest == Some(idx) && state.entries.iter().any(|e| matches!(e, Entry::Project { id: p } if p == id))
    })
}

// ---- the board ---------------------------------------------------------------------------------------

/// `1:42`, `12:04`, `1h05`: elapsed time in as few columns as the board has.
pub fn short_elapsed(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}h{:02}", s / 3600, (s / 60) % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// The board's rows for the projects: `{glyph} {title}` then the elapsed time and the counts of
/// the phases that run (`DC 63/83`), as many as fit.
pub fn board_rows(state: &State, w: usize, max: usize, theme: &Theme) -> (Vec<(Line<'static>, Option<Target>)>, Option<String>) {
    let ui = &state.projects;
    let pal = Pal::new(theme);
    let now = ui.now(state.now_ms);
    let mut rows = Vec::new();
    let all = ui.ordered();
    for l in all.iter().take(max.max(1)) {
        let s = &l.state;
        let (g, gs, word) = pal.project(s.status);
        let mut facts: Vec<(String, Style)> = vec![(short_elapsed(derive::elapsed_ms(s, now)), pal.dim)];
        match s.status {
            ProjectStatus::Running => {
                for ph in s.phases.iter().filter(|p| p.status == StageStatus::Running) {
                    facts.push((format!("{} {}/{}", derive::initials(&ph.ty), ph.progress.done, ph.progress.total), pal.dim));
                }
            }
            ProjectStatus::AwaitingPlan => facts.push(("plan".into(), pal.amber)),
            ProjectStatus::AwaitingCheckin => facts.push(("checkin".into(), pal.amber)),
            ProjectStatus::Finished => {
                let badge = derive::result_item(s).map(|it| view::view(it, s, now)).and_then(|r| {
                    r.cells.into_iter().find_map(|c| match c.cell {
                        Cell::Badge { text, tone, .. } => Some((text, tone)),
                        _ => None,
                    })
                });
                match badge {
                    Some((t, tone)) => facts.push((t, pal.tone(tone))),
                    None => facts.push((word.to_string(), pal.dim)),
                }
            }
            _ => facts.push((word.to_string(), if s.status == ProjectStatus::Failed { pal.red } else { pal.dim })),
        }
        // Facts are kept from the left while the title keeps at least 10 columns.
        let mut shown: Vec<(String, Style)> = Vec::new();
        let mut used = 0;
        for (t, st) in facts {
            let add = text::width(&t) + 1;
            if w.saturating_sub(2 + used + add) < 10 {
                break;
            }
            used += add;
            shown.push((t, st));
        }
        let title = text::truncate(&text::clean_maths(&s.title), w.saturating_sub(2 + used + 1).max(4));
        let pad = w.saturating_sub(2 + text::width(&title) + used);
        let target = Target::Proj(ProjTarget::Rail(s.id.clone()));
        let mut spans = vec![Span::styled(format!("{g} "), gs), Span::styled(title, pal.body), Span::raw(" ".repeat(pad))];
        for (t, st) in shown {
            spans.push(Span::styled(format!("{t} "), st));
        }
        rows.push((Line::from(spans), Some(target)));
    }
    if all.len() > max.max(1) {
        rows.push((Line::from(Span::styled(format!("+{} more", all.len() - max.max(1)), pal.dimmer)), None));
    }
    let count = if ui.needs_you() > 0 {
        Some(format!("{} need{} you", ui.needs_you(), if ui.needs_you() == 1 { "s" } else { "" }))
    } else if ui.running() > 0 {
        Some(format!("{} running", ui.running()))
    } else {
        None
    };
    (rows, count)
}
