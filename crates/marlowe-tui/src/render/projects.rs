//! The painters a project's screens share: palette roles, a line builder that records click
//! targets, the phase counts, the rollups, the cards' facts, the report.
//!
//! Everything is derived from a [`ProjectState`] through the generic view-model and
//! `marlowe_project::derive`; nothing here names a project kind, a phase or a step. Foreground
//! colours only: a project never draws a fill.

use marlowe_project::form::Tone;
use marlowe_project::state::*;
use marlowe_project::view::{self, Cell};
use marlowe_project::{derive, Gate};
use marlowe_surface::chrome::Ink;
use marlowe_surface::{markdown, Theme};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

use super::{hovered, ink};
use crate::projects::{registry, Loaded, ProjTarget, ProjectsUi, Prompt};
use crate::state::{State, Target};
use crate::text;

// ---- palette roles ------------------------------------------------------------------------------

/// The palette roles of the project screens, as styles (foreground only).
#[derive(Clone, Copy)]
pub struct Pal {
    pub body: Style,
    pub bright: Style,
    pub dim: Style,
    pub dimmer: Style,
    pub accent: Style,
    pub accent_b: Style,
    pub speech: Style,
    pub green: Style,
    pub amber: Style,
    pub red: Style,
    pub info: Style,
}

impl Pal {
    pub fn new(theme: &Theme) -> Pal {
        Pal {
            body: theme.normal(),
            bright: theme.bright(),
            dim: ink(Ink::Dim, theme),
            dimmer: ink(Ink::Dimmer, theme),
            accent: ink(Ink::Accent, theme),
            accent_b: ink(Ink::Accent, theme).add_modifier(Modifier::BOLD),
            speech: ink(Ink::Speech, theme),
            green: ink(Ink::Green, theme),
            amber: ink(Ink::Amber, theme),
            red: ink(Ink::Red, theme),
            info: ink(Ink::Info, theme),
        }
    }

    /// A view-model tone as a style.
    pub fn tone(&self, t: Tone) -> Style {
        match t {
            Tone::Ok => self.green,
            Tone::Warn => self.amber,
            Tone::Bad => self.red,
            Tone::Info => self.info,
            Tone::Accent => self.accent,
            Tone::Dim => self.dim,
        }
    }

    /// A phase or step status: its glyph and style.
    pub fn stage(&self, s: StageStatus) -> (&'static str, Style) {
        match s {
            StageStatus::Pending => ("○", self.dimmer),
            StageStatus::Running => ("●", self.info),
            StageStatus::Done => ("✓", self.green),
            StageStatus::Failed => ("✗", self.red),
            StageStatus::Skipped => ("–", self.dimmer),
        }
    }

    /// A project's status: glyph, style and the word.
    pub fn project(&self, s: ProjectStatus) -> (&'static str, Style, &'static str) {
        match s {
            ProjectStatus::AwaitingPlan => ("◐", self.amber, "awaiting plan"),
            ProjectStatus::AwaitingCheckin => ("◐", self.amber, "checkin"),
            ProjectStatus::Running => ("●", self.info, "running"),
            ProjectStatus::Finished => ("✓", self.green, "finished"),
            ProjectStatus::Failed => ("✗", self.red, "failed"),
            ProjectStatus::Cancelled => ("–", self.dimmer, "cancelled"),
        }
    }
}

/// What a painter needs: the client, the palette, the project.
pub struct Cx<'a> {
    pub state: &'a State,
    pub pal: Pal,
    pub ui: &'a ProjectsUi,
    pub l: &'a Loaded,
    /// The entry has the keys: its buttons show them.
    pub focused: bool,
    /// The clock the entry is drawn against.
    pub now: u64,
}

impl<'a> Cx<'a> {
    pub fn new(state: &'a State, theme: &'a Theme, l: &'a Loaded) -> Cx<'a> {
        let ui = &state.projects;
        let focused = ui.focus.as_deref() == Some(l.state.id.as_str()) && state.focus == crate::state::Focus::Conversation;
        Cx { state, pal: Pal::new(theme), ui, l, focused, now: ui.now(state.now_ms) }
    }

    pub fn s(&self) -> &'a ProjectState {
        &self.l.state
    }

    pub fn id(&self) -> String {
        self.l.state.id.clone()
    }

    /// The pointer is over `t`.
    pub fn hot(&self, t: &ProjTarget) -> bool {
        hovered(self.state, &Target::Proj(t.clone()))
    }
}

// ---- lines that remember where they can be clicked -------------------------------------------------

/// One drawn line and the click targets on it (`x`, width, target), relative to the line's origin.
#[derive(Debug, Clone, Default)]
pub struct BLine {
    pub line: Line<'static>,
    pub hits: Vec<(u16, u16, ProjTarget)>,
}

impl BLine {
    /// Shifts the line right by `n` columns.
    pub fn indented(self, n: usize) -> BLine {
        let mut l = L::new();
        l.sp(n);
        l.spans(self.line.spans);
        let mut out = l.done();
        out.hits = self.hits.into_iter().map(|(x, w, t)| (x + n as u16, w, t)).collect();
        out
    }
}

/// Builds a [`BLine`]: pieces of text in styles, with click regions.
#[derive(Default)]
pub struct L {
    spans: Vec<Span<'static>>,
    w: usize,
    hits: Vec<(u16, u16, ProjTarget)>,
}

impl L {
    pub fn new() -> L {
        L::default()
    }

    /// Appends `t` in style `st`.
    pub fn s(&mut self, t: impl AsRef<str>, st: Style) -> &mut L {
        let t = t.as_ref();
        if !t.is_empty() {
            self.w += text::width(t);
            self.spans.push(Span::styled(t.to_string(), st));
        }
        self
    }

    /// Appends `n` spaces.
    pub fn sp(&mut self, n: usize) -> &mut L {
        if n > 0 {
            self.w += n;
            self.spans.push(Span::raw(" ".repeat(n)));
        }
        self
    }

    /// The width so far.
    pub fn width(&self) -> usize {
        self.w
    }

    /// Pads with spaces to `w` columns.
    pub fn to(&mut self, w: usize) -> &mut L {
        let n = w.saturating_sub(self.w);
        self.sp(n)
    }

    /// Makes `[from, now)` a click region for `t`.
    pub fn hit(&mut self, from: usize, t: ProjTarget) -> &mut L {
        if self.w > from {
            self.hits.push((from as u16, (self.w - from) as u16, t));
        }
        self
    }

    /// Appends spans.
    pub fn spans(&mut self, v: Vec<Span<'static>>) -> &mut L {
        for s in v {
            self.w += text::width(&s.content);
            self.spans.push(s);
        }
        self
    }

    /// Cuts the line to `w` columns.
    pub fn clip(&mut self, w: usize) -> &mut L {
        if self.w <= w {
            return self;
        }
        let mut out: Vec<Span<'static>> = Vec::new();
        let mut used = 0;
        for s in std::mem::take(&mut self.spans) {
            let sw = text::width(&s.content);
            if used + sw <= w {
                used += sw;
                out.push(s);
            } else {
                let room = w.saturating_sub(used);
                if room > 0 {
                    out.push(Span::styled(text::truncate(&s.content, room), s.style));
                    used += room;
                }
                break;
            }
        }
        self.spans = out;
        self.w = used;
        self.hits.retain(|(x, _, _)| (*x as usize) < w);
        self
    }

    pub fn done(self) -> BLine {
        BLine { line: Line::from(self.spans), hits: self.hits }
    }
}

// ---- text helpers ---------------------------------------------------------------------------------

/// Wraps `s` to `w` and keeps `n` lines; the last ends in `…` when text was cut.
pub fn wrap_clip(s: &str, w: usize, n: usize) -> Vec<String> {
    let mut v = text::wrap(&text::clean(s), w.max(4));
    while v.last().is_some_and(|l| l.is_empty()) && v.len() > 1 {
        v.pop();
    }
    if v.len() > n {
        v.truncate(n);
        if let Some(last) = v.last_mut() {
            *last = text::truncate(&format!("{last}…"), w);
        }
    }
    v
}

/// Puts `right` flush to the right edge of `w` when it fits beside what is there.
pub fn lr(left: &mut L, w: usize, right: Vec<Span<'static>>) {
    let rw: usize = right.iter().map(|s| text::width(&s.content)).sum();
    if left.width() + rw + 2 <= w {
        left.to(w - rw);
        left.spans(right);
    }
}

pub fn dur(ms: u64) -> String {
    view::format_duration(ms)
}

// ---- the project's numbers ---------------------------------------------------------------------------

/// The number of questions: root items made by the first step of the first phase.
pub fn question_count(s: &ProjectState) -> usize {
    let first = s.phases.first().and_then(|p| p.steps.first()).map(|st| st.id.clone());
    match first {
        Some(step) => s.items.values().filter(|i| i.parent.is_none() && i.step == step).count(),
        None => s.plan.questions.len(),
    }
}

/// The consecutive phases of one round: (round, indices into `phases`).
pub fn round_groups(s: &ProjectState) -> Vec<(u32, Vec<usize>)> {
    let mut groups: Vec<(u32, Vec<usize>)> = Vec::new();
    for (i, ph) in s.phases.iter().enumerate() {
        match groups.last_mut() {
            Some((r, v)) if *r == ph.round => v.push(i),
            _ => groups.push((ph.round, vec![i])),
        }
    }
    groups
}

/// The rollups of the project's kind over settled items.
pub fn rollup_line(cx: &Cx, w: usize) -> BLine {
    let p = &cx.pal;
    let rs = derive::rollups_split(cx.s(), registry(), cx.now);
    if rs.iter().all(|r| r.total == 0) {
        let mut l = L::new();
        l.s("no items yet", p.dim);
        return l.done();
    }
    let build = |style: u8, n: usize| -> L {
        let mut l = L::new();
        for (i, r) in rs.iter().take(n).enumerate() {
            if i > 0 {
                l.sp(if style == 0 { 4 } else { 3 });
            }
            l.s(&r.title, p.dim);
            l.s(format!(" {}", r.total), p.body);
            let mut any = false;
            let push = |l: &mut L, any: &mut bool, t: String, st: Style| {
                if *any {
                    l.sp(if style == 0 { 2 } else { 1 });
                } else {
                    l.s(":", p.dim);
                    l.sp(if style == 0 { 2 } else { 1 });
                }
                l.s(t, st);
                *any = true;
            };
            for g in &r.groups {
                let t = match (&g.glyph, style) {
                    (Some(gl), 1) => format!("{gl}{}", g.count),
                    (Some(gl), _) => format!("{gl} {} {}", g.value, g.count),
                    (None, _) => format!("{} {}", g.value, g.count),
                };
                push(&mut l, &mut any, t, p.tone(g.tone));
            }
            if r.running > 0 {
                push(&mut l, &mut any, format!("+{} running", r.running), p.info);
            }
            if r.queued > 0 {
                push(&mut l, &mut any, format!("+{} queued", r.queued), p.dim);
            }
        }
        l
    };
    for style in [0u8, 3, 1] {
        let l = build(style, rs.len());
        if l.width() <= w {
            return l.done();
        }
    }
    for n in (1..rs.len()).rev() {
        let l = build(1, n);
        if l.width() <= w {
            return l.done();
        }
    }
    let mut l = build(1, 1);
    l.clip(w);
    l.done()
}

// ---- the cards' facts --------------------------------------------------------------------------------

/// The plan card's facts, from the gate payload (the Plan schema every kind shares).
pub struct PlanFacts {
    pub no: u64,
    pub objective: String,
    pub questions: Vec<(String, String)>,
    pub fields: Vec<String>,
    pub limits: String,
    pub network: String,
}

pub fn plan_facts(cx: &Cx) -> Option<PlanFacts> {
    let g = cx.l.open_gate().filter(|g| g.kind == GateKind::Plan)?;
    let pl = &g.payload;
    let s = |k: &str| pl.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let questions = pl
        .get("questions")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().map(|q| (q.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(), q.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string())).collect())
        .unwrap_or_default();
    let fields: Vec<String> =
        pl.get("fields").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|f| f.get("label").and_then(|v| v.as_str()).map(str::to_string)).collect()).unwrap_or_default();
    let limits = pl
        .get("limits")
        .and_then(|v| v.as_object())
        .map(|o| o.iter().map(|(k, v)| format!("{} {}", view::scalar_text(v), derive::limit_words(k))).collect::<Vec<_>>().join(" · "))
        .unwrap_or_default();
    Some(PlanFacts { no: pl.get("plan_no").and_then(|v| v.as_u64()).unwrap_or(1), objective: s("objective"), questions, fields, limits, network: s("grant") })
}

/// The decision a checkin follows: the newest item the last employee step before the gate made.
pub fn gate_source<'a>(s: &'a ProjectState, g: &Gate) -> Option<&'a Item> {
    let at = s.phases.iter().position(|p| p.id == g.phase).unwrap_or(s.phases.len());
    for ph in s.phases[..at].iter().rev() {
        if ph.steps.iter().any(|st| st.kind == StepKind::Fanout) {
            let it = s
                .order
                .values()
                .flatten()
                .rev()
                .filter_map(|id| s.items.get(id))
                .find(|i| i.phase == ph.id && ph.steps.iter().any(|st| st.id == i.step && st.kind == StepKind::Fanout));
            if it.is_some() {
                return it;
            }
        }
    }
    None
}

/// The checkin card's facts.
pub struct CheckinFacts {
    pub round: u64,
    pub recommended: Option<String>,
    pub summary: String,
    pub elapsed_ms: u64,
    pub decision: Option<(String, Tone, String)>,
    pub why: String,
    pub next: Vec<String>,
}

pub fn checkin_facts(cx: &Cx) -> Option<CheckinFacts> {
    let g = cx.l.open_gate().filter(|g| g.kind == GateKind::Checkin)?;
    let pl = &g.payload;
    let summary = pl.get("summary").and_then(|v| v.as_object()).cloned().unwrap_or_default();
    let elapsed_ms = summary.get("elapsed_ms").and_then(|v| v.as_u64()).unwrap_or(0);
    let row = gate_source(cx.s(), g).map(|it| view::view(it, cx.s(), cx.now));
    let mut decision = None;
    let mut why = String::new();
    if let Some(r) = &row {
        for c in &r.cells {
            match &c.cell {
                Cell::Badge { text, glyph, tone } if decision.is_none() => {
                    decision = Some((glyph.clone().unwrap_or_default(), *tone, text.clone()));
                }
                Cell::Text { text, .. } if why.is_empty() => why = text.clone(),
                _ => {}
            }
        }
    }
    // The rest of the payload is the lead's to describe: arrays of one-string objects are a list,
    // string scalars are text. The gate envelope's own fields are not.
    const ENVELOPE: [&str; 6] = ["kind", "reason", "round", "summary", "options", "recommended"];
    let mut next: Vec<String> = Vec::new();
    let mut note = String::new();
    if let Some(o) = pl.as_object() {
        for v in o.iter().filter(|(k, _)| !ENVELOPE.contains(&k.as_str())).map(|(_, v)| v) {
            match v {
                Value::Array(a) if next.is_empty() && !a.is_empty() => {
                    let list: Vec<String> = a
                        .iter()
                        .filter_map(|e| match e {
                            Value::Object(m) if m.len() == 1 => m.values().next().and_then(|x| x.as_str()).map(str::to_string),
                            _ => None,
                        })
                        .collect();
                    if list.len() == a.len() {
                        next = list;
                    }
                }
                Value::String(t) if t.len() > note.len() => note = t.clone(),
                _ => {}
            }
        }
    }
    if why.is_empty() {
        why = note;
    }
    Some(CheckinFacts {
        round: pl.get("round").and_then(|v| v.as_u64()).unwrap_or(1),
        recommended: pl.get("recommended").and_then(|v| v.as_str()).map(str::to_string),
        summary: derive::summary_words(&summary, true),
        elapsed_ms,
        decision,
        why,
        next,
    })
}

/// The card's buttons as one line: `Approve the plan   revise   Cancel`. `keys` puts the key
/// letter before each (a focused card).
pub fn card_buttons(cx: &Cx, w: usize, keys: bool) -> BLine {
    let p = &cx.pal;
    let Some(g) = cx.l.open_gate() else { return BLine::default() };
    let id = cx.id();
    let rec = cx.ui.recommended(&id);
    let letters = crate::projects::option_keys(g);
    let build = |human: bool| -> L {
        let mut l = L::new();
        for (i, o) in g.options.iter().enumerate() {
            if i > 0 {
                l.sp(3);
            }
            let t = ProjTarget::Option(id.clone(), o.id.clone());
            let from = l.width();
            if keys {
                l.s(letters.get(i).map_or("?".to_string(), |k| k.to_string()), p.accent_b);
                l.sp(1);
            }
            let label = if human { o.id.replace('_', " ") } else { o.label.clone() };
            let picked = keys && cx.ui.card_sel == i;
            let hot = cx.hot(&t) || picked;
            let ls = if keys {
                if hot {
                    p.bright
                } else {
                    p.body
                }
            } else if hot {
                p.accent_b
            } else {
                p.accent
            };
            l.s(label, ls);
            if rec.as_deref() == Some(o.id.as_str()) {
                l.s(" ◆", p.accent_b);
            }
            l.hit(from, t);
            if o.id == "approve" && g.kind == GateKind::Plan {
                l.sp(3);
                let from = l.width();
                if keys {
                    l.s("r", p.accent_b);
                    l.sp(1);
                }
                let t = ProjTarget::Revise(id.clone());
                let hot = cx.hot(&t);
                l.s("revise", if keys { if hot { p.bright } else { p.body } } else if hot { p.accent_b } else { p.accent });
                l.hit(from, t);
            }
        }
        l
    };
    let l = build(false);
    if l.width() <= w {
        return l.done();
    }
    let mut l = build(true);
    l.clip(w);
    l.done()
}

/// The inline steer line, the cancel question, or nothing: a line under the buttons.
pub fn card_prompt(cx: &Cx) -> Option<BLine> {
    let p = &cx.pal;
    let id = cx.id();
    match &cx.ui.prompt {
        Some(Prompt::Steer { project, text }) if *project == id => {
            let mut l = L::new();
            l.s("steer ", p.dim);
            l.s("› ", p.accent);
            l.s(text, p.body);
            l.s("▏", p.accent);
            l.sp(2);
            l.s("↵ send   esc back", p.dimmer);
            Some(l.done())
        }
        Some(Prompt::ConfirmCancel { project }) if *project == id => {
            let mut l = L::new();
            l.s(format!("cancel {id}? "), p.amber);
            let from = l.width();
            l.s("y", p.accent_b);
            l.hit(from, ProjTarget::CancelYes(id.clone()));
            l.s(" / ", p.dimmer);
            let from = l.width();
            l.s("n", p.accent_b);
            l.hit(from, ProjTarget::CancelNo);
            Some(l.done())
        }
        _ => None,
    }
}

// ---- items as table cells ----------------------------------------------------------------------------

/// Table cell text on one line.
pub fn plain_cell(c: &Cell) -> String {
    match c {
        Cell::Text { text, .. } => text.replace('\n', " "),
        Cell::Badge { text, glyph, .. } => match glyph {
            Some(g) => format!("{g} {text}"),
            None => text.clone(),
        },
        Cell::Link { label, host, .. } => {
            if label == host {
                host.clone()
            } else {
                label.clone()
            }
        }
        Cell::Meter { text, .. } => text.clone(),
        Cell::Bar { text, .. } => text.clone(),
        Cell::Duration { text, .. } | Cell::Time { text, .. } => text.clone(),
        Cell::Json(s) => s.clone(),
        Cell::Table { rows, .. } => format!("{} rows", rows.len()),
    }
}

/// A table of strings aligned in columns; the widest column gives way to fit `width`.
pub fn table_lines(rows: &[Vec<String>], width: usize, gap: usize) -> Vec<Vec<String>> {
    let ncol = rows.iter().map(Vec::len).max().unwrap_or(0);
    if ncol == 0 {
        return Vec::new();
    }
    let mut ws: Vec<usize> = (0..ncol).map(|i| rows.iter().map(|r| r.get(i).map_or(0, |c| text::width(c))).max().unwrap_or(0)).collect();
    let mut total: usize = ws.iter().sum::<usize>() + gap * (ncol - 1);
    while total > width {
        let (i, _) = ws.iter().enumerate().max_by_key(|(_, w)| **w).unwrap();
        if ws[i] <= 3 {
            break;
        }
        ws[i] -= 1;
        total -= 1;
    }
    rows.iter().map(|r| (0..ncol).map(|i| text::pad(r.get(i).map_or("", |c| c.as_str()), ws[i])).collect()).collect()
}

/// What the records table leaves out below its rows: the rows the row cap cut (they have details)
/// and, apart from them, the rows that only name their key and have nothing more to show.
pub fn records_rest(shown: usize, total: usize, cap: usize) -> Option<String> {
    let cut = shown.saturating_sub(cap);
    let bare = total.saturating_sub(shown);
    match (cut, bare) {
        (0, 0) => None,
        (c, 0) => Some(format!("+{c} more")),
        (0, b) => Some(format!("{b} named with no details")),
        (c, b) => Some(format!("+{c} more · {b} named with no details")),
    }
}

/// The records table, ordered by filled cells (stable), without the rows that only name their key.
/// Returns the columns, the rows that have details and the number of rows in all.
pub fn records_table(cx: &Cx) -> Option<(Vec<view::Column>, Vec<Vec<Cell>>, usize)> {
    let out = derive::output_row(cx.s(), registry(), cx.now)?;
    let key = out.records_key?;
    let c = out.row.cells.into_iter().find(|c| c.key == key)?;
    let Cell::Table { columns, rows } = c.cell else { return None };
    let filled = |r: &Vec<Cell>| r.iter().filter(|c| !plain_cell(c).is_empty()).count();
    let mut idx: Vec<usize> = (0..rows.len()).collect();
    idx.sort_by_key(|i| std::cmp::Reverse(filled(&rows[*i])));
    let total = rows.len();
    let shown: Vec<Vec<Cell>> = idx.into_iter().filter(|i| filled(&rows[*i]) > 1).map(|i| rows[i].clone()).collect();
    Some((columns, shown, total))
}

// ---- the report -------------------------------------------------------------------------------------

/// The report's markdown with the project's own title heading removed.
pub fn report_text(l: &Loaded) -> Option<String> {
    let raw = l.report.as_ref()?.text.as_deref()?;
    let title = l.state.title.trim();
    let mut out = String::new();
    for (i, line) in raw.lines().enumerate() {
        if i == 0 && line.strip_prefix("# ").is_some_and(|h| h.trim() == title) {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    Some(out.trim_start_matches('\n').to_string())
}

/// A line of the report that is `[n] title - url`: it stays its own row.
fn is_reference_line(line: &str) -> bool {
    let Some(rest) = line.strip_prefix('[') else { return false };
    rest.split_once("] ").is_some_and(|(n, _)| n.parse::<usize>().is_ok())
}

/// The report as lines in Marlowe's voice colour (the markdown renderer's own styles). Each
/// `[n] …` reference renders on its own, so the list stays one entry to a row.
pub fn report_lines(l: &Loaded, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let base = Style::default().fg(theme.speech());
    let Some(t) = report_text(l) else { return Vec::new() };
    let mut rendered: Vec<Line<'static>> = Vec::new();
    let mut chunk = String::new();
    for line in t.lines() {
        if is_reference_line(line) {
            if !chunk.is_empty() {
                rendered.extend(markdown::render_prose(&chunk, width, theme, base));
                chunk.clear();
            }
            rendered.extend(markdown::render_prose(line, width, theme, base));
        } else {
            chunk.push_str(line);
            chunk.push('\n');
        }
    }
    if !chunk.is_empty() {
        rendered.extend(markdown::render_prose(&chunk, width, theme, base));
    }
    rendered
}

#[cfg(test)]
mod tests {
    use marlowe_project::GateKind;

    use super::*;

    #[test]
    fn the_records_rest_counts_cut_rows_and_bare_rows_apart() {
        // m6: 61 rows, 52 with details; 8 shown.
        assert_eq!(records_rest(52, 61, 8).as_deref(), Some("+44 more · 9 named with no details"));
        // m9: 12 rows, 6 with details, all shown.
        assert_eq!(records_rest(6, 12, 8).as_deref(), Some("6 named with no details"));
        assert_eq!(records_rest(10, 10, 8).as_deref(), Some("+2 more"));
        assert_eq!(records_rest(3, 3, 8), None);
    }

    #[test]
    fn a_reference_line_is_its_own_row_and_a_citation_is_not() {
        assert!(is_reference_line("[2] Releases - crossterm-rs/crossterm - upd.dev - https://upd.dev/x"));
        assert!(!is_reference_line("[2, 3] is a citation inside prose"));
        assert!(!is_reference_line("Kimi K3 [2]"));
    }

    fn gate(kind: GateKind, ids: &[(&str, &str)]) -> marlowe_project::Gate {
        let options: Vec<serde_json::Value> = ids.iter().map(|(id, label)| serde_json::json!({"id": id, "label": label, "needs_text": false})).collect();
        serde_json::from_value(serde_json::json!({
            "id": "g_1", "kind": if kind == GateKind::Plan { "plan" } else { "checkin" }, "phase": "ph_1", "step": "s", "status": "open",
            "payload": {}, "options": options, "asked_ms": 0
        }))
        .expect("a gate")
    }

    #[test]
    fn card_keys_follow_the_labels_and_keep_a_and_x() {
        let g = gate(GateKind::Checkin, &[("approve", "Approve"), ("keep_researching", "Keep researching"), ("steer", "Steer"), ("cancel", "Cancel")]);
        assert_eq!(crate::projects::option_keys(&g), vec!['a', 'k', 's', 'x']);
        // A budget checkin: other options get their own first free letter, never a, x or `?`.
        let g = gate(GateKind::Checkin, &[("approve", "Approve"), ("extend", "Extend the budget"), ("expand", "Expand the scope"), ("cancel", "Cancel")]);
        assert_eq!(crate::projects::option_keys(&g), vec!['a', 'e', 'p', 'x']);
        // A plan keeps `r` for revise.
        let g = gate(GateKind::Plan, &[("approve", "Approve"), ("reject", "Reject"), ("cancel", "Cancel")]);
        assert_eq!(crate::projects::option_keys(&g), vec!['a', 'e', 'x']);
    }
}
