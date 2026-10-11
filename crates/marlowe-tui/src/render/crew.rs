//! The crew on screen: the CREW rail section, the run list and the Run view (alt-r), and the ^d
//! dispatch prompt. Foreground only; the prompt's `Clear` resets cells to the terminal default.

use marlowe_proto::{CrewRunInfo, CrewRunStatus, CrewStep, Dispatch, Playbook, SandboxKind, StageState};
use marlowe_surface::chrome::Ink;
use marlowe_surface::{markdown, Theme};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Widget};

use super::{hit, hovered, ink, lit, note_style, put, put_row};
use crate::crew::{
    discardable, mergeable, report_artifact, run_detail, status_glyph, status_word, ResearchTrack, RunConfirm,
    RunView, PLAYBOOKS, STEP_TAIL, STRIP,
};
use crate::state::{State, Target};
use crate::text;
use crate::wake::Presence;

/// Changed files listed before `+N more`.
const DIFF_FILES: usize = 12;
/// Citations listed under a result.
const CITATIONS: usize = 8;
/// Width of the dispatch prompt.
const PROMPT_W: u16 = 84;

fn dimmer(s: &str, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(s.to_string(), ink(Ink::Dimmer, theme)))
}

fn section(title: &'static str, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(title, ink(Ink::Accent, theme)))
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

// ---- rail ---------------------------------------------------------------------------------------

/// Crew on the board: two lines per run (at most `max_runs`), most urgent first. Returns the lines
/// and the section's count.
pub fn rail_lines(state: &State, w: usize, max_runs: usize, theme: &Theme) -> (Vec<super::board::Row>, Option<String>) {
    let crew = &state.crew;
    if crew.runs.is_empty() {
        let empty = if crew.presence == Presence::Absent { "crew not running" } else { "no runs" };
        return (vec![(dimmer(empty, theme), None)], None);
    }
    let needs = crew.count(CrewRunStatus::NeedsYou);
    let running = crew.count(CrewRunStatus::Running);
    let count = if needs > 0 {
        Some(format!("{needs} need{} you", if needs == 1 { "s" } else { "" }))
    } else if running > 0 {
        Some(format!("{running} running"))
    } else {
        None
    };
    let runs = crew.ordered();
    let mut out = Vec::new();
    let max_runs = max_runs.max(1);
    for r in runs.iter().take(max_runs) {
        let (glyph, gink) = status_glyph(r.status);
        let role = text::clean(&r.role);
        let title = text::truncate(&text::clean_maths(&r.title), w.saturating_sub(5 + text::width(&role)));
        let at = Some(Target::Run(r.id.clone()));
        out.push((Line::from(vec![
            Span::styled(format!("{glyph} "), ink(gink, theme)),
            Span::styled(role, ink(Ink::Dim, theme)),
            Span::styled(" · ", ink(Ink::Dimmer, theme)),
            Span::styled(title, theme.normal()),
        ]), at.clone()));
        let dstyle = match r.status {
            CrewRunStatus::NeedsYou => ink(Ink::Amber, theme),
            CrewRunStatus::Failed => ink(Ink::Red, theme),
            _ => ink(Ink::Dim, theme),
        };
        let detail = text::truncate(&run_detail(r, state.now_ms), w.saturating_sub(2));
        out.push((Line::from(vec![Span::raw("  "), Span::styled(detail, dstyle)]), at));
    }
    let hidden = runs.len().saturating_sub(max_runs);
    if hidden > 0 {
        out.push((dimmer(&format!("+{hidden} more · alt-r"), theme), Some(Target::Chord(crate::state::Command::Crew))));
    }
    (out, count)
}

// ---- crew view ----------------------------------------------------------------------------------

fn content(main: Rect) -> Rect {
    Rect {
        x: main.x + 3,
        y: main.y + 2,
        width: main.width.saturating_sub(6),
        height: main.height.saturating_sub(4),
    }
}

pub fn cursor(main: Rect, state: &State) -> Option<(u16, u16)> {
    let c = content(main);
    let open = state.crew.open.as_ref()?;
    if open.plan_edit.as_ref().is_some_and(|e| e.typing.is_some()) {
        return open.plan_cursor.get();
    }
    let v = Some(open).filter(|v| v.steering)?;
    let before = &v.steer.text()[..v.steer.cursor()];
    let col = text::width(&text::clean(before)).min(c.width.saturating_sub(10) as usize) as u16;
    Some((c.x + 8 + col, c.bottom() - 1))
}

pub fn draw(main: Rect, state: &State, theme: &Theme, buf: &mut Buffer) {
    let c = content(main);
    if c.width < 40 || c.height < 8 {
        return;
    }
    let hints: &[(&str, &str)] = match (&state.crew.open, state.crew.open_run()) {
        (Some(v), Some(r)) => {
            draw_run(c, state, v, r, theme, buf);
            match &v.plan_edit {
                Some(e) if e.typing.is_some() => &[("↵", "done"), ("esc", "cancel")],
                Some(_) => &[("↑↓", "row"), ("↵", "type"), ("←→", "adjust"), ("d", "drop"), ("s", "send"), ("esc", "discard")],
                None if v.confirm.is_none() && state.crew.plan(&r.id).is_some() => {
                    &[("a", "approve plan"), ("e", "edit"), ("x", "reject"), ("p", "pause"), ("↑↓", "scroll"), ("esc/⌫", "runs")]
                }
                None => run_hints(v, r),
            }
        }
        (Some(_), None) => {
            put(buf, c.x, c.y, &Line::from(Span::styled("loading the run…", ink(Ink::Dim, theme))), c.width);
            &[("esc/⌫", "back")]
        }
        (None, _) => {
            draw_list(c, state, theme, buf);
            &[("↵", "open"), ("↑↓", "select"), ("^d", "dispatch"), ("r", "refresh"), ("esc/⌫", "back")]
        }
    };
    footer(main, state, hints, theme, buf);
}

fn run_hints(v: &RunView, r: &CrewRunInfo) -> &'static [(&'static str, &'static str)] {
    if v.confirm.is_some() {
        return &[("y", "yes"), ("n", "no")];
    }
    if v.steering {
        return &[("↵", "steer"), ("esc", "done")];
    }
    let research = r.playbook == Some(Playbook::DeepResearch);
    match (r.status.is_terminal(), mergeable(r), discardable(r)) {
        (_, true, _) if research => &[("m", "accept report"), ("d", "discard"), ("↑↓", "scroll"), ("esc/⌫", "runs")],
        (_, true, _) => &[("m", "merge"), ("d", "discard"), ("↑↓", "scroll"), ("esc/⌫", "runs")],
        (true, false, true) => &[("d", "discard"), ("↑↓", "scroll"), ("esc/⌫", "runs")],
        (true, false, false) => &[("↑↓", "scroll"), ("esc/⌫", "runs")],
        (false, false, _) if r.status == CrewRunStatus::Paused => {
            &[("s", "steer"), ("p", "resume"), ("c", "cancel"), ("↑↓", "scroll"), ("esc/⌫", "runs")]
        }
        (false, false, _) => &[("s", "steer"), ("p", "pause"), ("c", "cancel"), ("↑↓", "scroll"), ("esc/⌫", "runs")],
    }
}

fn footer(main: Rect, state: &State, hints: &[(&str, &str)], theme: &Theme, buf: &mut Buffer) {
    let dim = ink(Ink::Dim, theme);
    let dimmer = ink(Ink::Dimmer, theme);
    let mut spans = vec![Span::raw(" ")];
    for (i, (k, v)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ·  ", dimmer));
        }
        spans.push(Span::styled(*k, dim));
        spans.push(Span::styled(format!(" {v}"), dimmer));
    }
    spans.push(Span::raw(" "));
    let l = Line::from(spans);
    let hw = l.width() as u16;
    put(buf, main.right().saturating_sub(hw + 2), main.bottom() - 1, &l, hw);
    if let Some(f) = &state.flash {
        let max = main.width.saturating_sub(hw + 8);
        let fl = Line::from(Span::styled(
            format!(" {} ", text::truncate(&text::clean(&f.text), max as usize)),
            note_style(f.tone, theme),
        ));
        put(buf, main.x + 2, main.bottom() - 1, &fl, max + 2);
    }
}

/// The run list: one row per run, most urgent first.
fn draw_list(c: Rect, state: &State, theme: &Theme, buf: &mut Buffer) {
    let crew = &state.crew;
    let w = c.width as usize;
    let dim = ink(Ink::Dim, theme);
    let accent = ink(Ink::Accent, theme);
    let mut lines: Vec<(Line<'static>, Option<Target>)> = Vec::new();
    let mut head = vec![Span::styled("Crew", accent.add_modifier(Modifier::BOLD))];
    if !crew.runs.is_empty() {
        let mut parts = vec![format!("{} run{}", crew.runs.len(), plural(crew.runs.len()))];
        for (s, word) in [(CrewRunStatus::NeedsYou, "need you"), (CrewRunStatus::Running, "running")] {
            let n = crew.count(s);
            if n > 0 {
                parts.push(format!("{n} {word}"));
            }
        }
        head.push(Span::styled(format!("  ·  {}", parts.join(" · ")), dim));
    }
    lines.push((Line::from(head), None));
    lines.push((Line::from(""), None));
    if crew.runs.is_empty() {
        let msg = if crew.presence == Presence::Absent {
            "The crew is not running in this daemon."
        } else {
            "No runs yet. ^d dispatches one: a role, a goal, and optionally a playbook."
        };
        lines.push((Line::from(Span::styled(msg, dim)), None));
    }
    let rows = c.height as usize - lines.len();
    let runs = crew.ordered();
    let first = crew.sel.saturating_sub(rows.saturating_sub(1));
    for (i, r) in runs.iter().enumerate().skip(first).take(rows) {
        let sel = i == crew.sel;
        let (glyph, gink) = status_glyph(r.status);
        let target = Target::Run(r.id.clone());
        let marker = match (sel, hovered(state, &target)) {
            (true, _) => Span::styled("› ", accent),
            (false, true) => Span::styled("› ", ink(Ink::Hover, theme)),
            (false, false) => Span::raw("  "),
        };
        let title_w = (w / 3).clamp(20, 44);
        let room = w.saturating_sub(2 + 2 + 12 + title_w + 11 + 1);
        lines.push((Line::from(vec![
            marker,
            Span::styled(format!("{glyph} "), ink(gink, theme)),
            Span::styled(text::pad(&text::truncate(&text::clean(&r.role), 11), 12), dim),
            Span::styled(
                text::pad(&text::truncate(&text::clean_maths(&r.title), title_w - 1), title_w),
                if sel { theme.bright() } else { theme.normal() },
            ),
            Span::styled(text::pad(status_word(r.status), 11), ink(gink, theme)),
            Span::styled(text::truncate(&run_detail(r, state.now_ms), room), dim),
        ]), Some(target)));
    }
    for (i, (l, t)) in lines.into_iter().enumerate() {
        let y = c.y + i as u16;
        match t {
            Some(t) => put_row(state, buf, (c.x, y, c.width), l, t, theme),
            None => put(buf, c.x, y, &l, c.width),
        }
    }
}

/// The Run view: header, result (diff, report or summary), transcript, steer box.
fn draw_run(c: Rect, state: &State, v: &RunView, r: &CrewRunInfo, theme: &Theme, buf: &mut Buffer) {
    let w = c.width as usize;
    let dim = ink(Ink::Dim, theme);
    let dimmer = ink(Ink::Dimmer, theme);
    let (glyph, gink) = status_glyph(r.status);

    // Header: fixed at the top.
    let mut head: Vec<Line<'static>> = Vec::new();
    let word = status_word(r.status);
    let left = vec![
        Span::styled(format!("{glyph} "), ink(gink, theme)),
        Span::styled(text::clean(&r.role), ink(Ink::Accent, theme).add_modifier(Modifier::BOLD)),
        Span::styled(" · ", dimmer),
        Span::styled(text::truncate(&text::clean_maths(&r.title), w.saturating_sub(24 + text::width(&r.role))), theme.bright()),
    ];
    let lw: usize = left.iter().map(|s| s.width()).sum();
    let mut l = left;
    l.push(Span::raw(" ".repeat(w.saturating_sub(lw + text::width(word)))));
    l.push(Span::styled(word, ink(gink, theme)));
    head.push(Line::from(l));
    head.push(Line::from(Span::styled(text::truncate(&progress_line(r, state.now_ms), w), dim)));
    head.push(Line::from(Span::styled(text::truncate(&where_line(r), w), dimmer)));
    if r.playbook == Some(Playbook::DeepResearch) {
        if let Some(t) = state.crew.research(&r.id) {
            head.extend(stage_strip(t, w, theme));
        }
    }
    if let Some(d) = state.crew.dag(&r.id) {
        head.extend(dag_strip(d, w, theme));
    }
    for (i, g) in text::wrap(&text::clean(r.goal.trim()), w.saturating_sub(6)).into_iter().enumerate().take(3) {
        let lead = if i == 0 { "goal  " } else { "      " };
        head.push(Line::from(vec![Span::styled(lead, dimmer), Span::styled(g, theme.normal())]));
    }
    // A waiting plan's note says what its panel shows.
    let plan_shown = state.crew.plan(&r.id).is_some();
    if let Some(n) = r.note.as_deref().filter(|n| !n.trim().is_empty() && !plan_shown) {
        let st = match r.status {
            CrewRunStatus::Failed => ink(Ink::Red, theme),
            CrewRunStatus::NeedsYou => ink(Ink::Amber, theme),
            _ => dim,
        };
        for l in text::wrap(&text::clean(n.trim()), w).into_iter().take(3) {
            head.push(Line::from(Span::styled(l, st)));
        }
    }
    head.push(Line::from(""));

    // The bottom line: a confirmation, the steer box, or nothing.
    let bottom = bottom_line(v, r, w, theme);
    let head_h = head.len().min(c.height as usize);
    for (i, l) in head.iter().take(head_h).enumerate() {
        put(buf, c.x, c.y + i as u16, l, c.width);
    }
    let under = |taken: usize| Rect {
        y: c.y + taken as u16,
        height: (c.height as usize).saturating_sub(taken + usize::from(bottom.is_some()) * 2) as u16,
        ..c
    };
    // A waiting plan sits under the header, fixed, with its buttons; its editor takes the body.
    if let Some(e) = &v.plan_edit {
        super::plan::editor(under(head_h), state, v, e, theme, buf);
        if let Some(b) = bottom {
            put(buf, c.x, c.bottom() - 1, &b, c.width);
        }
        return;
    }
    let plan_h = if state.crew.plan(&r.id).is_some() {
        let room = under(head_h);
        // The transcript keeps a few rows; the plan gets the rest.
        let cap = Rect { height: room.height.saturating_sub(4).max(room.height.min(8)), ..room };
        super::plan::panel(cap, state, r, theme, buf) as usize
    } else {
        0
    };
    let head_h = head_h + plan_h;

    // Body: scrolls.
    let mut body: Vec<Line<'static>> = Vec::new();
    if let Some(d) = &r.result {
        result_lines(r, d, v, w, theme, &mut body);
        body.push(Line::from(""));
    }
    transcript_lines(v, w, theme, &mut body);
    let body_h = (c.height as usize).saturating_sub(head_h + usize::from(bottom.is_some()) * 2);
    let total = body.len();
    let max = total.saturating_sub(body_h);
    v.scroll_max.set(max);
    let scroll = v.scroll.min(max);
    // A run with a result reads from the top; a working run follows its tail.
    let start = if r.result.is_some() { scroll } else { max - scroll };
    for (i, l) in body.iter().skip(start).take(body_h).enumerate() {
        put(buf, c.x, c.y + (head_h + i) as u16, l, c.width);
    }
    if total > body_h {
        let more = if r.result.is_some() { total - start - body_h.min(total - start) } else { start };
        if more > 0 {
            let arrow = if r.result.is_some() { "↓" } else { "↑" };
            let tag = Line::from(Span::styled(format!(" {arrow} {more} more "), dim));
            let tw = tag.width() as u16;
            let y = c.y + (head_h + body_h).saturating_sub(1) as u16;
            put(buf, c.right().saturating_sub(tw), y, &tag, tw);
        }
    }
    if let Some(b) = bottom {
        let y = c.bottom() - 1;
        if v.confirm.is_none() && !v.steering && !r.status.is_terminal() && !mergeable(r) {
            // The idle steer line is a button: a click puts the keys in it.
            let b = if hovered(state, &Target::Steer) { lit(b, theme) } else { b };
            hit(state, Rect { x: c.x, y, width: c.width, height: 1 }, Target::Steer);
            put(buf, c.x, y, &b, c.width);
        } else {
            put(buf, c.x, y, &b, c.width);
        }
    }
}

/// A research run's stages and counts, from its `ResearchProgress` reports:
///
/// ```text
/// ✓ Plan → ● Explore 3/8 → ○ Compress → ○ Corroborate → ○ Verify → ○ Synthesise
/// sources 42 found · 30 fetched · claims 118 · independent roots 9 · citations 24 supported · 2 unsupported
/// reading arXiv:2401.01234
/// ```
/// The build run's stage line and its task graph, one line per level of dependencies. Waiting,
/// running, done, failed, blocked on a hub change, re-running one at a time.
pub(super) fn dag_strip(d: &crate::dag::BuildDag, w: usize, theme: &Theme) -> Vec<Line<'static>> {
    use crate::dag::{TaskState, STAGES};
    let dim = ink(Ink::Dim, theme);
    let dimmer = ink(Ink::Dimmer, theme);
    let at = d.stage_index();
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, stage) in STAGES.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" → ", dimmer));
        }
        let (glyph, gstyle, nstyle) = match at {
            Some(c) if i < c => ("✓", ink(Ink::Green, theme), dim),
            Some(c) if i == c => ("●", ink(Ink::Accent, theme), theme.bright()),
            _ => ("○", dimmer, dimmer),
        };
        spans.push(Span::styled(format!("{glyph} "), gstyle));
        spans.push(Span::styled(*stage, nstyle));
    }
    let mut out = vec![Line::from(spans)];
    if d.tasks.is_empty() {
        return out;
    }
    out.push(Line::from(Span::styled(d.summary(), dim)));
    for (n, level) in d.levels().into_iter().enumerate() {
        let mut spans: Vec<Span<'static>> = vec![Span::styled(format!("{:>2}  ", n + 1), dimmer)];
        for t in level {
            let (glyph, gstyle) = match t.state {
                TaskState::Waiting => ("○", dimmer),
                TaskState::Running => ("●", ink(Ink::Accent, theme)),
                TaskState::Done => ("✓", ink(Ink::Green, theme)),
                TaskState::Failed => ("✗", ink(Ink::Red, theme)),
                TaskState::Blocked => ("◐", ink(Ink::Amber, theme)),
                TaskState::Serial => ("↺", ink(Ink::Amber, theme)),
            };
            spans.push(Span::styled(format!("{glyph} "), gstyle));
            spans.push(Span::styled(text::truncate(&text::clean(&t.id), 24), if t.state == TaskState::Running { theme.bright() } else { theme.normal() }));
            if !t.deps.is_empty() {
                let deps = t.deps.iter().map(|x| text::clean(x)).collect::<Vec<_>>().join(",");
                spans.push(Span::styled(format!(" ← {}", text::truncate(&deps, 24)), dimmer));
            }
            spans.push(Span::raw("   "));
        }
        // A level too wide for the line keeps what fits.
        let mut line = Line::from(spans);
        while line.width() > w && line.spans.len() > 3 {
            line.spans.truncate(line.spans.len() - 1);
        }
        out.push(line);
    }
    if !d.hubs.is_empty() && at.is_some_and(|c| c >= 2) {
        let hubs = d.hubs.iter().map(|h| text::clean(h)).collect::<Vec<_>>().join(", ");
        out.push(Line::from(Span::styled(format!("frozen after the contract: {}", text::truncate(&hubs, w.saturating_sub(28))), dimmer)));
    }
    out
}

fn stage_strip(t: &ResearchTrack, w: usize, theme: &Theme) -> Vec<Line<'static>> {
    let dim = ink(Ink::Dim, theme);
    let dimmer = ink(Ink::Dimmer, theme);
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, stage) in STRIP.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" → ", dimmer));
        }
        let name = stage_name(*stage);
        let (glyph, gstyle, nstyle) = match t.stage(*stage).map(|s| s.0) {
            None => ("○", dimmer, dimmer),
            Some(StageState::Done) => ("✓", ink(Ink::Green, theme), dim),
            Some(StageState::Started | StageState::Running) => ("●", ink(Ink::Accent, theme), theme.bright()),
            Some(StageState::NeedsYou) => ("◐", ink(Ink::Amber, theme), ink(Ink::Amber, theme)),
            Some(StageState::Failed) => ("✗", ink(Ink::Red, theme), ink(Ink::Red, theme)),
            Some(StageState::Skipped) => ("–", dimmer, dimmer),
        };
        spans.push(Span::styled(format!("{glyph} "), gstyle));
        spans.push(Span::styled(name, nstyle));
        if let Some((StageState::Started | StageState::Running, done, total)) = t.stage(*stage) {
            if total > 0 {
                spans.push(Span::styled(format!(" {done}/{total}"), dim));
            }
        }
    }
    let mut out = vec![Line::from(spans)];
    let c = &t.counts;
    let mut parts = Vec::new();
    if c.sources_found > 0 || c.sources_fetched > 0 {
        parts.push(format!("sources {} found · {} fetched", c.sources_found, c.sources_fetched));
    }
    if c.claims > 0 {
        parts.push(format!("claims {}", c.claims));
    }
    if c.independent_roots > 0 {
        parts.push(format!("independent roots {}", c.independent_roots));
    }
    if c.citations_supported + c.citations_partial + c.citations_unsupported > 0 {
        let mut cites = format!("citations {} supported", c.citations_supported);
        if c.citations_partial > 0 {
            cites.push_str(&format!(" · {} partial", c.citations_partial));
        }
        cites.push_str(&format!(" · {} unsupported", c.citations_unsupported));
        parts.push(cites);
    }
    if !parts.is_empty() {
        let unsupported = c.citations_unsupported > 0;
        let line = text::truncate(&parts.join(" · "), w);
        out.push(Line::from(Span::styled(line, if unsupported { ink(Ink::Amber, theme) } else { dim })));
    }
    if let Some(d) = t.detail.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        out.push(Line::from(Span::styled(text::truncate(&text::clean(d), w), dimmer)));
    }
    out
}

fn stage_name(s: marlowe_proto::ResearchStage) -> &'static str {
    use marlowe_proto::ResearchStage as S;
    match s {
        S::Plan => "Plan",
        S::Explore => "Explore",
        S::Compress => "Compress",
        S::Corroborate => "Corroborate",
        S::Verify => "Verify",
        S::Synthesise => "Synthesise",
        S::Remember => "Remember",
    }
}

/// `step 7 · $0.0213 of $0.50 · 41.2k tokens of 200k · 3m ago`.
fn progress_line(r: &CrewRunInfo, now_ms: i64) -> String {
    let mut parts = vec![match r.budget.steps {
        Some(m) => format!("step {} of {m}", r.step),
        None => format!("step {}", r.step),
    }];
    if r.spent.usd > 0.0 || r.budget.usd.is_some() {
        let s = text::cost(r.spent.usd);
        parts.push(match r.budget.usd {
            Some(m) => format!("{s} of {}", text::cost(m)),
            None => s,
        });
    }
    if r.spent.tokens > 0 {
        let s = format!("{} tokens", text::compact(r.spent.tokens));
        parts.push(match r.budget.tokens {
            Some(m) => format!("{s} of {}", text::compact(m)),
            None => s,
        });
    }
    if r.status == CrewRunStatus::Running {
        if let Some(a) = r.current_action.as_deref().filter(|a| !a.trim().is_empty()) {
            parts.push(text::clean(a.lines().next().unwrap_or("")));
        }
    }
    parts.push(text::ago(r.updated_ms, now_ms));
    parts.join(" · ")
}

/// `deep_research · scratch ~/AppData/…/scratch/ab12cd34` or `build · worktree crew/ab12cd34 of ~/Projects/x`.
fn where_line(r: &CrewRunInfo) -> String {
    let mut parts = vec![r.playbook.map_or("single dispatch", Playbook::as_str).to_string()];
    match &r.sandbox {
        Some(s) => match s.kind {
            SandboxKind::Worktree => {
                let branch = s.branch.clone().unwrap_or_else(|| format!("crew/{}", r.id.short()));
                let origin = s.origin.as_deref().map(|o| format!(" of {}", text::short_path(o))).unwrap_or_default();
                parts.push(format!("worktree {}{origin}", text::clean(&branch)));
            }
            SandboxKind::Scratch => parts.push(format!("scratch {}", text::short_path(&text::clean(&s.root)))),
            SandboxKind::None => parts.push("no sandbox".into()),
        },
        None => {
            if let Some(ws) = &r.workspace {
                parts.push(text::short_path(&text::clean(ws)));
            }
        }
    }
    parts.push(format!("run {}", r.id.short()));
    parts.join(" · ")
}

fn result_lines(r: &CrewRunInfo, d: &Dispatch, v: &RunView, w: usize, theme: &Theme, out: &mut Vec<Line<'static>>) {
    let dim = ink(Ink::Dim, theme);
    let dimmer = ink(Ink::Dimmer, theme);
    let speech = Style::default().fg(theme.speech());
    let conf = format!("confidence {}", confidence_word(d));

    if let Some(diff) = &d.diff {
        out.push(section("DIFF", theme));
        let n = diff.files.len();
        out.push(Line::from(vec![
            Span::styled(text::clean(&diff.branch), theme.normal()),
            Span::styled(format!(" · {n} file{} · ", plural(n)), dim),
            Span::styled(format!("+{}", diff.insertions), ink(Ink::Green, theme)),
            Span::styled(" ", dim),
            Span::styled(format!("−{}", diff.deletions), ink(Ink::Red, theme)),
        ]));
        for f in diff.files.iter().take(DIFF_FILES) {
            out.push(Line::from(vec![
                Span::styled("  ", dim),
                Span::styled(text::truncate(&text::clean(f), w.saturating_sub(2)), dim),
            ]));
        }
        if n > DIFF_FILES {
            out.push(dimmer_line(&format!("  +{} more", n - DIFF_FILES), dimmer));
        }
        out.push(Line::from(""));
        merge_prompt(r, w, theme, out);
        out.push(Line::from(""));
        out.push(section("SUMMARY", theme));
        out.extend(markdown::render_prose(&d.summary, w, theme, speech));
    } else if r.playbook == Some(Playbook::DeepResearch) {
        let mut head = vec![Span::styled("REPORT", ink(Ink::Accent, theme))];
        head.push(Span::styled(format!("  {conf} · {} source{}", d.citations.len(), plural(d.citations.len())), dimmer));
        out.push(Line::from(head));
        match &v.report {
            Some(rep) => out.extend(markdown::render_prose(rep, w, theme, speech)),
            None => {
                out.extend(markdown::render_prose(&d.summary, w, theme, speech));
                if let Some(p) = report_artifact(d) {
                    out.push(dimmer_line(&format!("the report at {} could not be read", text::short_path(&text::clean(p))), dimmer));
                }
            }
        }
        citations(d, w, theme, out);
        out.push(Line::from(""));
        merge_prompt(r, w, theme, out);
    } else {
        let mut head = vec![Span::styled("RESULT", ink(Ink::Accent, theme))];
        head.push(Span::styled(format!("  {conf}"), dimmer));
        out.push(Line::from(head));
        out.extend(markdown::render_prose(&d.summary, w, theme, speech));
        citations(d, w, theme, out);
        for a in &d.artifacts {
            out.push(Line::from(vec![
                Span::styled("▸ ", dimmer),
                Span::styled(text::truncate(&text::short_path(&text::clean(a)), w.saturating_sub(2)), dim),
            ]));
        }
    }
}

fn dimmer_line(s: &str, style: Style) -> Line<'static> {
    Line::from(Span::styled(s.to_string(), style))
}

fn confidence_word(d: &Dispatch) -> &'static str {
    match d.confidence {
        marlowe_proto::Confidence::Low => "low",
        marlowe_proto::Confidence::Medium => "medium",
        marlowe_proto::Confidence::High => "high",
    }
}

fn citations(d: &Dispatch, w: usize, theme: &Theme, out: &mut Vec<Line<'static>>) {
    if d.citations.is_empty() {
        return;
    }
    out.push(Line::from(""));
    out.push(section("SOURCES", theme));
    for (i, c) in d.citations.iter().take(CITATIONS).enumerate() {
        let n = format!("{:>2} ", i + 1);
        let title = text::clean_maths(&c.title);
        let tw = text::width(&title).min(w / 2);
        let room = w.saturating_sub(3 + tw + 3);
        out.push(Line::from(vec![
            Span::styled(n, ink(Ink::Dimmer, theme)),
            Span::styled(text::truncate(&title, w / 2), theme.normal()),
            Span::styled(" · ", ink(Ink::Dimmer, theme)),
            Span::styled(text::truncate(&text::clean(&c.url_or_path), room), ink(Ink::Dim, theme)),
        ]));
    }
    if d.citations.len() > CITATIONS {
        out.push(dimmer_line(&format!("   +{} more", d.citations.len() - CITATIONS), ink(Ink::Dimmer, theme)));
    }
}

/// `m merge crew/ab12cd34 into ~/Projects/x (asks first) · d discard`, amber while it waits.
fn merge_prompt(r: &CrewRunInfo, w: usize, theme: &Theme, out: &mut Vec<Line<'static>>) {
    if !mergeable(r) {
        return;
    }
    let accent = ink(Ink::Accent, theme);
    let tone = if r.status == CrewRunStatus::NeedsYou { ink(Ink::Amber, theme) } else { ink(Ink::Dim, theme) };
    let what = match (&r.result.as_ref().and_then(|d| d.diff.as_ref()), &r.sandbox) {
        (Some(d), Some(s)) => {
            let into = s.origin.as_deref().map(|o| format!(" into {}", text::short_path(o))).unwrap_or_default();
            format!(" merge {}{into}, after your approval; nothing is pushed", text::clean(&d.branch))
        }
        (Some(d), None) => format!(" merge {}, after your approval; nothing is pushed", text::clean(&d.branch)),
        (None, _) => {
            let into = r.workspace.as_deref().map(|w| format!(" into {}/research", text::short_path(w))).unwrap_or_default();
            format!(" accept: copy the report{into}, after your approval")
        }
    };
    let mut spans = vec![Span::styled("m", accent), Span::styled(text::truncate(&what, w.saturating_sub(16)), tone)];
    if discardable(r) {
        spans.push(Span::styled("  ·  ", ink(Ink::Dimmer, theme)));
        spans.push(Span::styled("d", accent));
        spans.push(Span::styled(" discard", tone));
    }
    out.push(Line::from(spans));
}

fn transcript_lines(v: &RunView, w: usize, theme: &Theme, out: &mut Vec<Line<'static>>) {
    out.push(section("TRANSCRIPT", theme));
    if !v.loaded && v.steps.is_empty() {
        out.push(dimmer("loading the transcript…", theme));
        return;
    }
    if v.steps.is_empty() {
        out.push(dimmer("no steps yet", theme));
        return;
    }
    let rule = ink(Ink::Dimmer, theme);
    for s in &v.steps {
        out.push(step_head(s, w, theme));
        let body = text::clean(s.text.trim());
        if body.is_empty() {
            continue;
        }
        let style = if s.role == "you" { theme.normal() } else { ink(Ink::Dim, theme) };
        let lines = text::wrap(&body, w.saturating_sub(5));
        let skip = lines.len().saturating_sub(STEP_TAIL);
        if skip > 0 {
            out.push(Line::from(vec![Span::styled("   ▏ ", rule), Span::styled(format!("… {skip} lines"), rule)]));
        }
        for l in lines.into_iter().skip(skip) {
            out.push(Line::from(vec![Span::styled("   ▏ ", rule), Span::styled(l, style)]));
        }
    }
}

fn step_head(s: &CrewStep, w: usize, theme: &Theme) -> Line<'static> {
    if s.role == "you" {
        return Line::from(vec![
            Span::styled("  › ", ink(Ink::Accent, theme)),
            Span::styled("steer", ink(Ink::Accent, theme)),
        ]);
    }
    let n = format!("{:>3} ", s.step);
    let role = text::clean(&s.role);
    let action = text::clean(s.action.lines().next().unwrap_or(""));
    let room = w.saturating_sub(4 + text::width(&role) + 3);
    Line::from(vec![
        Span::styled(n, ink(Ink::Dimmer, theme)),
        Span::styled(role, ink(Ink::Accent, theme)),
        Span::styled(" · ", ink(Ink::Dimmer, theme)),
        Span::styled(text::truncate(&action, room), theme.normal()),
    ])
}

fn bottom_line(v: &RunView, r: &CrewRunInfo, w: usize, theme: &Theme) -> Option<Line<'static>> {
    let accent = ink(Ink::Accent, theme);
    let amber = ink(Ink::Amber, theme);
    let dim = ink(Ink::Dim, theme);
    let dimmer = ink(Ink::Dimmer, theme);
    if let Some(c) = v.confirm {
        let (q, why) = match c {
            RunConfirm::Cancel => ("cancel this run?", "  its sandbox stays until you discard it  "),
            RunConfirm::Discard => match r.sandbox.as_ref().map(|s| s.kind) {
                Some(SandboxKind::Worktree) => ("discard this run?", "  the worktree and its branch are deleted  "),
                _ => ("discard this run?", "  its scratch directory is deleted  "),
            },
            RunConfirm::RejectPlan => ("reject this plan?", "  the run ends cancelled  "),
        };
        return Some(Line::from(vec![
            Span::styled(q, amber),
            Span::styled(why, dim),
            Span::styled("y", accent),
            Span::styled(" / ", dimmer),
            Span::styled("n", accent),
        ]));
    }
    if v.plan_edit.is_some() {
        return None;
    }
    if v.steering {
        let t = text::clean(v.steer.text());
        return Some(Line::from(vec![Span::styled("steer › ", accent), Span::styled(text::truncate(&t, w.saturating_sub(9)), theme.normal())]));
    }
    // A run waiting only on a merge has nothing left to steer.
    if !r.status.is_terminal() && !mergeable(r) {
        return Some(Line::from(vec![
            Span::styled("steer › ", ink(Ink::Structure, theme)),
            Span::styled("s to steer: a note the run reads at its next step", dimmer),
        ]));
    }
    None
}

// ---- dispatch prompt ----------------------------------------------------------------------------

/// The ^d prompt. Returns the cursor position in the goal line.
pub fn dispatch(area: Rect, state: &State, theme: &Theme, buf: &mut Buffer) -> Option<(u16, u16)> {
    let p = state.crew.prompt.as_ref()?;
    let accent = ink(Ink::Accent, theme);
    let dim = ink(Ink::Dim, theme);
    let dimmer = ink(Ink::Dimmer, theme);
    let roles = state.crew.roles.as_deref().unwrap_or(&[]);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let inner_w = PROMPT_W.saturating_sub(4) as usize;

    lines.push(section("ROLE", theme));
    if state.crew.roles.is_none() {
        lines.push(dimmer_line("loading roles…", dimmer));
    } else if roles.is_empty() {
        lines.push(dimmer_line("no role cards found", dimmer));
    }
    for (i, r) in roles.iter().enumerate() {
        let sel = i == p.role;
        let marker = if sel { Span::styled("› ", accent) } else { Span::raw("  ") };
        let name_style = if sel { accent.add_modifier(Modifier::BOLD) } else { theme.normal() };
        let desc = text::truncate(&text::clean(&r.description), inner_w.saturating_sub(16));
        lines.push(Line::from(vec![
            marker,
            Span::styled(text::pad(&text::truncate(&text::clean(&r.name), 12), 14), name_style),
            Span::styled(desc, if sel { dim } else { dimmer }),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(section("GOAL", theme));
    let goal_row = lines.len();
    let goal = text::clean(p.goal.text());
    lines.push(if goal.is_empty() {
        Line::from(vec![Span::styled("› ", accent), Span::styled("what should the run find out or build?", dimmer)])
    } else {
        Line::from(vec![Span::styled("› ", accent), Span::styled(goal.clone(), theme.normal())])
    });
    lines.push(Line::from(""));
    let mut pb = vec![Span::styled("PLAYBOOK  ", accent)];
    for (i, choice) in PLAYBOOKS.iter().enumerate() {
        if i > 0 {
            pb.push(Span::styled(" · ", dimmer));
        }
        let label = choice.map_or("none", Playbook::as_str);
        let st = if i == p.playbook { accent.add_modifier(Modifier::BOLD) } else { dimmer };
        pb.push(Span::styled(label, st));
    }
    lines.push(Line::from(pb));
    let role_name = roles.get(p.role).map(|r| text::clean(&r.name)).unwrap_or_else(|| "the role".into());
    let how = match p.playbook() {
        None => format!("{role_name} works the goal alone and reports back"),
        Some(Playbook::DeepResearch) => "plan → scouts in parallel → verify → a cited report".into(),
        Some(Playbook::Build) => "plan → implement in a worktree → test → review → merge approval".into(),
    };
    lines.push(Line::from(Span::styled(how, dim)));
    let ws = state.shown_workspace().map(|w| text::short_path(&text::clean(w))).unwrap_or_else(|| "no workspace".into());
    lines.push(Line::from(Span::styled(format!("in {ws} · returns at once; the result comes back here"), dimmer)));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("↵", dim),
        Span::styled(" dispatch  ·  ", dimmer),
        Span::styled("↑↓", dim),
        Span::styled(" role  ·  ", dimmer),
        Span::styled("tab", dim),
        Span::styled(" playbook  ·  ", dimmer),
        Span::styled("esc", dim),
        Span::styled(" cancel", dimmer),
    ]));

    let w = PROMPT_W.min(area.width.saturating_sub(4));
    let h = (lines.len() as u16 + 2).min(area.height.saturating_sub(2));
    let r = Rect { x: area.x + area.width.saturating_sub(w) / 2, y: area.y + area.height.saturating_sub(h) / 4, width: w, height: h };
    Clear.render(r, buf);
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(accent)
        .title(Span::styled(" dispatch ", accent))
        .render(r, buf);
    let inner = Rect { x: r.x + 2, y: r.y + 1, width: r.width.saturating_sub(4), height: r.height.saturating_sub(2) };
    for (i, l) in lines.iter().enumerate().take(inner.height as usize) {
        put(buf, inner.x, inner.y + i as u16, l, inner.width);
    }
    if goal_row >= inner.height as usize {
        return None;
    }
    let before = &p.goal.text()[..p.goal.cursor()];
    let col = text::width(&text::clean(before)).min(inner.width.saturating_sub(3) as usize) as u16;
    Some((inner.x + 2 + col, inner.y + goal_row as u16))
}
