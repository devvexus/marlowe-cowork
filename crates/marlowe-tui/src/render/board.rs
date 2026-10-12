//! The Situation Board column: the Model and Workspace pickers, then Crew, Watch, Threads and
//! Memory — each in its own box when the column is tall enough, else one box whose title is a
//! numbered tab strip. Every section shows an honest empty state. Today (the date and quiet
//! hours) is on the footer, where it costs no rows.

use marlowe_proto::{RunStatus, Severity};
use marlowe_surface::chrome::Ink;
use marlowe_surface::{FocusLevel, RegionId, Theme};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::boot::severity_style;
use super::{
    border_left, border_right, focus_level, hints, hit, hovered, ink, label_style, lit, put, region, region_box,
    titled, BoardMode, Geometry,
};
use crate::crew::RAIL_RUNS;
use crate::state::{BoardTab, Entry, Focus, State, Target};
use crate::text;
use crate::wake::{notice_glyph, run_detail, run_glyph, Presence};

/// Notices Watch lists under the runs.
const BOARD_NOTICES: usize = 3;

pub fn draw(g: &Geometry, state: &State, theme: &Theme, buf: &mut Buffer) {
    if let Some(r) = g.model {
        model_box(r, state, theme, buf);
    }
    if let Some(r) = g.workspace {
        workspace_box(r, state, theme, buf);
    }
    match g.board_mode {
        BoardMode::Stacked => {
            for tab in BoardTab::ALL {
                stacked_box(g.sections[tab.index()], tab, state, theme, buf);
            }
        }
        BoardMode::Tabbed => tabbed_box(g.board, state, theme, buf),
    }
}

// ---- the pickers --------------------------------------------------------------------------------

fn model_box(r: Rect, state: &State, theme: &Theme, buf: &mut Buffer) {
    let reg = region(RegionId::Model);
    let focus = focus_level(state, Focus::Model);
    let inner = region_box(r, titled(reg.label(), None, focus, theme), &reg.hotkey_label(), focus, theme, buf);
    let w = inner.width.saturating_sub(2);
    let conductor = state.library.conductor();
    // `/fast`: the next call runs on the Quick slot's model (U17).
    let fast = state.bg.fast_on(state.session.as_ref());
    let shown = fast.and_then(|f| f.model.clone()).or_else(|| conductor.clone());
    let (name, style) = match &shown {
        Some(m) => (text::short_model(m, state.library.info(m)), theme.normal()),
        None if state.library.known => ("add a model".to_string(), ink(Ink::Amber, theme)),
        None => ("…".to_string(), ink(Ink::Dim, theme)),
    };
    // Effort here; the context window is the meter on the next row.
    let mut facts = Vec::new();
    if conductor.is_some() {
        facts.push(state.library.slot_rows()[0].effort.as_str().to_string());
    }
    if fast.is_some() {
        facts.insert(0, "fast".to_string());
    }
    // The name comes first: facts drop off from the right before it is cut.
    while !facts.is_empty() && text::width(&name) + 2 + text::width(&facts.join(" · ")) + 2 > w as usize {
        facts.pop();
    }
    let facts = facts.join(" · ");
    let name_w = (w as usize).saturating_sub(text::width(&facts) + 4).max(8);
    let left = Line::from(vec![
        Span::styled(text::truncate(&name, name_w), style),
        Span::styled(" ▾", ink(Ink::Dim, theme)),
    ]);
    put(buf, inner.x + 1, inner.y, &left, w);
    let fw = text::width(&facts) as u16;
    if !facts.is_empty() && left.width() as u16 + fw + 2 <= w {
        put(buf, inner.right().saturating_sub(fw + 1), inner.y, &Line::from(Span::styled(facts, ink(Ink::Dim, theme))), fw);
    }
    if inner.height >= 2 {
        super::context::draw_meter(Rect { y: inner.y + 1, height: 1, ..inner }, state, theme, buf);
    }
}

fn workspace_box(r: Rect, state: &State, theme: &Theme, buf: &mut Buffer) {
    let reg = region(RegionId::Workspace);
    let focus = focus_level(state, Focus::Workspace);
    let inner = region_box(r, titled(reg.label(), None, focus, theme), &reg.hotkey_label(), focus, theme, buf);
    let w = inner.width.saturating_sub(2) as usize;
    let line = match state.shown_workspace() {
        Some(ws) => Line::from(vec![
            Span::styled(text::truncate_left(&text::short_path(&text::clean(ws)), w.saturating_sub(2)), theme.normal()),
            Span::styled(" ▾", ink(Ink::Dim, theme)),
        ]),
        None => Line::from(Span::styled("none — new sessions have no workspace", ink(Ink::Dimmer, theme))),
    };
    put(buf, inner.x + 1, inner.y, &line, w as u16);
}

// ---- the sections -------------------------------------------------------------------------------

/// The tab strip's layout for a title `width` wide: each tab's offset and width.
pub struct TabStrip {
    pub cells: Vec<(BoardTab, usize, usize)>,
    /// Inactive tabs show their number only.
    pub compact: bool,
    pub sep: usize,
}

/// `1 Crew  2 Watch  3 Threads  4 Memory`, tightening until it fits: one space between tabs, then
/// the inactive tabs by number only.
pub fn tab_strip(width: usize, active: BoardTab) -> TabStrip {
    let full = |t: BoardTab| 2 + text::width(t.label());
    for (sep, compact) in [(2, false), (1, false), (1, true)] {
        let mut cells = Vec::new();
        let mut x = 0;
        for t in BoardTab::ALL {
            if x > 0 {
                x += sep;
            }
            let w = if compact && t != active { 1 } else { full(t) };
            cells.push((t, x, w));
            x += w;
        }
        if x <= width || compact {
            return TabStrip { cells, compact, sep };
        }
    }
    unreachable!("the compact strip is always returned")
}

/// A board line and what a click on it opens.
pub type Row = (Line<'static>, Option<Target>);

/// Rows the Crew section needs when stacked.
pub fn crew_rows(state: &State) -> usize {
    let n = state.crew.runs.len();
    if n == 0 {
        1
    } else {
        n.min(RAIL_RUNS) * 2 + usize::from(n > RAIL_RUNS)
    }
}

fn tabbed_box(r: Rect, state: &State, theme: &Theme, buf: &mut Buffer) {
    let focus = focus_level(state, Focus::Board);
    let active = state.board_tab;
    let strip = tab_strip(r.width.saturating_sub(4) as usize, active);
    let mut title: Vec<Span<'static>> = Vec::new();
    for (i, (t, _, _)) in strip.cells.iter().enumerate() {
        if i > 0 {
            title.push(Span::raw(" ".repeat(strip.sep)));
        }
        let on = *t == active;
        let over = hovered(state, &Target::Tab(*t));
        let num = match (on, over) {
            (true, _) => label_style(focus, theme),
            (false, true) => ink(Ink::Hover, theme),
            (false, false) => ink(Ink::Dimmer, theme),
        };
        title.push(Span::styled(t.key().to_string(), num));
        if on || !strip.compact {
            let name = match (on, over) {
                (true, _) => label_style(focus, theme),
                (false, true) => ink(Ink::Hover, theme),
                (false, false) => ink(Ink::Dim, theme),
            };
            title.push(Span::styled(format!(" {}", t.label()), name));
        }
    }
    let taken = Line::from(title.clone()).width() as u16 + 3;
    let inner = region_box(r, title, "(1-4)", focus, theme, buf);
    let w = inner.width.saturating_sub(3) as usize;
    let rows = inner.height as usize;
    let (lines, count) = section_lines(active, state, w, rows, rows.saturating_sub(1) / 2, theme);
    if let Some(c) = count {
        border_right(r, r.y, &Line::from(Span::styled(c, ink(Ink::Dim, theme))), taken, buf);
    }
    draw_lines(inner, lines, state, theme, buf);
    if focus == FocusLevel::Focused {
        border_left(r, &hints(&[("↵", "open"), ("←→", "section")], theme), buf);
    }
}

fn stacked_box(r: Rect, tab: BoardTab, state: &State, theme: &Theme, buf: &mut Buffer) {
    let focus = match focus_level(state, Focus::Board) {
        FocusLevel::Focused if state.board_tab == tab => FocusLevel::Focused,
        _ if hovered(state, &Target::Tab(tab)) && state.overlay.is_none() => FocusLevel::Hovered,
        _ => FocusLevel::Unfocused,
    };
    let title = titled(tab.label(), None, focus, theme);
    let taken = Line::from(title.clone()).width() as u16 + 3;
    let inner = region_box(r, title, &format!("({})", tab.key()), focus, theme, buf);
    let w = inner.width.saturating_sub(3) as usize;
    let (lines, count) = section_lines(tab, state, w, inner.height as usize, RAIL_RUNS, theme);
    if let Some(c) = count {
        border_right(r, r.y, &Line::from(Span::styled(c, ink(Ink::Dim, theme))), taken, buf);
    }
    draw_lines(inner, lines, state, theme, buf);
    if focus == FocusLevel::Focused && tab != BoardTab::Threads {
        border_left(r, &hints(&[("↵", "open")], theme), buf);
    }
}

/// A section's rows; a row with a target is a button, lit under the pointer.
fn draw_lines(inner: Rect, rows: Vec<Row>, state: &State, theme: &Theme, buf: &mut Buffer) {
    let w = inner.width.saturating_sub(2);
    for (i, (l, t)) in rows.into_iter().take(inner.height as usize).enumerate() {
        let y = inner.y + i as u16;
        let l = match &t {
            Some(t) => {
                hit(state, Rect { x: inner.x, y, width: inner.width, height: 1 }, t.clone());
                if hovered(state, t) { lit(l, theme) } else { l }
            }
            None => l,
        };
        put(buf, inner.x + 1, y, &l, w);
    }
}

/// A section's lines and its count, for `rows` rows `w` wide.
fn section_lines(
    tab: BoardTab,
    state: &State,
    w: usize,
    rows: usize,
    max_runs: usize,
    theme: &Theme,
) -> (Vec<Row>, Option<String>) {
    match tab {
        BoardTab::Crew => super::crew::rail_lines(state, w, max_runs, theme),
        BoardTab::Watch => watch_lines(state, w, rows, theme),
        BoardTab::Threads => (vec![(dimmer("no open threads", theme), None)], None),
        BoardTab::Memory => {
            let lines = memory_lines(state, w)
                .into_iter()
                .map(|(s, i)| (Line::from(Span::styled(s, ink(i, theme))), None))
                .collect();
            (lines, None)
        }
    }
}

fn dimmer(s: &str, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(s.to_string(), ink(Ink::Dimmer, theme)))
}

/// Memory: the store's state, an import or offer, and what was recalled into this session.
/// Plain text and an ink, so the layout can count the rows without a theme.
pub fn memory_lines(state: &State, w: usize) -> Vec<(String, Ink)> {
    let mut v: Vec<(String, Ink)> = Vec::new();
    if let Some(i) = state.import {
        v.push((
            format!("importing… {} read · {} in", text::thousands(i.read), text::thousands(i.imported)),
            Ink::Accent,
        ));
    }
    match &state.memory.stats {
        Some(s) => {
            v.push((format!("{} memories · {} episodes", text::thousands(s.beliefs), text::thousands(s.episodes)), Ink::Body));
            if s.degraded.is_some() {
                v.push(("degraded: keyword search only".into(), Ink::Amber));
            }
        }
        None if state.wake.memory_presence == Presence::Absent => v.push(("memory not running".into(), Ink::Red)),
        None => v.push(("waiting for its stats…".into(), Ink::Dimmer)),
    }
    if state.legacy.is_some() && state.import.is_none() {
        v.push(("old harness memories to import · ^k".into(), Ink::Amber));
    }
    let recalls: Vec<(u32, &str)> = state
        .entries
        .iter()
        .filter_map(|e| match e {
            Entry::Recall { count, summary } => Some((*count, summary.as_str())),
            _ => None,
        })
        .collect();
    match recalls.last() {
        None => v.push(("nothing recalled this session".into(), Ink::Dimmer)),
        Some((_, summary)) => {
            let total: u32 = recalls.iter().map(|r| r.0).sum();
            let turns = recalls.len();
            v.push((
                format!("{total} recalled over {turns} turn{}", if turns == 1 { "" } else { "s" }),
                Ink::Dim,
            ));
            let first = summary.lines().next().unwrap_or("").trim();
            if !first.is_empty() {
                v.push((format!("⋯ {}", text::truncate(first, w.saturating_sub(2))), Ink::Dimmer));
            }
        }
    }
    v.into_iter().map(|(s, i)| (text::truncate(&s, w), i)).collect()
}

/// Watch: runs (two lines each, most urgent first), then unseen notices not about a listed run.
/// Returns the lines and the section's count.
fn watch_lines(state: &State, w: usize, rows: usize, theme: &Theme) -> (Vec<Row>, Option<String>) {
    let wake = &state.wake;
    let Some(ws) = &wake.watch else {
        let empty = match wake.sentinel {
            Presence::Absent => "sentinel not running",
            _ => "nothing watched",
        };
        return (vec![(dimmer(empty, theme), None)], None);
    };
    if ws.roots.is_empty() {
        let mut v = vec![(dimmer("nothing watched", theme), None)];
        let offers = wake.open_offers();
        if !offers.is_empty() {
            let n = offers.len();
            v.push((
                Line::from(vec![
                    Span::styled(format!("{n} offer{} ", if n == 1 { "" } else { "s" }), ink(Ink::Dim, theme)),
                    Span::styled("· ^w", ink(Ink::Dimmer, theme)),
                ]),
                Some(Target::Watched(offers[0].path.clone())),
            ));
        }
        return (v, None);
    }
    let alive = ws.runs.iter().filter(|r| r.status == RunStatus::Alive).count();
    let count = Some(format!("{alive} alive"));
    let mut out: Vec<Row> = Vec::new();
    let runs = wake.runs();
    let mut listed = 0;
    for run in &runs {
        if out.len() + 2 > rows.saturating_sub(1).max(2) {
            break;
        }
        let (glyph, sev) = run_glyph(run.status);
        let gstyle = match run.status {
            RunStatus::Alive => ink(Ink::Green, theme),
            RunStatus::Ended => ink(Ink::Dim, theme),
            _ => severity_style(sev, theme),
        };
        let name = text::truncate(&text::clean(&run.name), w.saturating_sub(2));
        let at = Some(Target::Watched(run.dir.clone()));
        out.push((Line::from(vec![Span::styled(format!("{glyph} "), gstyle), Span::styled(name, theme.normal())]), at.clone()));
        let detail = run_detail(run, state.now_ms);
        let dstyle = match run.status {
            RunStatus::Stalled => ink(Ink::Amber, theme),
            RunStatus::Failed => ink(Ink::Red, theme),
            _ => ink(Ink::Dim, theme),
        };
        out.push((Line::from(vec![Span::raw("  "), Span::styled(text::truncate(&detail, w.saturating_sub(2)), dstyle)]), at));
        listed += 1;
    }
    let hidden = runs.len() - listed;
    if runs.is_empty() {
        out.push((dimmer("no runs found yet", theme), None));
    }
    // Notices not about a listed run: unseen first, most severe first.
    let dirs: Vec<String> = runs.iter().take(listed).map(|r| crate::wake::norm(&r.dir)).collect();
    let mut notices: Vec<_> = wake
        .notices
        .iter()
        .filter(|n| !n.seen && n.severity > Severity::Info)
        .filter(|n| !dirs.iter().any(|d| crate::wake::norm(&n.subject).starts_with(d.as_str())))
        .collect();
    notices.sort_by_key(|n| std::cmp::Reverse((n.severity, n.at_ms)));
    for n in notices.into_iter().take(BOARD_NOTICES) {
        if out.len() + 1 > rows.saturating_sub(usize::from(hidden > 0)) {
            break;
        }
        out.push((
            Line::from(vec![
                Span::styled(format!("{} ", notice_glyph(n.severity)), severity_style(n.severity, theme)),
                Span::styled(text::truncate(&text::clean(&n.title), w.saturating_sub(2)), ink(Ink::Dim, theme)),
            ]),
            Some(Target::Watched(n.subject.clone())),
        ));
    }
    if hidden > 0 {
        out.push((dimmer(&format!("+{hidden} more · ^w"), theme), Some(Target::Watched(String::new()))));
    }
    (out, count)
}
