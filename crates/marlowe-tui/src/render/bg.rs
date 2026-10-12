//! Wave 2 on screen (track H3): the `/tasks`, `/workflows`, `/findings`, `/files`, plan, `/bench`,
//! suggestions and thread panels; the conversation's news and file entries; and the task strip.
//! Foreground colours from the palette only, never a fill.

use marlowe_proto::{
    FindingSeverity, TaskInfo, TaskKind, TaskStatus, WorkflowAgentInfo, WorkflowAgentStatus, WorkflowInfo,
};
use marlowe_surface::chrome::{self, Ink};
use marlowe_surface::{markdown, Theme};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::overlay::{centred, panel, panel_hints, row_marker};
use super::panel::{first_row, frame, note, unavailable};
use super::{hit, ink, put, put_row};
use crate::bg::{severity_word, Delivery, ThreadFrom};
use crate::bg_panels::{
    kind_label, ordered_agents, sorted_findings, task_rows, workflow_runs, BenchUi, FindingsUi, PlanUi, SuggestUi, TasksUi,
    ThreadUi, WorkflowsUi, PLAN_CHOICES,
};
use crate::report::{self, Tone};
use crate::state::{Command, State, Target};
use crate::text;

// ---- small pieces -----------------------------------------------------------------------------

pub fn task_status_ink(s: TaskStatus) -> Ink {
    match s {
        TaskStatus::Running => Ink::Green,
        TaskStatus::Waiting => Ink::Amber,
        TaskStatus::Done => Ink::Dim,
        TaskStatus::Failed => Ink::Red,
        TaskStatus::Stopped => Ink::Dimmer,
    }
}

pub fn severity_ink(s: FindingSeverity) -> Ink {
    match s {
        FindingSeverity::Critical | FindingSeverity::High => Ink::Red,
        FindingSeverity::Medium => Ink::Amber,
        FindingSeverity::Low => Ink::Dim,
        FindingSeverity::Info => Ink::Dimmer,
    }
}

fn agent_glyph(s: WorkflowAgentStatus) -> (&'static str, Ink) {
    match s {
        WorkflowAgentStatus::Queued => ("\u{25cb}", Ink::Dim),
        WorkflowAgentStatus::Running => ("\u{25cf}", Ink::Accent),
        WorkflowAgentStatus::Done => ("\u{2713}", Ink::Green),
        WorkflowAgentStatus::Failed => ("\u{2717}", Ink::Red),
        WorkflowAgentStatus::Skipped => ("\u{2298}", Ink::Dimmer),
    }
}

fn agent_word(s: WorkflowAgentStatus) -> &'static str {
    match s {
        WorkflowAgentStatus::Queued => "queued",
        WorkflowAgentStatus::Running => "running",
        WorkflowAgentStatus::Done => "done",
        WorkflowAgentStatus::Failed => "failed",
        WorkflowAgentStatus::Skipped => "skipped",
    }
}

/// How long a task has run, or ran.
pub fn elapsed(t: &TaskInfo, now_ms: i64) -> String {
    let end = t.ended_ms.unwrap_or(now_ms);
    text::took((end - t.started_ms).max(0) as u64)
}

fn tone_ink(t: Tone) -> Ink {
    match t {
        Tone::Head => Ink::Accent,
        Tone::Body => Ink::Body,
        Tone::Dim => Ink::Dim,
        Tone::Good => Ink::Green,
        Tone::Warn => Ink::Amber,
        Tone::Bad => Ink::Red,
    }
}

fn marker(selected: bool, state: &State, theme: &Theme, i: usize) -> Span<'static> {
    row_marker(selected, super::hovered(state, &Target::Row(i)), theme)
}

// ---- /tasks -----------------------------------------------------------------------------------

pub fn tasks(area: Rect, state: &State, ui: &TasksUi, theme: &Theme, buf: &mut Buffer) {
    let rows = task_rows(&state.bg, state.session.as_ref(), ui);
    let h = if ui.output { area.height.saturating_sub(4) } else { (rows.len() as u16 + 7).clamp(10, area.height.saturating_sub(2)) };
    let hints: &[(&str, &str)] = if ui.output {
        &[("\u{2191}\u{2193}", "scroll"), ("r", "more"), ("esc", "back")]
    } else {
        &[("\u{21b5}", "output"), ("x", "stop"), ("tab", "kind"), ("s", "this session"), ("h", "hide done"), ("esc", "close")]
    };
    let inner = frame(area, 116, h, "tasks", hints, theme, buf);
    let dim = ink(Ink::Dim, theme);
    if ui.output {
        task_output(inner, state, ui, theme, buf);
        return;
    }
    let mut filter = vec![format!("kind {}", kind_label(ui.kind))];
    if ui.mine_only {
        filter.push("this session only".into());
    }
    if ui.hide_done {
        filter.push("finished hidden".into());
    }
    put(buf, inner.x, inner.y, &Line::from(Span::styled(filter.join(" \u{b7} "), dim)), inner.width);
    if !state.bg.tasks_known {
        let msg = if unavailable(state, "list_tasks") { "this daemon does not keep a task list yet" } else { "loading\u{2026}" };
        note(inner, inner.y + 2, msg, theme, buf);
        return;
    }
    if rows.is_empty() {
        note(inner, inner.y + 2, "nothing is running in the background \u{b7} a shell command with ctrl+b, a monitor, a workflow or a background agent shows up here", theme, buf);
        return;
    }
    let visible = (inner.height as usize).saturating_sub(2);
    let first = first_row(ui.sel, visible);
    for (i, t) in rows.iter().enumerate().skip(first).take(visible) {
        let sel = i == ui.sel;
        let y = inner.y + 2 + (i - first) as u16;
        let who = match (&t.session, &state.session) {
            (Some(s), Some(me)) if s != me => " \u{b7} other session",
            _ => "",
        };
        let detail = t.detail.as_deref().filter(|d| !d.trim().is_empty()).map(|d| format!(" \u{b7} {}", text::clean(d))).unwrap_or_default();
        let room = (inner.width as usize).saturating_sub(2 + 9 + 22 + 10 + 9);
        let title = text::truncate(&format!("{}{detail}{who}", text::clean(&t.title)), room);
        let spans = vec![
            marker(sel, state, theme, i),
            Span::styled(text::pad(t.kind.as_str(), 9), ink(Ink::Dim, theme)),
            Span::styled(text::pad(&text::truncate(&t.id, 21), 22), ink(Ink::Dimmer, theme)),
            Span::styled(text::pad(t.status.as_str(), 10), ink(task_status_ink(t.status), theme)),
            Span::styled(text::pad(&elapsed(t, state.now_ms), 9), ink(Ink::Dimmer, theme)),
            Span::styled(title, if sel { theme.bright() } else { theme.normal() }),
        ];
        put_row(state, buf, (inner.x, y, inner.width), Line::from(spans), Target::Row(i), theme);
    }
}

fn task_output(inner: Rect, state: &State, ui: &TasksUi, theme: &Theme, buf: &mut Buffer) {
    let Some(o) = state.bg.task_out.as_ref() else { return };
    let t = state.bg.task(&o.id);
    let status = t.map_or("", |t| t.status.as_str());
    let head = Line::from(vec![
        Span::styled(o.id.clone(), ink(Ink::Accent, theme)),
        Span::styled(format!(" \u{b7} {}", text::truncate(&text::clean(t.map_or("", |t| t.title.as_str())), inner.width as usize / 2)), theme.normal()),
        Span::styled(format!(" \u{b7} {status}"), ink(t.map_or(Ink::Dim, |t| task_status_ink(t.status)), theme)),
    ]);
    put(buf, inner.x, inner.y, &head, inner.width);
    let body = Rect { y: inner.y + 2, height: inner.height.saturating_sub(2), ..inner };
    let mut lines: Vec<Line<'static>> = Vec::new();
    if let Some(e) = &o.error {
        lines.push(Line::from(Span::styled(text::clean(e), ink(Ink::Amber, theme))));
    }
    for l in o.text.lines() {
        lines.push(Line::from(Span::styled(text::truncate(&text::clean(l), body.width as usize), theme.normal())));
    }
    if lines.is_empty() {
        let msg = if o.loading { "reading\u{2026}" } else if o.done { "no output" } else { "nothing yet \u{b7} it is read again every second while the task runs" };
        lines.push(Line::from(Span::styled(msg, ink(Ink::Dimmer, theme))));
    }
    let rows = body.height as usize;
    let max = lines.len().saturating_sub(rows);
    ui.scroll.max.set(max);
    // Anchored at the end, as the transcript is: `pos` counts rows up from the last line.
    let top = max.saturating_sub(ui.scroll.at());
    for (i, l) in lines.iter().skip(top).take(rows).enumerate() {
        put(buf, body.x, body.y + i as u16, l, body.width);
    }
    if lines.len() > rows {
        let tag = format!(" {}\u{2013}{} of {} ", top + 1, (top + rows).min(lines.len()), lines.len());
        let tw = text::width(&tag) as u16;
        put(buf, inner.right().saturating_sub(tw), inner.y, &Line::from(Span::styled(tag, ink(Ink::Dimmer, theme))), tw);
    }
}

// ---- /workflows -------------------------------------------------------------------------------

pub fn workflows(area: Rect, state: &State, ui: &WorkflowsUi, theme: &Theme, buf: &mut Buffer) {
    match ui.open.as_ref().and_then(|r| state.bg.workflow(r)) {
        Some(info) => workflow_run(area, state, ui, info, theme, buf),
        None if ui.open.is_some() => {
            let inner = frame(area, 100, 8, "workflow", &[("esc", "back")], theme, buf);
            note(inner, inner.y, "waiting for the run's first update\u{2026}", theme, buf);
        }
        None => workflow_list(area, state, ui, theme, buf),
    }
}

fn workflow_list(area: Rect, state: &State, ui: &WorkflowsUi, theme: &Theme, buf: &mut Buffer) {
    let runs = workflow_runs(state);
    let saved = &state.bg.saved;
    let h = (runs.len() + saved.len()) as u16 + 10;
    let inner = frame(
        area,
        108,
        h.clamp(12, area.height.saturating_sub(2)),
        "workflows",
        &[("\u{21b5}", "open or start"), ("r", "refresh"), ("\u{2191}\u{2193}", "select"), ("esc", "close")],
        theme,
        buf,
    );
    let dim = ink(Ink::Dim, theme);
    let mut y = inner.y;
    put(buf, inner.x, y, &Line::from(Span::styled("runs", ink(Ink::Accent, theme))), inner.width);
    y += 1;
    if !state.bg.workflows_known {
        let msg = if unavailable(state, "list_workflows") { "this daemon does not run workflows yet" } else { "loading\u{2026}" };
        note(inner, y, msg, theme, buf);
        y += 1;
    } else if runs.is_empty() {
        note(inner, y, "none yet \u{b7} /workflow <name> starts one", theme, buf);
        y += 1;
    }
    for (i, w) in runs.iter().enumerate() {
        if y >= inner.bottom().saturating_sub(2) {
            break;
        }
        let sel = i == ui.sel;
        let status = state.crew.get(&w.run).map(|r| r.status);
        let (glyph, gink) = status.map_or(("\u{25cb}", Ink::Dim), crate::crew::status_glyph);
        let done = w.agents.iter().filter(|a| a.status == WorkflowAgentStatus::Done).count();
        let phase = w.phase.as_deref().map(|p| format!(" \u{b7} {}", text::clean(p))).unwrap_or_default();
        let spans = vec![
            marker(sel, state, theme, i),
            Span::styled(format!("{glyph} "), ink(gink, theme)),
            Span::styled(text::pad(&text::truncate(&text::clean(&w.name), 30), 31), if sel { theme.bright() } else { theme.normal() }),
            Span::styled(text::pad(status.map_or("", crate::crew::status_word), 10), ink(gink, theme)),
            Span::styled(format!("{done} of {} agents{phase}", w.agents.len()), dim),
        ];
        put_row(state, buf, (inner.x, y, inner.width), Line::from(spans), Target::Row(i), theme);
        y += 1;
    }
    y += 1;
    if y < inner.bottom() {
        put(buf, inner.x, y, &Line::from(Span::styled("saved", ink(Ink::Accent, theme))), inner.width);
        y += 1;
    }
    for (j, w) in saved.iter().enumerate() {
        if y >= inner.bottom() {
            break;
        }
        let i = runs.len() + j;
        let sel = i == ui.sel;
        let spans = vec![
            marker(sel, state, theme, i),
            Span::styled(text::pad(&text::truncate(&text::clean(&w.name), 24), 25), if sel { theme.bright() } else { theme.normal() }),
            Span::styled(text::truncate(&text::clean(&w.description), (inner.width as usize).saturating_sub(40)), dim),
            Span::styled(format!("  {}", text::short_path(&text::clean(&w.source))), ink(Ink::Dimmer, theme)),
        ];
        put_row(state, buf, (inner.x, y, inner.width), Line::from(spans), Target::Row(i), theme);
        y += 1;
    }
}

fn workflow_run(area: Rect, state: &State, ui: &WorkflowsUi, info: &WorkflowInfo, theme: &Theme, buf: &mut Buffer) {
    let hints: &[(&str, &str)] = if ui.confirm_stop {
        &[("y", "stop it"), ("any", "keep going")]
    } else {
        &[("s", "skip agent"), ("x", "stop run"), ("m", "messages"), ("\u{2191}\u{2193}", "select"), ("esc", "back")]
    };
    let h = area.height.saturating_sub(4);
    let inner = frame(area, 112, h, &format!("workflow \u{b7} {}", text::truncate(&text::clean(&info.name), 40)), hints, theme, buf);
    let dim = ink(Ink::Dim, theme);
    let dimmer = ink(Ink::Dimmer, theme);
    let mut y = inner.y;
    // The phase strip: the script's phases in order, the current one lit.
    if !info.phases.is_empty() {
        let mut spans = Vec::new();
        for (i, p) in info.phases.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" \u{203a} ", dimmer));
            }
            let current = info.phase.as_deref() == Some(p.as_str());
            spans.push(Span::styled(text::clean(p), if current { ink(Ink::Accent, theme).add_modifier(Modifier::BOLD) } else { dim }));
        }
        put(buf, inner.x, y, &Line::from(spans), inner.width);
        y += 1;
    }
    let status = state.crew.get(&info.run).map(|r| r.status);
    let mut head = vec![];
    if let Some(s) = status {
        let (g, gi) = crate::crew::status_glyph(s);
        head.push(Span::styled(format!("{g} {}", crate::crew::status_word(s)), ink(gi, theme)));
        head.push(Span::styled(" \u{b7} ", dimmer));
    }
    let done = info.agents.iter().filter(|a| a.status == WorkflowAgentStatus::Done).count();
    head.push(Span::styled(format!("{done} of {} agents done", info.agents.len()), dim));
    if let Some(total) = info.budget_total {
        let hot = info.budget_spent * 10 >= total * 8;
        head.push(Span::styled(" \u{b7} ", dimmer));
        head.push(Span::styled(
            format!("budget {} of {}", text::compact(info.budget_spent), text::compact(total)),
            ink(if hot { Ink::Amber } else { Ink::Dim }, theme),
        ));
    }
    put(buf, inner.x, y, &Line::from(head), inner.width);
    y += 2;
    if ui.confirm_stop {
        put(buf, inner.x, y - 1, &Line::from(Span::styled("stop this workflow? agents in flight are cancelled; the run can be resumed \u{b7} y / n", ink(Ink::Amber, theme))), inner.width);
    }

    // The log takes the bottom; the agents take the rest.
    let log_rows = 6usize.min(info.log.len() + 1).max(2);
    let agent_room = (inner.bottom().saturating_sub(y) as usize).saturating_sub(log_rows + 1);
    let ordered = ordered_agents(info);
    let mut rows: Vec<AgentRow> = Vec::new();
    let mut last_phase: Option<Option<&str>> = None;
    for (n, a) in ordered.iter().enumerate() {
        if last_phase != Some(a.phase.as_deref()) {
            rows.push(AgentRow::Phase(a.phase.as_deref().unwrap_or("other")));
            last_phase = Some(a.phase.as_deref());
        }
        rows.push(AgentRow::Agent(n, a));
    }
    if rows.is_empty() {
        note(inner, y, "no agent has started yet", theme, buf);
    }
    // Keep the selected agent on screen.
    let sel_row = rows.iter().position(|r| matches!(r, AgentRow::Agent(n, _) if *n == ui.agent)).unwrap_or(0);
    let first = first_row(sel_row, agent_room.max(1));
    for (k, r) in rows.iter().enumerate().skip(first).take(agent_room) {
        let ry = y + (k - first) as u16;
        match r {
            AgentRow::Phase(p) => {
                put(buf, inner.x, ry, &Line::from(Span::styled(text::clean(p), ink(Ink::Accent, theme))), inner.width);
            }
            AgentRow::Agent(n, a) => {
                let sel = *n == ui.agent;
                let (g, gi) = agent_glyph(a.status);
                let spans = vec![
                    marker(sel, state, theme, *n),
                    Span::styled(format!("{g} "), ink(gi, theme)),
                    Span::styled(format!("#{:<3}", a.index), dimmer),
                    Span::styled(text::pad(&text::truncate(&text::clean(&a.label), 44), 45), if sel { theme.bright() } else { theme.normal() }),
                    Span::styled(text::pad(agent_word(a.status), 9), ink(gi, theme)),
                    Span::styled(text::pad(&format!("{} tok", text::compact(a.tokens)), 11), dim),
                    Span::styled(text::took(a.elapsed_ms), dimmer),
                ];
                put_row(state, buf, (inner.x, ry, inner.width), Line::from(spans), Target::Row(*n), theme);
            }
        }
    }
    // log() lines, oldest first, the newest at the bottom.
    let ly = inner.bottom().saturating_sub(log_rows as u16);
    put(buf, inner.x, ly, &Line::from(Span::styled("log", ink(Ink::Accent, theme))), inner.width);
    let take = log_rows - 1;
    let start = info.log.len().saturating_sub(take);
    for (i, l) in info.log[start..].iter().enumerate() {
        put(buf, inner.x + 2, ly + 1 + i as u16, &Line::from(Span::styled(text::truncate(&text::clean(l), inner.width as usize - 2), dim)), inner.width.saturating_sub(2));
    }
    if info.log.is_empty() {
        put(buf, inner.x + 2, ly + 1, &Line::from(Span::styled("nothing logged yet", dimmer)), inner.width.saturating_sub(2));
    }
}

enum AgentRow<'a> {
    Phase(&'a str),
    Agent(usize, &'a &'a WorkflowAgentInfo),
}

// ---- /findings --------------------------------------------------------------------------------

pub fn findings(area: Rect, state: &State, ui: &FindingsUi, theme: &Theme, buf: &mut Buffer) {
    let Some(set) = state.bg.findings.get(ui.set) else {
        let inner = frame(area, 90, 8, "findings", &[("esc", "close")], theme, buf);
        note(inner, inner.y, "no findings yet \u{b7} /code-review looks for them", theme, buf);
        return;
    };
    let rows = sorted_findings(set);
    let title = format!("findings \u{b7} {}", crate::bg::findings_summary(&set.findings));
    let h = area.height.saturating_sub(2).min(rows.len() as u16 + 18).max(16);
    let mut hints = vec![("f", "fix it"), ("o", "open file"), ("\u{2191}\u{2193}", "select")];
    if state.bg.findings.len() > 1 {
        hints.push(("tab", "other report"));
    }
    hints.push(("esc", "close"));
    let inner = frame(area, 124, h, &title, &hints, theme, buf);
    let detail_h = 9u16.min(inner.height / 2);
    let list_h = inner.height.saturating_sub(detail_h + 1) as usize;
    let first = first_row(ui.sel, list_h);
    for (i, f) in rows.iter().enumerate().skip(first).take(list_h) {
        let sel = i == ui.sel;
        let at = match f.line {
            Some(l) => format!("{}:{l}", f.file),
            None => f.file.clone(),
        };
        let room = (inner.width as usize).saturating_sub(2 + 10 + 14 + 34);
        let spans = vec![
            marker(sel, state, theme, i),
            Span::styled(text::pad(severity_word(f.severity), 9), ink(severity_ink(f.severity), theme)),
            Span::styled(text::pad(&text::truncate(&text::clean(&f.lens), 13), 14), ink(Ink::Dim, theme)),
            Span::styled(text::pad(&text::truncate_left(&text::clean(&at), 33), 34), ink(Ink::Speech, theme)),
            Span::styled(text::truncate(&text::clean(&f.claim), room), if sel { theme.bright() } else { theme.normal() }),
        ];
        put_row(state, buf, (inner.x, inner.y + (i - first) as u16, inner.width), Line::from(spans), Target::Row(i), theme);
    }
    // The selected finding in full.
    let dy = inner.bottom().saturating_sub(detail_h);
    let Some(f) = rows.get(ui.sel) else { return };
    let mut y = dy;
    let w = inner.width as usize;
    let mut block = |label: &str, body: &str, st: Style, y: &mut u16| {
        for (i, l) in text::wrap(&text::clean(body), w.saturating_sub(10)).into_iter().enumerate() {
            if *y >= inner.bottom() {
                return;
            }
            let lead = if i == 0 { text::pad(label, 10) } else { " ".repeat(10) };
            put(buf, inner.x, *y, &Line::from(vec![Span::styled(lead, ink(Ink::Dimmer, theme)), Span::styled(l, st)]), inner.width);
            *y += 1;
        }
    };
    block("claim", &f.claim, theme.bright(), &mut y);
    if !f.evidence.trim().is_empty() {
        block("evidence", &f.evidence, ink(Ink::Dim, theme), &mut y);
    }
    if let Some(fix) = f.fix.as_deref().filter(|x| !x.trim().is_empty()) {
        block("fix", fix, theme.normal(), &mut y);
    }
}

// ---- /files -----------------------------------------------------------------------------------

pub fn files(area: Rect, state: &State, sel: usize, theme: &Theme, buf: &mut Buffer) {
    let files = state.shown_files();
    let h = (files.len() as u16 * 2 + 6).clamp(9, area.height.saturating_sub(2));
    let inner = frame(area, 108, h, "files", &[("\u{21b5}", "open"), ("r", "show in Explorer"), ("\u{2191}\u{2193}", "select"), ("esc", "close")], theme, buf);
    if files.is_empty() {
        note(inner, inner.y, "nothing sent yet \u{b7} the agent sends a file with send_file and it appears here", theme, buf);
        return;
    }
    let visible = (inner.height as usize) / 2;
    let first = first_row(sel, visible.max(1));
    for (i, f) in files.iter().enumerate().skip(first).take(visible) {
        let s = i == sel;
        let y = inner.y + (i - first) as u16 * 2;
        let spans = vec![
            marker(s, state, theme, i),
            Span::styled("\u{25aa} ", ink(Ink::Accent, theme)),
            Span::styled(text::pad(&text::truncate(&text::clean(&f.name), 36), 37), if s { theme.bright() } else { theme.normal() }),
            Span::styled(text::pad(&text::bytes(f.bytes), 10), ink(Ink::Dim, theme)),
            Span::styled(text::truncate(&text::clean(&f.note), (inner.width as usize).saturating_sub(52)), ink(Ink::Dim, theme)),
        ];
        put_row(state, buf, (inner.x, y, inner.width), Line::from(spans), Target::Row(i), theme);
        put(buf, inner.x + 4, y + 1, &Line::from(Span::styled(text::truncate_left(&text::clean(&f.path), inner.width as usize - 4), ink(Ink::Dimmer, theme))), inner.width.saturating_sub(4));
    }
}

/// The conversation's card for a file the agent sent.
pub fn file_lines(f: &marlowe_proto::SentFile, width: usize, theme: &Theme, out: &mut Vec<Line<'static>>) {
    out.push(Line::from(vec![
        Span::styled("\u{25aa} ", ink(Ink::Accent, theme)),
        Span::styled(text::truncate(&text::clean(&f.name), width.saturating_sub(16).max(8)), theme.bright()),
        Span::styled(format!(" \u{b7} {}", text::bytes(f.bytes)), ink(Ink::Dim, theme)),
    ]));
    if !f.note.trim().is_empty() {
        for l in text::wrap(&text::clean(&f.note), width.saturating_sub(2)) {
            out.push(Line::from(vec![Span::raw("  "), Span::styled(l, ink(Ink::Dim, theme))]));
        }
    }
    out.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(text::truncate_left(&text::clean(&f.path), width.saturating_sub(24).max(8)), ink(Ink::Dimmer, theme)),
        Span::styled("  /files \u{b7} \u{21b5} open \u{b7} r reveal", ink(Ink::Dimmer, theme)),
    ]));
}

/// The conversation's lines for news from background work: its source, then what it said.
pub fn news_lines(label: &str, body: &str, width: usize, theme: &Theme, out: &mut Vec<Line<'static>>) {
    out.push(Line::from(vec![
        Span::styled("\u{21b3} ", ink(Ink::Dimmer, theme)),
        Span::styled(text::truncate(&text::clean(label), width.saturating_sub(4).max(8)), ink(Ink::Dim, theme)),
    ]));
    // The label already says what a crew run's or another session's header says; every other form
    // (`[job .. exited ..]`, `[wake-up ..: reason]`, `[monitor .. ended ..]`) carries its message on
    // the first line, which is shown with its brackets taken off.
    let first = body.lines().next().unwrap_or("").trim();
    let header_only = first.starts_with("[crew \u{b7} run ") || first.starts_with("[message from session ");
    let head = (!header_only)
        .then(|| first.strip_prefix('[').map_or(first, |f| f.strip_suffix(']').unwrap_or(f)))
        .filter(|h| !h.trim().is_empty());
    let rest: Vec<&str> = head.into_iter().chain(body.lines().skip(1).filter(|l| !l.trim().is_empty())).collect();
    let shown = rest.len().min(NEWS_ROWS);
    for l in rest.iter().take(shown) {
        out.push(Line::from(vec![
            Span::styled(format!("  {} ", chrome::QUOTE_RULE), ink(Ink::Dimmer, theme)),
            Span::styled(text::truncate(&text::clean(l), width.saturating_sub(4)), ink(Ink::Dim, theme)),
        ]));
    }
    if rest.len() > shown {
        out.push(Line::from(Span::styled(format!("  {} more lines \u{b7} ^o shows all", rest.len() - shown), ink(Ink::Dimmer, theme))));
    }
}

/// Rows of a news message the conversation shows.
const NEWS_ROWS: usize = 6;

// ---- the plan file ----------------------------------------------------------------------------

pub fn plan(area: Rect, state: &State, ui: &PlanUi, theme: &Theme, buf: &mut Buffer) -> Option<(u16, u16)> {
    let w = area.width.saturating_sub(8).clamp(60, 104);
    // The plan sets the height, up to the screen.
    let foot = PLAN_CHOICES.len() as u16 + 3;
    let text_rows = markdown::render_prose(&ui.ask.text, (w - 4) as usize, theme, Style::default()).len() as u16;
    let h = (text_rows + foot + 2).clamp(14, area.height.saturating_sub(2));
    let r = centred(area, w, h, false);
    let title = format!("plan \u{b7} {}{}", text::short_path(&text::clean(&ui.ask.path)), if ui.edited { " \u{b7} edited" } else { "" });
    let inner = panel(r, &title, ink(Ink::Amber, theme), true, buf);
    let hints: &[(&str, &str)] = if ui.feedback.is_some() {
        &[("\u{21b5}", "send"), ("esc", "back")]
    } else {
        &[("\u{21b5}", "choose"), ("1-3", "pick"), ("e", "edit the file"), ("j k", "scroll"), ("esc", "no")]
    };
    panel_hints(r, hints, theme, buf);
    let body = Rect { height: inner.height.saturating_sub(foot), ..inner };
    let lines = markdown::render_prose(&ui.ask.text, body.width as usize, theme, Style::default().fg(theme.speech()));
    let rows = body.height as usize;
    let max = lines.len().saturating_sub(rows);
    ui.scroll.max.set(max);
    let top = ui.scroll.at();
    for (i, l) in lines.iter().skip(top).take(rows).enumerate() {
        put(buf, body.x, body.y + i as u16, l, body.width);
    }
    if lines.len() > rows {
        let tag = format!(" {}\u{2013}{} of {} ", top + 1, (top + rows).min(lines.len()), lines.len());
        let tw = text::width(&tag) as u16;
        put(buf, r.right().saturating_sub(tw + 2), r.y, &Line::from(Span::styled(tag, ink(Ink::Dimmer, theme))), tw);
    }
    // The way out: three choices.
    let y0 = inner.bottom().saturating_sub(foot - 1);
    put(buf, inner.x, y0 - 1, &Line::from(Span::styled("Marlowe wrote this plan and is waiting. Plan mode stays on until you approve it.", ink(Ink::Dim, theme))), inner.width);
    for (i, (key, label)) in PLAN_CHOICES.iter().enumerate() {
        let on = i == ui.choice;
        let over = super::hovered(state, &Target::Button(i));
        let style = match (on, over) {
            (true, _) => ink(Ink::Accent, theme).add_modifier(Modifier::BOLD),
            (false, true) => theme.bright(),
            (false, false) => theme.normal(),
        };
        let y = y0 + i as u16;
        let line = Line::from(vec![row_marker(on, over, theme), Span::styled(format!("{key} "), ink(Ink::Dimmer, theme)), Span::styled(*label, style)]);
        hit(state, Rect { x: inner.x, y, width: inner.width, height: 1 }, Target::Button(i));
        put(buf, inner.x, y, &line, inner.width);
    }
    let mut cursor = None;
    if let Some(f) = &ui.feedback {
        let y = y0 + PLAN_CHOICES.len() as u16;
        let shown = if f.text().is_empty() {
            Span::styled("what should change? (enter sends it back)", ink(Ink::Dimmer, theme))
        } else {
            Span::styled(text::clean(f.text()), theme.bright())
        };
        put(buf, inner.x, y, &Line::from(vec![Span::styled("\u{203a} ", ink(Ink::Accent, theme)), shown]), inner.width);
        cursor = Some((inner.x + 2 + text::width(&text::clean(&f.text()[..f.cursor()])) as u16, y));
    }
    cursor
}

// ---- /bench -----------------------------------------------------------------------------------

pub fn bench(area: Rect, state: &State, ui: &BenchUi, theme: &Theme, buf: &mut Buffer) {
    if ui.reading {
        let h = area.height.saturating_sub(4);
        let inner = frame(area, 130, h, "bench and eval", &[("\u{2191}\u{2193}", "scroll"), ("esc", "back")], theme, buf);
        let list = state.bg.reports.as_deref().unwrap_or(&[]);
        let Some(file) = list.get(ui.sel) else { return };
        put(buf, inner.x, inner.y, &Line::from(vec![Span::styled(crate::bg_panels::report_kind_word(file.kind), ink(Ink::Accent, theme)), Span::styled(format!(" \u{b7} {}", text::clean(&file.name)), theme.normal())]), inner.width);
        let body = Rect { y: inner.y + 2, height: inner.height.saturating_sub(2), ..inner };
        let lines: Vec<Line<'static>> = match &state.bg.report_text {
            Some((p, Ok(t))) if *p == file.path => report::lines(file.kind, t)
                .into_iter()
                .map(|l| {
                    let st = if l.tone == Tone::Head { ink(Ink::Accent, theme).add_modifier(Modifier::BOLD) } else if l.tone == Tone::Body { theme.normal() } else { ink(tone_ink(l.tone), theme) };
                    Line::from(Span::styled(text::truncate(&text::clean(&l.text), body.width as usize), st))
                })
                .collect(),
            Some((p, Err(e))) if *p == file.path => vec![Line::from(Span::styled(format!("could not read it: {}", text::clean(e)), ink(Ink::Amber, theme)))],
            _ => vec![Line::from(Span::styled("reading\u{2026}", ink(Ink::Dimmer, theme)))],
        };
        let rows = body.height as usize;
        ui.scroll.max.set(lines.len().saturating_sub(rows));
        let top = ui.scroll.at();
        for (i, l) in lines.iter().skip(top).take(rows).enumerate() {
            put(buf, body.x, body.y + i as u16, l, body.width);
        }
        return;
    }
    let n = state.bg.reports.as_ref().map_or(0, Vec::len);
    let inner = frame(area, 108, (n as u16 + 7).clamp(10, area.height.saturating_sub(2)), "bench and eval", &[("\u{21b5}", "read"), ("r", "look again"), ("\u{2191}\u{2193}", "select"), ("esc", "close")], theme, buf);
    let Some(list) = &state.bg.reports else {
        note(inner, inner.y, "looking\u{2026}", theme, buf);
        return;
    };
    note(inner, inner.y, "marlowe bench <model> writes a card; marlowe eval run writes a report", theme, buf);
    if list.is_empty() {
        note(inner, inner.y + 2, "no reports yet \u{b7} run `marlowe bench <model>` or `marlowe eval run --suite micro`", theme, buf);
        return;
    }
    let visible = (inner.height as usize).saturating_sub(2);
    let first = first_row(ui.sel, visible);
    for (i, f) in list.iter().enumerate().skip(first).take(visible) {
        let s = i == ui.sel;
        let spans = vec![
            marker(s, state, theme, i),
            Span::styled(text::pad(crate::bg_panels::report_kind_word(f.kind), 7), ink(Ink::Dim, theme)),
            Span::styled(text::pad(&text::truncate(&text::clean(&f.name), 56), 57), if s { theme.bright() } else { theme.normal() }),
            Span::styled(text::ago(f.modified_ms, state.now_ms), ink(Ink::Dimmer, theme)),
        ];
        put_row(state, buf, (inner.x, inner.y + 2 + (i - first) as u16, inner.width), Line::from(spans), Target::Row(i), theme);
    }
}

// ---- /fewer-permission-prompts ----------------------------------------------------------------

pub fn suggestions(area: Rect, state: &State, ui: &SuggestUi, theme: &Theme, buf: &mut Buffer) {
    let list = state.bg.suggestions.as_deref().unwrap_or(&[]);
    let h = (list.len() as u16 * 2 + 7).clamp(10, area.height.saturating_sub(2));
    let inner = frame(area, 104, h, "fewer permission prompts", &[("space", "pick"), ("a", "all"), ("tab", "layer"), ("\u{21b5}", "add"), ("esc", "close")], theme, buf);
    put(
        buf,
        inner.x,
        inner.y,
        &Line::from(vec![
            Span::styled("allow rules, written to the ", ink(Ink::Dim, theme)),
            Span::styled(crate::panel::scope_label(ui.scope), ink(Ink::Accent, theme).add_modifier(Modifier::BOLD)),
            Span::styled(" settings \u{b7} read-only calls you approved again and again", ink(Ink::Dim, theme)),
        ]),
        inner.width,
    );
    let visible = (inner.height as usize).saturating_sub(2) / 2;
    let first = first_row(ui.sel, visible.max(1));
    for (i, s) in list.iter().enumerate().skip(first).take(visible) {
        let sel = i == ui.sel;
        let y = inner.y + 2 + (i - first) as u16 * 2;
        let picked = ui.picked.get(i).copied().unwrap_or(false);
        let spans = vec![
            marker(sel, state, theme, i),
            Span::styled(if picked { "[x] " } else { "[ ] " }, ink(if picked { Ink::Accent } else { Ink::Dim }, theme)),
            Span::styled(text::pad(&text::truncate(&text::clean(&s.pattern), 44), 45), if sel { theme.bright() } else { theme.normal() }),
            Span::styled(format!("{}\u{d7} approved", s.count), ink(Ink::Dim, theme)),
        ];
        put_row(state, buf, (inner.x, y, inner.width), Line::from(spans), Target::Row(i), theme);
        if !s.example.trim().is_empty() {
            put(buf, inner.x + 6, y + 1, &Line::from(Span::styled(text::truncate(&text::clean(&s.example), inner.width as usize - 6), ink(Ink::Dimmer, theme))), inner.width.saturating_sub(6));
        }
    }
}

// ---- threads ----------------------------------------------------------------------------------

pub fn thread(area: Rect, state: &State, ui: &ThreadUi, theme: &Theme, buf: &mut Buffer) -> Option<(u16, u16)> {
    let hints: &[(&str, &str)] =
        if ui.typing { &[("\u{21b5}", "send"), ("esc", "back")] } else { &[("\u{21b5}", "write"), ("tab", "next thread"), ("\u{2191}\u{2193}", "scroll"), ("esc", "close")] };
    let h = area.height.saturating_sub(4);
    let inner = frame(area, 100, h, "messages", hints, theme, buf);
    let dim = ink(Ink::Dim, theme);
    let target = &ui.target;
    let name = if target.is_empty() {
        "no thread yet".to_string()
    } else if let Some(r) = state.crew.get(&marlowe_proto::RunId(target.clone())) {
        format!("run {} \u{b7} {} \u{b7} {}", text::clean(target).chars().take(8).collect::<String>(), text::truncate(&text::clean(&r.title), 40), crate::crew::status_word(r.status))
    } else {
        text::clean(target)
    };
    let count = state.bg.thread_targets().len();
    let more = if count > 1 { format!("  ({count} threads \u{b7} tab)") } else { String::new() };
    put(buf, inner.x, inner.y, &Line::from(vec![Span::styled(name, ink(Ink::Accent, theme)), Span::styled(more, ink(Ink::Dimmer, theme))]), inner.width);
    let body = Rect { y: inner.y + 2, height: inner.height.saturating_sub(if ui.typing { 5 } else { 3 }), ..inner };
    let msgs = state.bg.thread_to(target);
    let mut lines: Vec<Line<'static>> = Vec::new();
    if msgs.is_empty() {
        lines.push(Line::from(Span::styled("no messages yet \u{b7} the conductor writes with send_message; you write to a running crew run with enter", ink(Ink::Dimmer, theme))));
    }
    for m in msgs {
        let (who, wink) = match m.from {
            ThreadFrom::You => ("you", Ink::Accent),
            ThreadFrom::Conductor => ("conductor", Ink::Speech),
        };
        for (i, l) in text::wrap(&text::clean(&m.text), (body.width as usize).saturating_sub(13)).into_iter().enumerate() {
            let lead = if i == 0 { Span::styled(text::pad(who, 11), ink(wink, theme)) } else { Span::raw(" ".repeat(11)) };
            lines.push(Line::from(vec![lead, Span::styled("\u{203a} ", ink(Ink::Dimmer, theme)), Span::styled(l, theme.normal())]));
        }
        let (word, dink) = match &m.delivery {
            Delivery::Sent => ("sent".to_string(), Ink::Dimmer),
            Delivery::Delivered(s) if s.is_empty() => ("delivered".to_string(), Ink::Green),
            Delivery::Delivered(s) => (format!("delivered \u{b7} {}", text::truncate(&text::clean(s), 60)), Ink::Green),
            Delivery::Refused(s) => (format!("not delivered \u{b7} {}", text::truncate(&text::clean(s), 60)), Ink::Red),
        };
        lines.push(Line::from(vec![Span::raw(" ".repeat(13)), Span::styled(word, ink(dink, theme))]));
    }
    let rows = body.height as usize;
    let max = lines.len().saturating_sub(rows);
    ui.scroll.max.set(max);
    let top = max.saturating_sub(ui.scroll.at());
    for (i, l) in lines.iter().skip(top).take(rows).enumerate() {
        put(buf, body.x, body.y + i as u16, l, body.width);
    }
    let mut cursor = None;
    if ui.typing {
        let y = inner.bottom().saturating_sub(2);
        put(buf, inner.x, y - 1, &Line::from(Span::styled("to the run \u{b7} it reads this at its next step", dim)), inner.width);
        let shown = if ui.input.text().is_empty() { Span::styled("message\u{2026}", ink(Ink::Dimmer, theme)) } else { Span::styled(text::clean(ui.input.text()), theme.bright()) };
        put(buf, inner.x, y, &Line::from(vec![Span::styled("\u{203a} ", ink(Ink::Accent, theme)), shown]), inner.width);
        cursor = Some((inner.x + 2 + text::width(&text::clean(&ui.input.text()[..ui.input.cursor()])) as u16, y));
    }
    cursor
}

// ---- the strip under the conversation ----------------------------------------------------------

/// Rows the task strip takes: its header and up to three tasks; none when nothing is running or
/// the pane is short.
pub fn strip_height(state: &State, r: Rect) -> u16 {
    if r.height < 16 {
        return 0;
    }
    let n = state.bg.strip(state.session.as_ref()).len();
    if n == 0 {
        0
    } else {
        1 + n.min(STRIP_ROWS) as u16
    }
}

/// Tasks the strip lists.
pub const STRIP_ROWS: usize = 3;

pub fn draw_strip(r: Rect, state: &State, theme: &Theme, buf: &mut Buffer, pad: u16) {
    let tasks = state.bg.strip(state.session.as_ref());
    let head = Line::from(vec![
        Span::styled("background ", ink(Ink::Dim, theme)),
        Span::styled(format!("{} running \u{b7} /tasks", tasks.len()), ink(Ink::Dimmer, theme)),
    ]);
    let hw = r.width.saturating_sub(pad);
    hit(state, Rect { x: r.x + pad, y: r.y, width: hw, height: 1 }, Target::Chord(Command::Tasks));
    put(buf, r.x + pad, r.y, &head, hw);
    // The newest are the ones to see; the rest are in the panel.
    let skip = tasks.len().saturating_sub(STRIP_ROWS);
    for (row, t) in tasks.iter().skip(skip).take(STRIP_ROWS).enumerate() {
        let (glyph, gi) = match (t.kind, t.status) {
            (_, TaskStatus::Waiting) => ("\u{25d0}", Ink::Amber),
            (TaskKind::Monitor, _) => ("\u{25c9}", Ink::Green),
            _ => ("\u{25cf}", Ink::Green),
        };
        let time = elapsed(t, state.now_ms);
        let room = hw as usize;
        let tail = format!(" {time}");
        let body = text::truncate(&state.task_line(t), room.saturating_sub(2 + text::width(&tail)));
        let line = Line::from(vec![
            Span::styled(format!("{glyph} "), ink(gi, theme)),
            Span::styled(body.clone(), ink(Ink::Body, theme)),
            Span::raw(" ".repeat(room.saturating_sub(2 + text::width(&body) + text::width(&tail)))),
            Span::styled(tail, ink(Ink::Dimmer, theme)),
        ]);
        let y = r.y + 1 + row as u16;
        hit(state, Rect { x: r.x + pad, y, width: hw, height: 1 }, Target::Chord(Command::Tasks));
        put(buf, r.x + pad, y, &line, hw);
    }
}
