//! The Watch view (^w): roots and discovery offers (selectable), runs, recent notices, and the
//! machine (GPU, Ollama, voice, quiet hours). Adding a root is always an explicit key press.

use marlowe_proto::{RunStatus, WatchCandidate, WatchRoot};
use marlowe_surface::chrome::Ink;
use marlowe_surface::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use super::boot::severity_style;
use super::{hovered, ink, note_style, put, put_row};
use crate::state::{State, Target};
use crate::text;
use crate::wake::{notice_glyph, run_detail, run_glyph, Presence, WatchMode, WatchRow};

/// Notices listed at most.
const NOTICES_SHOWN: usize = 8;

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
    match &state.wake.mode {
        WatchMode::Add { input } => {
            let before = &input.text()[..input.cursor()];
            let col = text::width(&text::clean(before)).min(c.width as usize - 20) as u16;
            Some((c.x + 18 + col, c.bottom() - 1))
        }
        _ => None,
    }
}

pub fn draw(main: Rect, state: &State, theme: &Theme, buf: &mut Buffer) {
    let c = content(main);
    if c.width < 40 || c.height < 8 {
        return;
    }
    let wake = &state.wake;
    let w = c.width as usize;
    let dim = ink(Ink::Dim, theme);
    let dimmer = ink(Ink::Dimmer, theme);
    let accent = ink(Ink::Accent, theme);
    let mut lines: Vec<Line<'static>> = Vec::new();

    // Heading.
    let mut head = vec![Span::styled("Watch", accent.add_modifier(Modifier::BOLD))];
    if let Some(ws) = &wake.watch {
        let alive = ws.runs.iter().filter(|r| r.status == RunStatus::Alive).count();
        head.push(Span::styled(
            format!(
                "  ·  {} root{} · {} run{} · {alive} alive",
                ws.roots.len(),
                plural(ws.roots.len()),
                ws.runs.len(),
                plural(ws.runs.len())
            ),
            dim,
        ));
    }
    lines.push(Line::from(head));
    lines.push(machine_line(state, w, theme));
    lines.push(Line::from(""));

    if wake.watch.is_none() {
        let msg = match wake.sentinel {
            Presence::Absent => "The Sentinel is not running in this daemon.",
            _ => "asking the Sentinel…",
        };
        lines.push(Line::from(Span::styled(msg, dim)));
    }

    // Roots and offers: the selectable rows.
    let rows = wake.rows();
    let roots: Vec<&WatchRoot> = wake.watch.iter().flat_map(|w| w.roots.iter()).collect();
    if wake.watch.is_some() {
        lines.push(section("ROOTS", theme));
        if roots.is_empty() {
            lines.push(Line::from(Span::styled("  nothing watched yet — a to add a directory", dimmer)));
        }
    }
    let mut offers_header = false;
    // Which lines are selectable rows, by line index.
    let mut targets: Vec<(usize, Target)> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let browse = matches!(wake.mode, WatchMode::Browse);
        let sel = i == wake.sel && browse;
        let marker = if sel {
            Span::styled("› ", accent)
        } else if browse && hovered(state, &Target::Row(i)) {
            Span::styled("› ", ink(Ink::Hover, theme))
        } else {
            Span::raw("  ")
        };
        match row {
            WatchRow::Root(p) => {
                let Some(root) = roots.iter().find(|r| &r.path == p) else { continue };
                targets.push((lines.len(), Target::Row(i)));
                lines.push(root_line(root, marker, sel, w, theme));
            }
            WatchRow::Offer(o) => {
                if !offers_header {
                    offers_header = true;
                    lines.push(Line::from(""));
                    lines.push(section("OFFERS", theme));
                }
                targets.push((lines.len(), Target::Row(i)));
                lines.push(offer_line(o, marker, sel, state.now_ms, w, theme));
            }
        }
    }
    if wake.watch.is_some() && !offers_header {
        let msg = match &wake.candidates {
            None => "  looking for live runs under Projects…",
            Some(_) => "  no new run directories found",
        };
        lines.push(Line::from(""));
        lines.push(section("OFFERS", theme));
        lines.push(Line::from(Span::styled(msg, dimmer)));
    }

    // Runs.
    if let Some(ws) = &wake.watch {
        if !ws.runs.is_empty() {
            lines.push(Line::from(""));
            lines.push(section("RUNS", theme));
            for r in wake.runs() {
                let (glyph, sev) = run_glyph(r.status);
                let gstyle = match r.status {
                    RunStatus::Alive => ink(Ink::Green, theme),
                    RunStatus::Ended => dim,
                    _ => severity_style(sev, theme),
                };
                let name = text::pad(&text::clean(&r.name), 34);
                let status = format!("{:<9}", status_word(r.status));
                let room = w.saturating_sub(4 + 34 + 9 + 2);
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("{glyph} "), gstyle),
                    Span::styled(name, theme.normal()),
                    Span::styled(status, gstyle),
                    Span::styled(text::truncate(&run_detail(r, state.now_ms), room), dim),
                ]));
            }
        }
    }

    // Notices.
    if !wake.notices.is_empty() {
        lines.push(Line::from(""));
        lines.push(section("NOTICES", theme));
        for n in wake.notices.iter().take(NOTICES_SHOWN) {
            let when = text::ago(n.at_ms, state.now_ms);
            let title_w = w.saturating_sub(4 + 12);
            let title_style = if n.seen { dim } else { theme.normal() };
            let mut spans = vec![
                Span::raw("  "),
                Span::styled(format!("{} ", notice_glyph(n.severity)), severity_style(n.severity, theme)),
                Span::styled(text::pad(&text::clean(&n.title), title_w.min(40)), title_style),
            ];
            let detail = text::clean(n.detail.lines().next().unwrap_or("").trim());
            let room = w.saturating_sub(4 + title_w.min(40) + 12);
            if !detail.is_empty() && room > 8 {
                spans.push(Span::styled(text::pad(&text::truncate(&detail, room - 2), room), dim));
            }
            spans.push(Span::styled(when, dimmer));
            lines.push(Line::from(spans));
        }
    }

    // The prompt, when adding or confirming, takes the last two rows.
    let prompt = !matches!(wake.mode, WatchMode::Browse);
    let body_h = c.height.saturating_sub(if prompt { 2 } else { 0 }) as usize;
    let browse = matches!(wake.mode, WatchMode::Browse);
    for (i, l) in lines.into_iter().take(body_h).enumerate() {
        let y = c.y + i as u16;
        match targets.iter().find(|(at, _)| *at == i) {
            Some((_, t)) if browse => put_row(state, buf, (c.x, y, c.width), l, t.clone(), theme),
            _ => put(buf, c.x, y, &l, c.width),
        }
    }
    match &wake.mode {
        WatchMode::Add { input } => {
            let l = Line::from(vec![
                Span::styled("watch directory › ", accent),
                Span::styled(text::clean(input.text()), theme.normal()),
            ]);
            put(buf, c.x, c.bottom() - 1, &l, c.width);
        }
        WatchMode::ConfirmRemove { path } => {
            let l = Line::from(vec![
                Span::styled("stop watching ", ink(Ink::Amber, theme)),
                Span::styled(text::truncate(&text::short_path(&text::clean(path)), w.saturating_sub(40)), theme.bright()),
                Span::styled("?  nothing on disk is touched  ", dim),
                Span::styled("y", accent),
                Span::styled(" / ", dimmer),
                Span::styled("n", accent),
            ]);
            put(buf, c.x, c.bottom() - 1, &l, c.width);
        }
        WatchMode::Browse => {}
    }

    // Footer hints, then the flash.
    let hints: &[(&str, &str)] = match &wake.mode {
        WatchMode::Browse => {
            &[("↵", "watch offer"), ("a", "add"), ("x", "remove"), ("r", "rescan"), ("↑↓", "select"), ("esc/⌫", "back")]
        }
        WatchMode::Add { .. } => &[("↵", "watch"), ("esc", "cancel")],
        WatchMode::ConfirmRemove { .. } => &[("y", "remove"), ("n", "keep")],
    };
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

fn section(title: &'static str, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(title, ink(Ink::Accent, theme)))
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn status_word(s: RunStatus) -> &'static str {
    match s {
        RunStatus::Alive => "alive",
        RunStatus::Stalled => "stalled",
        RunStatus::Finished => "finished",
        RunStatus::Failed => "failed",
        RunStatus::Ended => "ended",
    }
}

/// `› ~/Projects/X   3 runs · main · 2 dirty · fix the gate`.
fn root_line(r: &WatchRoot, marker: Span<'static>, sel: bool, w: usize, theme: &Theme) -> Line<'static> {
    let dim = ink(Ink::Dim, theme);
    let path_w = (w / 2).clamp(20, 56);
    let path = text::pad(&text::short_path(&text::clean(&r.path)), path_w);
    let mut spans =
        vec![marker, Span::styled(path, if sel { theme.bright() } else { theme.normal() })];
    if let Some(p) = &r.problem {
        spans.push(Span::styled(text::truncate(&text::clean(p), w.saturating_sub(path_w + 4)), ink(Ink::Red, theme)));
        return Line::from(spans);
    }
    let mut parts = vec![format!("{} run{}", r.runs, plural(r.runs as usize))];
    if let Some(g) = &r.git {
        parts.push(text::clean(&g.branch));
        if g.dirty > 0 {
            parts.push(format!("{} dirty", g.dirty));
        }
        if let Some(s) = &g.subject {
            parts.push(text::clean(s));
        }
    }
    let room = w.saturating_sub(path_w + 4);
    spans.push(Span::styled(text::truncate(&parts.join(" · "), room), dim));
    Line::from(spans)
}

/// `  ~/Projects/PruningResearch/valiance   heartbeat.json, 3 logs · 1 live run · 2h ago`.
fn offer_line(o: &WatchCandidate, marker: Span<'static>, sel: bool, now_ms: i64, w: usize, theme: &Theme) -> Line<'static> {
    let path_w = (w / 2).clamp(20, 56);
    let path = text::pad(&text::short_path(&text::clean(&o.path)), path_w);
    let mut spans = vec![marker, Span::styled(path, if sel { theme.bright() } else { theme.normal() })];
    let mut room = w.saturating_sub(path_w + 4);
    if o.live_runs > 0 {
        let s = format!("{} live run{} · ", o.live_runs, plural(o.live_runs as usize));
        room = room.saturating_sub(text::width(&s));
        spans.push(Span::styled(s, ink(Ink::Green, theme)));
    }
    let tail = format!("{} · {}", text::clean(&o.reason), text::ago(o.modified_ms, now_ms));
    spans.push(Span::styled(text::truncate(&tail, room), ink(Ink::Dim, theme)));
    Line::from(spans)
}

/// `quiet 23:00–07:00 · gpu RTX 4090 87% · 20.1/24.0 GB · 71°C · ollama 14 models · voice George`.
fn machine_line(state: &State, w: usize, theme: &Theme) -> Line<'static> {
    let wake = &state.wake;
    let mut parts = vec![format!("quiet {}", wake.quiet().label())];
    if let Some(ws) = &wake.watch {
        match (ws.gpu.devices.first(), &ws.gpu.unavailable) {
            (Some(d), _) => {
                let mut g = format!(
                    "gpu {} {}% · {:.1}/{:.1} GB",
                    text::clean(&d.name),
                    d.util_pct,
                    d.mem_used_mb as f64 / 1024.0,
                    d.mem_total_mb as f64 / 1024.0
                );
                if let Some(t) = d.temp_c {
                    g.push_str(&format!(" · {t}°C"));
                }
                parts.push(g);
            }
            (None, Some(_)) => parts.push("gpu unavailable".into()),
            (None, None) => {}
        }
        parts.push(if ws.ollama.reachable {
            format!("ollama {} model{}", ws.ollama.models, plural(ws.ollama.models as usize))
        } else {
            "ollama not running".into()
        });
    }
    match (&wake.voice, wake.voice_presence) {
        (Some(v), _) if v.available && v.enabled => {
            parts.push(format!("voice {}", v.voice.as_deref().map(text::clean).unwrap_or_else(|| "on".into())))
        }
        (Some(v), _) if v.available => parts.push("voice off".into()),
        (Some(_), _) | (None, Presence::Absent) => parts.push("no voice".into()),
        (None, _) => {}
    }
    Line::from(Span::styled(text::truncate(&parts.join(" · "), w), ink(Ink::Dimmer, theme)))
}
