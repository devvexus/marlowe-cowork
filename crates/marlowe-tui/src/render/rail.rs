//! The Situation Board rail: CREW, WATCH, THREADS, TODAY. WATCH shows the Sentinel's runs and
//! recent notices; TODAY the date and quiet hours. Each shows an honest empty state.

use marlowe_proto::{RunStatus, Severity};
use marlowe_surface::chrome::Ink;
use marlowe_surface::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::boot::severity_style;
use super::{ink, put};
use crate::state::State;
use crate::text;
use crate::wake::{clock_minute, notice_glyph, run_detail, run_glyph, Presence};

/// Notices the rail lists under the runs.
const RAIL_NOTICES: usize = 3;

pub fn draw(r: Rect, state: &State, theme: &Theme, buf: &mut Buffer) {
    let x = r.x + 2;
    let w = r.width.saturating_sub(3);
    let crew = vec![dimmer("no runs", theme)];
    let threads = vec![dimmer("no open threads", theme)];
    let today = today_lines(state, w as usize, theme);
    // WATCH takes what the other three leave.
    let fixed = (crew.len() + threads.len() + today.len()) as u16 + 4 * 2;
    let watch_rows = r.height.saturating_sub(fixed).max(1) as usize;
    let (watch, count) = watch_lines(state, w as usize, watch_rows, theme);

    let mut y = r.y;
    for (title, right, body) in
        [("CREW", None, crew), ("WATCH", count, watch), ("THREADS", None, threads), ("TODAY", None, today)]
    {
        if y + 2 > r.bottom() {
            break;
        }
        put(buf, x, y, &Line::from(Span::styled(title, ink(Ink::Accent, theme))), w);
        if let Some(right) = right {
            let rw = text::width(&right) as u16;
            put(buf, x + w.saturating_sub(rw), y, &Line::from(Span::styled(right, ink(Ink::Dimmer, theme))), rw);
        }
        y += 1;
        for l in body {
            if y >= r.bottom() {
                return;
            }
            put(buf, x, y, &l, w);
            y += 1;
        }
        y += 1;
    }
}

fn dimmer(s: &str, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(s.to_string(), ink(Ink::Dimmer, theme)))
}

/// WATCH: runs (two lines each, most urgent first), then unseen notices not about a listed run.
/// Returns the lines and the header's right-hand count.
fn watch_lines(state: &State, w: usize, rows: usize, theme: &Theme) -> (Vec<Line<'static>>, Option<String>) {
    let wake = &state.wake;
    let Some(ws) = &wake.watch else {
        let empty = match wake.sentinel {
            Presence::Absent => "sentinel not running",
            _ => "nothing watched",
        };
        return (vec![dimmer(empty, theme)], None);
    };
    if ws.roots.is_empty() {
        let mut v = vec![dimmer("nothing watched", theme)];
        let offers = wake.open_offers().len();
        if offers > 0 {
            v.push(Line::from(vec![
                Span::styled(format!("{offers} offer{} ", if offers == 1 { "" } else { "s" }), ink(Ink::Dim, theme)),
                Span::styled("· ^w", ink(Ink::Dimmer, theme)),
            ]));
        }
        return (v, None);
    }
    let alive = ws.runs.iter().filter(|r| r.status == RunStatus::Alive).count();
    let count = Some(format!("{alive} alive"));
    let mut out: Vec<Line<'static>> = Vec::new();
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
        out.push(Line::from(vec![Span::styled(format!("{glyph} "), gstyle), Span::styled(name, theme.normal())]));
        let detail = run_detail(run, state.now_ms);
        let dstyle = match run.status {
            RunStatus::Stalled => ink(Ink::Amber, theme),
            RunStatus::Failed => ink(Ink::Red, theme),
            _ => ink(Ink::Dim, theme),
        };
        out.push(Line::from(vec![Span::raw("  "), Span::styled(text::truncate(&detail, w.saturating_sub(2)), dstyle)]));
        listed += 1;
    }
    let hidden = runs.len() - listed;
    if runs.is_empty() {
        out.push(dimmer("no runs found yet", theme));
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
    for n in notices.into_iter().take(RAIL_NOTICES) {
        if out.len() + 1 > rows.saturating_sub(usize::from(hidden > 0)) {
            break;
        }
        out.push(Line::from(vec![
            Span::styled(format!("{} ", notice_glyph(n.severity)), severity_style(n.severity, theme)),
            Span::styled(text::truncate(&text::clean(&n.title), w.saturating_sub(2)), ink(Ink::Dim, theme)),
        ]));
    }
    if hidden > 0 {
        out.push(dimmer(&format!("+{hidden} more · ^w"), theme));
    }
    (out, count)
}

/// TODAY: the date, quiet hours (amber while in them), routines.
fn today_lines(state: &State, w: usize, theme: &Theme) -> Vec<Line<'static>> {
    if state.date.is_empty() && state.wake.watch.is_none() {
        return vec![dimmer("nothing scheduled", theme)];
    }
    let mut v = Vec::new();
    if !state.date.is_empty() {
        v.push(Line::from(Span::styled(text::truncate(&state.date, w), theme.normal())));
    }
    let q = state.wake.quiet();
    let now_quiet = clock_minute(&state.clock).is_some_and(|m| q.contains(m));
    v.push(if now_quiet {
        Line::from(vec![
            Span::styled("quiet now ", ink(Ink::Amber, theme)),
            Span::styled(format!("· {}", q.label()), ink(Ink::Dim, theme)),
        ])
    } else {
        Line::from(vec![Span::styled("quiet ", ink(Ink::Dim, theme)), Span::styled(q.label(), ink(Ink::Dim, theme))])
    });
    v.push(dimmer("no routines", theme));
    v
}
